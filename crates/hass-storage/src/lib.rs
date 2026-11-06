//! SQLite persistence for the Home Assistant runtime.
//!
//! The store is synchronous. Async callers should run its methods inside
//! `tokio::task::spawn_blocking`.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hass_core::{Error, Event, Result, State, Store};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A synchronous SQLite implementation of [`hass_core::Store`].
///
/// Disk databases use WAL mode and a bounded busy timeout. All related state,
/// history, and event writes are committed in one transaction.
pub struct SqliteStore {
    connection: Mutex<Connection>,
    _writer_lock: Option<File>,
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
        let mut lock_path = canonical.into_os_string();
        lock_path.push(".lock");
        let writer_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(Path::new(&lock_path))
            .map_err(|error| storage_error("open database writer lock", error))?;
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
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| storage_error("configure SQLite synchronous mode", error))?;

        initialize_schema(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            _writer_lock: Some(writer_lock),
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
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| Error::Storage("SQLite connection lock was poisoned".to_owned()))
    }
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
                             PRAGMA user_version = 1;",
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
        SCHEMA_VERSION => {}
        other => {
            return Err(Error::Storage(format!(
                "unsupported SQLite schema version {other}; expected {SCHEMA_VERSION}"
            )));
        }
    }

    validate_schema(connection)
}

fn check_schema_compatibility(connection: &Connection) -> Result<()> {
    match schema_version(connection)? {
        0 => ensure_unversioned_database_is_empty(connection),
        SCHEMA_VERSION => validate_schema(connection),
        other => Err(Error::Storage(format!(
            "unsupported SQLite schema version {other}; expected {SCHEMA_VERSION}"
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

fn validate_schema(connection: &Connection) -> Result<()> {
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
    let expected = vec![
        ("index".to_owned(), "history_entity_time_idx".to_owned()),
        ("table".to_owned(), "events".to_owned()),
        ("table".to_owned(), "history".to_owned()),
        ("table".to_owned(), "states".to_owned()),
    ];
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
            let primary = if table == "states" {
                *name == "entity_id"
            } else {
                *name == "id"
            };
            (
                (*name).to_owned(),
                (*kind).to_owned(),
                table == "states" || *name != "id",
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
    if table != "states" {
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
