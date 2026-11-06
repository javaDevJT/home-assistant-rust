//! SQLite persistence for the Home Assistant runtime.
//!
//! The store is synchronous. Async callers should run its methods inside
//! `tokio::task::spawn_blocking`.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hass_core::{
    Error, Event, EventHistoryQuery, HistoryQuery, HistorySeries, MAX_HISTORY_QUERY_RESULTS,
    PurgeResult, Result, State, Store,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
};
use serde_json::Value;

const SCHEMA_VERSION: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A synchronous SQLite implementation of [`hass_core::Store`].
///
/// Disk databases use WAL mode and a bounded busy timeout. All related state,
/// history, and event writes are committed in one transaction.
pub struct SqliteStore {
    connection: Mutex<Connection>,
    _writer_lock: Option<File>,
    database_path: Option<PathBuf>,
}

impl SqliteStore {
    /// Opens a disk-backed SQLite database, creating and migrating an empty
    /// database to the current schema when needed.
    ///
    /// Databases with an unknown schema version or unversioned user objects
    /// are rejected rather than modified.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(Error::InvalidInput(
                "use a disk path or SqliteStore::in_memory".into(),
            ));
        }
        let mut connection =
            Connection::open(path).map_err(|error| storage_error("open SQLite database", error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| storage_error("set SQLite busy timeout", error))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|error| storage_error("enable SQLite foreign keys", error))?;
        check_schema_compatibility(&connection)?;

        // Keep one authoritative writer even when two processes use different
        // ports or symlink spellings for the same database path.
        let canonical = path
            .canonicalize()
            .map_err(|error| storage_error("resolve SQLite database path", error))?;
        protect_database_files(&canonical)?;
        let mut lock_path = canonical.as_os_str().to_os_string();
        lock_path.push(".lock");
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let writer_lock = lock_options
            .open(Path::new(&lock_path))
            .map_err(|error| storage_error("open database writer lock", error))?;
        protect_private_file(Path::new(&lock_path), false)?;
        writer_lock.try_lock().map_err(|error| {
            storage_error("database already has a writer or cannot be locked", error)
        })?;

        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(|error| storage_error("enable SQLite WAL mode", error))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Storage(
                "SQLite did not enable WAL mode for the disk database".to_owned(),
            ));
        }
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| storage_error("configure SQLite synchronous mode", error))?;

        initialize_schema(&mut connection)?;
        protect_database_files(&canonical)?;
        Ok(Self {
            connection: Mutex::new(connection),
            _writer_lock: Some(writer_lock),
            database_path: Some(canonical),
        })
    }

    /// Creates a private in-memory SQLite database with the current schema.
    pub fn in_memory() -> Result<Self> {
        let mut connection = Connection::open_in_memory()
            .map_err(|error| storage_error("open in-memory SQLite database", error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| storage_error("set SQLite busy timeout", error))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|error| storage_error("enable SQLite foreign keys", error))?;
        check_schema_compatibility(&connection)?;
        initialize_schema(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            _writer_lock: None,
            database_path: None,
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| Error::Storage("SQLite connection lock was poisoned".to_owned()))
    }
}

// Authentication keys, TOTP secrets and broker credentials share this owned
// store. Restrict files only after schema preflight; never chmod a foreign DB.
#[cfg(unix)]
fn protect_private_file(path: &Path, optional: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(storage_error("inspect private store file", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Storage(
            "private store file must be a regular file".into(),
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| storage_error("restrict private store file", error))
}

#[cfg(not(unix))]
fn protect_private_file(_path: &Path, _optional: bool) -> Result<()> {
    Ok(())
}

fn protect_database_files(path: &Path) -> Result<()> {
    protect_private_file(path, false)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        protect_private_file(Path::new(&sidecar), true)?;
    }
    Ok(())
}

impl Store for SqliteStore {
    fn load_states(&self) -> Result<Vec<State>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT entity_id, state_json FROM states ORDER BY entity_id ASC")
            .map_err(|error| storage_error("prepare current-state query", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| storage_error("query current states", error))?;

        let mut states = Vec::new();
        for row in rows {
            let (entity_id, encoded) =
                row.map_err(|error| storage_error("read current state", error))?;
            let state = decode_state(&encoded, "current state")?;
            if state.entity_id != entity_id {
                return Err(Error::Storage(
                    "current state JSON disagrees with its entity key".into(),
                ));
            }
            states.push(state);
        }
        Ok(states)
    }

    fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
        validate_state(state)?;
        let state_json = encode_state(state)?;
        let event_record = event.map(encode_event).transpose()?;
        let (seconds, nanoseconds) = timestamp_parts(&state.last_updated);

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("begin state transaction", error))?;
        transaction
            .execute(
                "INSERT INTO states(entity_id, state_json) VALUES (?1, ?2) \
                 ON CONFLICT(entity_id) DO UPDATE SET state_json = excluded.state_json",
                params![state.entity_id, state_json],
            )
            .map_err(|error| storage_error("persist current state", error))?;

        if let (Some(event), Some(event_json)) = (event, event_record.as_deref()) {
            transaction
                .execute(
                    "INSERT INTO history(entity_id, time_seconds, time_nanoseconds, state_json) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![state.entity_id, seconds, nanoseconds, state_json],
                )
                .map_err(|error| storage_error("append state history", error))?;
            insert_event(&transaction, event, event_json)?;
        }

        transaction
            .commit()
            .map_err(|error| storage_error("commit state transaction", error))
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
        validate_entity_id(entity_id)?;
        let event_json = encode_event(event)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("begin state-removal transaction", error))?;
        transaction
            .execute("DELETE FROM states WHERE entity_id = ?1", [entity_id])
            .map_err(|error| storage_error("remove current state", error))?;
        insert_event(&transaction, event, &event_json)?;
        transaction
            .commit()
            .map_err(|error| storage_error("commit state-removal transaction", error))
    }

    fn append_event(&self, event: &Event) -> Result<()> {
        let event_json = encode_event(event)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("begin event transaction", error))?;
        insert_event(&transaction, event, &event_json)?;
        transaction
            .commit()
            .map_err(|error| storage_error("commit event transaction", error))
    }

    fn get_data(&self, key: &str) -> Result<Option<Value>> {
        let connection = self.connection()?;
        let encoded = connection
            .query_row(
                "SELECT value_json FROM metadata WHERE key = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("read metadata", error))?;
        encoded
            .map(|encoded| {
                serde_json::from_str(&encoded)
                    .map_err(|error| Error::Storage(format!("corrupt metadata JSON: {error}")))
            })
            .transpose()
    }

    fn set_data(&self, key: &str, value: &Value) -> Result<()> {
        let encoded = serde_json::to_string(value).map_err(|error| {
            Error::InvalidInput(format!("metadata cannot be represented as JSON: {error}"))
        })?;
        self.connection()?
            .execute(
                "INSERT INTO metadata(key, value_json) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                params![key, encoded],
            )
            .map_err(|error| storage_error("persist metadata", error))?;
        Ok(())
    }

    fn delete_data(&self, key: &str) -> Result<bool> {
        let deleted = self
            .connection()?
            .execute("DELETE FROM metadata WHERE key = ?1", [key])
            .map_err(|error| storage_error("delete metadata", error))?;
        Ok(deleted != 0)
    }

    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>> {
        validate_entity_id(entity_id)?;
        if start > end {
            return Err(Error::InvalidInput(
                "history start must be at or before end".to_owned(),
            ));
        }
        let (start_seconds, start_nanoseconds) = timestamp_parts(&start);
        let (end_seconds, end_nanoseconds) = timestamp_parts(&end);

        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT state_json FROM history \
                 WHERE entity_id = ?1 \
                   AND (time_seconds > ?2 OR (time_seconds = ?2 AND time_nanoseconds >= ?3)) \
                   AND (time_seconds < ?4 OR (time_seconds = ?4 AND time_nanoseconds <= ?5)) \
                 ORDER BY time_seconds ASC, time_nanoseconds ASC, id ASC",
            )
            .map_err(|error| storage_error("prepare history query", error))?;
        let rows = statement
            .query_map(
                params![
                    entity_id,
                    start_seconds,
                    start_nanoseconds,
                    end_seconds,
                    end_nanoseconds
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| storage_error("query state history", error))?;

        let mut states = Vec::new();
        for row in rows {
            let encoded = row.map_err(|error| storage_error("read history state", error))?;
            let state = decode_state(&encoded, "history state")?;
            if state.entity_id != entity_id {
                return Err(Error::Storage(
                    "history state JSON disagrees with its entity key".into(),
                ));
            }
            states.push(state);
        }
        Ok(states)
    }

    fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
        query.validate()?;
        if query.entity_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let (start_seconds, start_nanoseconds) = timestamp_parts(&query.start);
        let (end_seconds, end_nanoseconds) = timestamp_parts(&query.end);
        let mut values = Vec::new();
        let mut sql = String::from(
            "SELECT entity_id, time_seconds, time_nanoseconds, id, state_json, is_boundary \
             FROM (SELECT h.entity_id, h.time_seconds, h.time_nanoseconds, h.id, h.state_json, 0 AS is_boundary \
                   FROM history h \
             WHERE (h.time_seconds > ? OR (h.time_seconds = ? AND h.time_nanoseconds > ?)) \
                     AND (h.time_seconds < ? OR (h.time_seconds = ? AND h.time_nanoseconds < ?))",
        );
        values.extend([
            SqlValue::Integer(start_seconds),
            SqlValue::Integer(start_seconds),
            SqlValue::Integer(start_nanoseconds),
            SqlValue::Integer(end_seconds),
            SqlValue::Integer(end_seconds),
            SqlValue::Integer(end_nanoseconds),
        ]);
        append_history_entity_filters(&mut sql, "h", query, &mut values);
        if query.include_start_time_state {
            sql.push_str(
                " UNION ALL SELECT h.entity_id, h.time_seconds, h.time_nanoseconds, h.id, h.state_json, 1 AS is_boundary \
                   FROM history h \
                   WHERE (h.time_seconds < ? OR (h.time_seconds = ? AND h.time_nanoseconds <= ?))",
            );
            values.extend([
                SqlValue::Integer(start_seconds),
                SqlValue::Integer(start_seconds),
                SqlValue::Integer(start_nanoseconds),
            ]);
            append_history_entity_filters(&mut sql, "h", query, &mut values);
            sql.push_str(
                " AND h.id = (SELECT p.id FROM history p \
                         WHERE p.entity_id = h.entity_id \
                           AND (p.time_seconds < ? OR (p.time_seconds = ? AND p.time_nanoseconds <= ?)) \
                         ORDER BY p.time_seconds DESC, p.time_nanoseconds DESC, p.id DESC LIMIT 1)",
            );
            values.extend([
                SqlValue::Integer(start_seconds),
                SqlValue::Integer(start_seconds),
                SqlValue::Integer(start_nanoseconds),
            ]);
        }
        sql.push_str(") ORDER BY entity_id, time_seconds, time_nanoseconds, id LIMIT ?");
        values.push(SqlValue::Integer((MAX_HISTORY_QUERY_RESULTS + 1) as i64));

        let connection = self.connection()?;
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("prepare multi-entity history query", error))?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .map_err(|error| storage_error("query multi-entity history", error))?;

        let mut by_entity = std::collections::BTreeMap::<String, Vec<State>>::new();
        let mut rows_seen = 0usize;
        for row in rows {
            rows_seen += 1;
            if rows_seen > MAX_HISTORY_QUERY_RESULTS {
                return Err(Error::InvalidInput(format!(
                    "history query exceeds the {MAX_HISTORY_QUERY_RESULTS} row limit"
                )));
            }
            let (entity_id, seconds, nanoseconds, encoded, is_boundary) =
                row.map_err(|error| storage_error("read multi-entity history row", error))?;
            let state = decode_state(&encoded, "history state")?;
            if state.entity_id != entity_id {
                return Err(Error::Storage(
                    "history state JSON disagrees with its entity key".into(),
                ));
            }
            if timestamp_parts(&state.last_updated) != (seconds, nanoseconds) {
                return Err(Error::Storage(
                    "history state timestamp disagrees with its index key".into(),
                ));
            }
            let mut state = state;
            if is_boundary {
                state.last_changed = query.start;
                state.last_updated = query.start;
                state.last_reported = query.start;
            } else if query.significant_changes_only && !is_significant_history_state(&state) {
                continue;
            }
            by_entity.entry(entity_id).or_default().push(state);
        }

        if let Some(entity_ids) = &query.entity_ids {
            let excluded: std::collections::HashSet<&str> =
                query.exclude_entities.iter().map(String::as_str).collect();
            let mut seen = std::collections::HashSet::new();
            Ok(entity_ids
                .iter()
                .filter(|entity_id| seen.insert(entity_id.as_str()))
                .map(|entity_id| HistorySeries {
                    entity_id: entity_id.clone(),
                    states: if excluded.contains(entity_id.as_str()) {
                        Vec::new()
                    } else {
                        by_entity.remove(entity_id).unwrap_or_default()
                    },
                })
                .collect())
        } else {
            Ok(by_entity
                .into_iter()
                .filter(|(entity_id, _)| {
                    !query
                        .exclude_entities
                        .iter()
                        .any(|excluded| excluded == entity_id)
                })
                .map(|(entity_id, states)| HistorySeries { entity_id, states })
                .collect())
        }
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
        query.validate()?;
        if query.event_types.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let (start_seconds, start_nanoseconds) = timestamp_parts(&query.start);
        let (end_seconds, end_nanoseconds) = timestamp_parts(&query.end);
        let mut values = vec![
            SqlValue::Integer(start_seconds),
            SqlValue::Integer(start_seconds),
            SqlValue::Integer(start_nanoseconds),
            SqlValue::Integer(end_seconds),
            SqlValue::Integer(end_seconds),
            SqlValue::Integer(end_nanoseconds),
        ];
        let mut sql = String::from(
            "SELECT event_type, time_seconds, time_nanoseconds, event_json FROM events \
             WHERE (time_seconds > ? OR (time_seconds = ? AND time_nanoseconds > ?)) \
               AND (time_seconds < ? OR (time_seconds = ? AND time_nanoseconds < ?))",
        );
        if let Some(event_types) = &query.event_types {
            let placeholders = std::iter::repeat_n("?", event_types.len())
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" AND event_type IN ({placeholders})"));
            values.extend(event_types.iter().cloned().map(SqlValue::Text));
        }
        sql.push_str(" ORDER BY time_seconds, time_nanoseconds, id LIMIT ?");
        values.push(SqlValue::Integer((MAX_HISTORY_QUERY_RESULTS + 1) as i64));

        let connection = self.connection()?;
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("prepare event history query", error))?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| storage_error("query event history", error))?;

        let mut events = Vec::new();
        for row in rows {
            if events.len() == MAX_HISTORY_QUERY_RESULTS {
                return Err(Error::InvalidInput(format!(
                    "event history query exceeds the {MAX_HISTORY_QUERY_RESULTS} row limit"
                )));
            }
            let (event_type, seconds, nanoseconds, encoded) =
                row.map_err(|error| storage_error("read event history row", error))?;
            let event = decode_event(&encoded)?;
            if event.event_type != event_type {
                return Err(Error::Storage(
                    "event JSON disagrees with its event-type key".into(),
                ));
            }
            if timestamp_parts(&event.time_fired) != (seconds, nanoseconds) {
                return Err(Error::Storage(
                    "event JSON timestamp disagrees with its index key".into(),
                ));
            }
            events.push(event);
        }
        Ok(events)
    }

    fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult> {
        let (seconds, nanoseconds) = timestamp_parts(&cutoff);
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage_error("begin retention purge", error))?;
        let states_deleted = transaction
            .execute(
                "DELETE FROM history WHERE time_seconds < ?1 \
                 OR (time_seconds = ?1 AND time_nanoseconds < ?2)",
                params![seconds, nanoseconds],
            )
            .map_err(|error| storage_error("purge historical states", error))?;
        let events_deleted = transaction
            .execute(
                "DELETE FROM events WHERE time_seconds < ?1 \
                 OR (time_seconds = ?1 AND time_nanoseconds < ?2)",
                params![seconds, nanoseconds],
            )
            .map_err(|error| storage_error("purge historical events", error))?;
        transaction
            .commit()
            .map_err(|error| storage_error("commit retention purge", error))?;
        Ok(PurgeResult {
            states_deleted,
            events_deleted,
        })
    }

    fn repack(&self) -> Result<()> {
        let connection = self.connection()?;
        if self.database_path.is_some() {
            let (busy, _, _): (i64, i64, i64) = connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(|error| storage_error("checkpoint WAL before repack", error))?;
            if busy != 0 {
                return Err(Error::Storage(
                    "SQLite WAL checkpoint was busy during repack".into(),
                ));
            }
        }
        connection
            .execute_batch("PRAGMA optimize; VACUUM; PRAGMA optimize;")
            .map_err(|error| storage_error("repack SQLite database", error))?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| storage_error("restore SQLite full durability", error))?;
        if let Some(path) = &self.database_path {
            protect_database_files(path)?;
        }
        Ok(())
    }
}

fn append_history_entity_filters(
    sql: &mut String,
    alias: &str,
    query: &HistoryQuery,
    values: &mut Vec<SqlValue>,
) {
    if let Some(entity_ids) = &query.entity_ids {
        let placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" AND {alias}.entity_id IN ({placeholders})"));
        values.extend(entity_ids.iter().cloned().map(SqlValue::Text));
    }
    if !query.exclude_entities.is_empty() {
        let placeholders = std::iter::repeat_n("?", query.exclude_entities.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" AND {alias}.entity_id NOT IN ({placeholders})"));
        values.extend(query.exclude_entities.iter().cloned().map(SqlValue::Text));
    }
}

fn is_significant_history_state(state: &State) -> bool {
    const SIGNIFICANT_DOMAINS: &[&str] = &[
        "climate",
        "device_tracker",
        "humidifier",
        "thermostat",
        "water_heater",
    ];
    state.last_changed == state.last_updated
        || SIGNIFICANT_DOMAINS.contains(
            &state
                .entity_id
                .split_once('.')
                .map_or("", |(domain, _)| domain),
        )
}

fn initialize_schema(connection: &mut Connection) -> Result<()> {
    let version = schema_version(connection)?;
    match version {
        0 => {
            // The immediate transaction makes migration safe when two processes
            // open a new database at the same time.
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage_error("begin SQLite schema migration", error))?;
            let locked_version: i64 = transaction
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .map_err(|error| storage_error("read locked SQLite schema version", error))?;
            match locked_version {
                0 => {
                    ensure_unversioned_database_is_empty(&transaction)?;
                    transaction
                        .execute_batch(
                            "CREATE TABLE states (
                                 entity_id TEXT PRIMARY KEY NOT NULL,
                                 state_json TEXT NOT NULL
                             );
                             CREATE TABLE history (
                                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                                 entity_id TEXT NOT NULL,
                                 time_seconds INTEGER NOT NULL,
                                 time_nanoseconds INTEGER NOT NULL
                                     CHECK (time_nanoseconds BETWEEN 0 AND 999999999),
                                 state_json TEXT NOT NULL
                             );
                             CREATE INDEX history_entity_time_idx
                                 ON history(entity_id, time_seconds, time_nanoseconds, id);
                             CREATE TABLE events (
                                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                                 event_type TEXT NOT NULL,
                                 time_seconds INTEGER NOT NULL,
                                 time_nanoseconds INTEGER NOT NULL
                                     CHECK (time_nanoseconds BETWEEN 0 AND 999999999),
                                 event_json TEXT NOT NULL
                             );
                             CREATE TABLE metadata (
                                 key TEXT PRIMARY KEY NOT NULL,
                                 value_json TEXT NOT NULL
                             );
                             PRAGMA user_version = 2;",
                        )
                        .map_err(|error| storage_error("create SQLite schema", error))?;
                }
                SCHEMA_VERSION => {}
                other => {
                    return Err(Error::Storage(format!(
                        "unsupported SQLite schema version {other}; expected {SCHEMA_VERSION}"
                    )));
                }
            }
            transaction
                .commit()
                .map_err(|error| storage_error("commit SQLite schema migration", error))?;
        }
        1 => migrate_v1_to_v2(connection)?,
        SCHEMA_VERSION => {}
        other => {
            return Err(Error::Storage(format!(
                "unsupported SQLite schema version {other}; expected {SCHEMA_VERSION}"
            )));
        }
    }

    validate_schema_v2(connection)
}

fn migrate_v1_to_v2(connection: &mut Connection) -> Result<()> {
    migrate_v1_to_v2_with(connection, || Ok(()))
}

fn migrate_v1_to_v2_with(
    connection: &mut Connection,
    after_metadata_table: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| storage_error("begin SQLite v1-to-v2 migration", error))?;
    let locked_version: i64 = transaction
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| storage_error("read locked SQLite schema version", error))?;
    match locked_version {
        1 => {
            validate_schema_v1(&transaction)?;
            transaction
                .execute_batch(
                    "CREATE TABLE metadata (
                         key TEXT PRIMARY KEY NOT NULL,
                         value_json TEXT NOT NULL
                     );",
                )
                .map_err(|error| storage_error("create SQLite metadata table", error))?;
            after_metadata_table()?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|error| storage_error("set SQLite schema version", error))?;
        }
        SCHEMA_VERSION => {}
        other => {
            return Err(Error::Storage(format!(
                "unsupported SQLite schema version {other}; expected 1 or {SCHEMA_VERSION}"
            )));
        }
    }
    transaction
        .commit()
        .map_err(|error| storage_error("commit SQLite v1-to-v2 migration", error))
}

fn check_schema_compatibility(connection: &Connection) -> Result<()> {
    match schema_version(connection)? {
        0 => ensure_unversioned_database_is_empty(connection),
        1 => validate_schema_v1(connection),
        SCHEMA_VERSION => validate_schema_v2(connection),
        other => Err(Error::Storage(format!(
            "unsupported SQLite schema version {other}; expected 0, 1, or {SCHEMA_VERSION}"
        ))),
    }
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| storage_error("read SQLite schema version", error))
}

fn ensure_unversioned_database_is_empty(connection: &Connection) -> Result<()> {
    let user_objects: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type IN ('table', 'index', 'view', 'trigger') \
               AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| storage_error("inspect unversioned SQLite schema", error))?;
    if user_objects != 0 {
        return Err(Error::Storage(
            "refusing an unversioned SQLite database with existing schema objects".to_owned(),
        ));
    }
    Ok(())
}

fn validate_schema_v1(connection: &Connection) -> Result<()> {
    validate_schema_version(connection, 1)
}

fn validate_schema_v2(connection: &Connection) -> Result<()> {
    validate_schema_version(connection, 2)
}

fn validate_schema_version(connection: &Connection, version: i64) -> Result<()> {
    let mut statement = connection
        .prepare(
            "SELECT type, name FROM sqlite_master \
             WHERE type IN ('table', 'index', 'view', 'trigger') \
               AND name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
        )
        .map_err(|error| storage_error("inspect SQLite schema", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| storage_error("query SQLite schema objects", error))?;
    let mut objects = Vec::new();
    for row in rows {
        objects.push(row.map_err(|error| storage_error("read SQLite schema object", error))?);
    }
    let mut expected = vec![
        ("index".to_owned(), "history_entity_time_idx".to_owned()),
        ("table".to_owned(), "events".to_owned()),
        ("table".to_owned(), "history".to_owned()),
    ];
    if version == 2 {
        expected.push(("table".to_owned(), "metadata".to_owned()));
    }
    expected.push(("table".to_owned(), "states".to_owned()));
    if objects != expected {
        return Err(Error::Storage(
            "SQLite schema does not match the supported storage schema".to_owned(),
        ));
    }

    validate_table_columns(
        connection,
        "states",
        &[("entity_id", "TEXT"), ("state_json", "TEXT")],
    )?;
    validate_table_columns(
        connection,
        "history",
        &[
            ("id", "INTEGER"),
            ("entity_id", "TEXT"),
            ("time_seconds", "INTEGER"),
            ("time_nanoseconds", "INTEGER"),
            ("state_json", "TEXT"),
        ],
    )?;
    validate_table_columns(
        connection,
        "events",
        &[
            ("id", "INTEGER"),
            ("event_type", "TEXT"),
            ("time_seconds", "INTEGER"),
            ("time_nanoseconds", "INTEGER"),
            ("event_json", "TEXT"),
        ],
    )?;
    if version == 2 {
        validate_table_columns(
            connection,
            "metadata",
            &[("key", "TEXT"), ("value_json", "TEXT")],
        )?;
    }
    validate_history_index(connection)
}

fn validate_table_columns(
    connection: &Connection,
    table: &str,
    expected: &[(&str, &str)],
) -> Result<()> {
    let sql = match table {
        "states" => "PRAGMA table_info(states)",
        "history" => "PRAGMA table_info(history)",
        "events" => "PRAGMA table_info(events)",
        "metadata" => "PRAGMA table_info(metadata)",
        _ => {
            return Err(Error::Storage(
                "internal request for an unknown SQLite table".to_owned(),
            ));
        }
    };
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| storage_error("inspect SQLite table columns", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|error| storage_error("query SQLite table columns", error))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(|error| storage_error("read SQLite table column", error))?);
    }
    let expected = expected
        .iter()
        .map(|(name, kind)| {
            let primary = match table {
                "states" => *name == "entity_id",
                "metadata" => *name == "key",
                _ => *name == "id",
            };
            (
                (*name).to_owned(),
                (*kind).to_owned(),
                table == "states" || table == "metadata" || *name != "id",
                None,
                i64::from(primary),
            )
        })
        .collect::<Vec<_>>();
    if columns != expected {
        return Err(Error::Storage(format!(
            "SQLite table {table} has an unsupported column layout"
        )));
    }
    if table == "history" || table == "events" {
        let definition: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("inspect table constraints", error))?;
        let normalized: String = definition
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        if !normalized.contains("AUTOINCREMENT")
            || !normalized.contains("CHECK(TIME_NANOSECONDSBETWEEN0AND999999999)")
        {
            return Err(Error::Storage(format!(
                "SQLite table {table} has unsupported constraints"
            )));
        }
    }
    Ok(())
}

fn validate_history_index(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("PRAGMA index_info(history_entity_time_idx)")
        .map_err(|error| storage_error("inspect history index", error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(2))
        .map_err(|error| storage_error("query history index", error))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(|error| storage_error("read history index column", error))?);
    }
    if columns
        != vec![
            "entity_id".to_owned(),
            "time_seconds".to_owned(),
            "time_nanoseconds".to_owned(),
            "id".to_owned(),
        ]
    {
        return Err(Error::Storage(
            "SQLite history index has an unsupported column layout".to_owned(),
        ));
    }
    Ok(())
}

fn insert_event(transaction: &Transaction<'_>, event: &Event, event_json: &str) -> Result<()> {
    let (seconds, nanoseconds) = timestamp_parts(&event.time_fired);
    transaction
        .execute(
            "INSERT INTO events(event_type, time_seconds, time_nanoseconds, event_json) \
             VALUES (?1, ?2, ?3, ?4)",
            params![event.event_type, seconds, nanoseconds, event_json],
        )
        .map_err(|error| storage_error("persist event", error))?;
    Ok(())
}

fn encode_state(state: &State) -> Result<String> {
    serde_json::to_string(state).map_err(|error| {
        Error::InvalidInput(format!("state cannot be represented as JSON: {error}"))
    })
}

fn encode_event(event: &Event) -> Result<String> {
    serde_json::to_string(event).map_err(|error| {
        Error::InvalidInput(format!("event cannot be represented as JSON: {error}"))
    })
}

fn decode_state(encoded: &str, record_kind: &str) -> Result<State> {
    let state: State = serde_json::from_str(encoded)
        .map_err(|error| Error::Storage(format!("corrupt {record_kind} JSON: {error}")))?;
    validate_state(&state)
        .map_err(|error| Error::Storage(format!("invalid persisted {record_kind}: {error}")))?;
    Ok(state)
}

fn decode_event(encoded: &str) -> Result<Event> {
    let event: Event = serde_json::from_str(encoded)
        .map_err(|error| Error::Storage(format!("corrupt event JSON: {error}")))?;
    if event.event_type.is_empty() || event.event_type.chars().count() > 255 {
        return Err(Error::Storage("invalid persisted event type".into()));
    }
    Ok(event)
}

fn validate_state(state: &State) -> Result<()> {
    validate_entity_id(&state.entity_id)?;
    if state.state.chars().count() > 255 {
        return Err(Error::InvalidInput(
            "state value must be at most 255 characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_entity_id(entity_id: &str) -> Result<()> {
    let mut components = entity_id.split('.');
    let domain = components.next().unwrap_or_default();
    let object_id = components.next().unwrap_or_default();
    let valid_component = |component: &str| {
        !component.is_empty()
            && component
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    if components.next().is_some() || !valid_component(domain) || !valid_component(object_id) {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {entity_id:?}"
        )));
    }
    Ok(())
}

fn timestamp_parts(timestamp: &DateTime<Utc>) -> (i64, i64) {
    (
        timestamp.timestamp(),
        i64::from(timestamp.timestamp_subsec_nanos()),
    )
}

fn storage_error(action: &str, error: impl std::fmt::Display) -> Error {
    Error::Storage(format!("{action}: {error}"))
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn v1_migration_failure_rolls_back_schema_version_and_all_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE states (
                     entity_id TEXT PRIMARY KEY NOT NULL,
                     state_json TEXT NOT NULL
                 );
                 CREATE TABLE history (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     entity_id TEXT NOT NULL,
                     time_seconds INTEGER NOT NULL,
                     time_nanoseconds INTEGER NOT NULL
                         CHECK (time_nanoseconds BETWEEN 0 AND 999999999),
                     state_json TEXT NOT NULL
                 );
                 CREATE INDEX history_entity_time_idx
                     ON history(entity_id, time_seconds, time_nanoseconds, id);
                 CREATE TABLE events (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     event_type TEXT NOT NULL,
                     time_seconds INTEGER NOT NULL,
                     time_nanoseconds INTEGER NOT NULL
                         CHECK (time_nanoseconds BETWEEN 0 AND 999999999),
                     event_json TEXT NOT NULL
                 );
                 INSERT INTO states VALUES ('sensor.temperature', '{}');
                 INSERT INTO history(entity_id, time_seconds, time_nanoseconds, state_json)
                     VALUES ('sensor.temperature', 10, 20, '{}');
                 INSERT INTO events(event_type, time_seconds, time_nanoseconds, event_json)
                     VALUES ('state_changed', 10, 20, '{}');
                 PRAGMA user_version = 1;",
            )
            .unwrap();

        let result = migrate_v1_to_v2_with(&mut connection, || {
            Err(Error::Storage("injected migration failure".to_owned()))
        });
        assert!(matches!(result, Err(Error::Storage(_))));
        assert_eq!(schema_version(&connection).unwrap(), 1);
        let metadata_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(metadata_tables, 0);
        for table in ["states", "history", "events"] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "v1 {table} rows must survive rollback");
        }
    }
}
