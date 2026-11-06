use std::sync::Arc;
use std::thread;

use chrono::{DateTime, TimeZone, Utc};
use hass_core::{
    Context, Error, Event, EventHistoryQuery, HistoryQuery, Runtime, State, StateRecordingPolicy,
    Store,
};
use hass_storage::SqliteStore;
use rusqlite::Connection;
use serde_json::{Map, json};

fn timestamp(seconds: i64, nanoseconds: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, nanoseconds)
        .single()
        .expect("valid test timestamp")
}

#[cfg(unix)]
#[test]
fn owned_database_and_sidecars_are_private_even_after_legacy_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.sqlite");
    let store = SqliteStore::open(&path).unwrap();
    store
        .set_data("auth.fixture", &json!({"signing_key":"fixture-only"}))
        .unwrap();
    for suffix in ["", "-wal", "-shm", ".lock"] {
        let file = dir.path().join(format!("private.sqlite{suffix}"));
        if file.exists() {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    drop(store);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn foreign_database_refusal_preserves_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated(value TEXT)")
        .unwrap();
    drop(connection);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(SqliteStore::open(&path).is_err());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn disk_database_refuses_a_second_writer_and_releases_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let store = SqliteStore::open(&path).unwrap();
    assert!(matches!(SqliteStore::open(&path), Err(Error::Storage(_))));
    drop(store);
    assert!(SqliteStore::open(&path).is_ok());
}

#[test]
fn restore_rejects_miskeyed_and_invalid_state_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let store = SqliteStore::open(&path).unwrap();
    let mut snapshot = state("20", timestamp(100, 0));
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO states(entity_id, state_json) VALUES (?1, ?2)",
            rusqlite::params!["sensor.other", serde_json::to_string(&snapshot).unwrap()],
        )
        .unwrap();
    assert!(matches!(store.load_states(), Err(Error::Storage(_))));
    connection.execute("DELETE FROM states", []).unwrap();
    snapshot.state = "x".repeat(256);
    connection
        .execute(
            "INSERT INTO states(entity_id, state_json) VALUES (?1, ?2)",
            rusqlite::params![
                snapshot.entity_id,
                serde_json::to_string(&snapshot).unwrap()
            ],
        )
        .unwrap();
    assert!(matches!(store.load_states(), Err(Error::Storage(_))));
    snapshot.state = "20".into();
    connection.execute(
        "INSERT INTO history(entity_id, time_seconds, time_nanoseconds, state_json) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params!["sensor.other", 100, 0, serde_json::to_string(&snapshot).unwrap()],
    ).unwrap();
    assert!(matches!(
        store.history("sensor.other", timestamp(99, 0), timestamp(101, 0)),
        Err(Error::Storage(_))
    ));
}

#[test]
fn versioned_database_missing_primary_key_is_refused_before_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(SqliteStore::open(&path).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("DROP TABLE states; CREATE TABLE states(entity_id TEXT NOT NULL, state_json TEXT NOT NULL);").unwrap();
    drop(connection);
    assert!(matches!(SqliteStore::open(&path), Err(Error::Storage(_))));
}

fn state(value: &str, at: DateTime<Utc>) -> State {
    let mut attributes = Map::new();
    attributes.insert("unit_of_measurement".to_owned(), json!("°C"));
    State {
        entity_id: "sensor.temperature".to_owned(),
        state: value.to_owned(),
        attributes,
        last_changed: at,
        last_updated: at,
        last_reported: at,
        context: Context {
            id: "persisted-state-context".to_owned(),
            parent_id: Some("parent-context".to_owned()),
            user_id: Some("user-id".to_owned()),
        },
    }
}

fn event(entity_id: &str, at: DateTime<Utc>) -> Event {
    Event {
        event_type: "state_changed".to_owned(),
        data: json!({ "entity_id": entity_id })
            .as_object()
            .expect("object")
            .clone(),
        origin: "LOCAL".to_owned(),
        time_fired: at,
        context: Context::default(),
    }
}

fn custom_event(event_type: &str, at: DateTime<Utc>) -> Event {
    Event {
        event_type: event_type.to_owned(),
        data: Map::new(),
        origin: "LOCAL".to_owned(),
        time_fired: at,
        context: Context::default(),
    }
}

fn create_v1_database(path: &std::path::Path) {
    let connection = Connection::open(path).expect("open v1 fixture database");
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
             PRAGMA user_version = 1;",
        )
        .expect("create original v1 schema");

    let at = timestamp(123, 456);
    let snapshot = state("v1 persisted", at);
    let state_json = serde_json::to_string(&snapshot).unwrap();
    connection
        .execute(
            "INSERT INTO states(entity_id, state_json) VALUES (?1, ?2)",
            rusqlite::params![snapshot.entity_id, state_json],
        )
        .expect("insert v1 state");
    connection
        .execute(
            "INSERT INTO history(entity_id, time_seconds, time_nanoseconds, state_json) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "sensor.temperature",
                at.timestamp(),
                at.timestamp_subsec_nanos(),
                state_json
            ],
        )
        .expect("insert v1 history");
    let event = event("sensor.temperature", at);
    connection
        .execute(
            "INSERT INTO events(event_type, time_seconds, time_nanoseconds, event_json) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                event.event_type,
                at.timestamp(),
                at.timestamp_subsec_nanos(),
                serde_json::to_string(&event).unwrap()
            ],
        )
        .expect("insert v1 event");
}

#[test]
fn migrates_v1_rows_and_durable_metadata_survives_restart_and_delete() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("v1.sqlite3");
    create_v1_database(&database);

    let metadata = json!({"users": [{"id": "alice", "enabled": true}]});
    {
        let store = SqliteStore::open(&database).expect("migrate v1 store");
        assert_eq!(store.load_states().unwrap()[0].state, "v1 persisted");
        let history = store
            .history(
                "sensor.temperature",
                timestamp(123, 456),
                timestamp(123, 456),
            )
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].state, "v1 persisted");
        assert_eq!(store.get_data("auth.users").unwrap(), None);

        store.set_data("auth.users", &metadata).unwrap();
        assert_eq!(
            store.get_data("auth.users").unwrap(),
            Some(metadata.clone())
        );
        assert!(store.delete_data("auth.users").unwrap());
        assert!(!store.delete_data("auth.users").unwrap());
        store.set_data("auth.users", &metadata).unwrap();
    }

    let reopened = SqliteStore::open(&database).expect("reopen migrated store");
    assert_eq!(reopened.get_data("auth.users").unwrap(), Some(metadata));
    assert_eq!(reopened.load_states().unwrap()[0].state, "v1 persisted");
    assert_eq!(
        reopened
            .history(
                "sensor.temperature",
                timestamp(123, 456),
                timestamp(123, 456)
            )
            .unwrap()
            .len(),
        1
    );
    drop(reopened);

    let connection = Connection::open(&database).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let preserved_events: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(preserved_events, 1);
    let preserved_event_json: String = connection
        .query_row("SELECT event_json FROM events", [], |row| row.get(0))
        .unwrap();
    let preserved_event: Event = serde_json::from_str(&preserved_event_json).unwrap();
    assert_eq!(preserved_event.event_type, "state_changed");
}

#[test]
fn malformed_v1_schema_is_refused_without_schema_or_row_changes() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("malformed-v1.sqlite3");
    create_v1_database(&database);
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE unrelated(value TEXT);")
            .unwrap();
    }

    assert!(matches!(
        SqliteStore::open(&database),
        Err(Error::Storage(_))
    ));

    let connection = Connection::open(&database).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let current_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM states", [], |row| row.get(0))
        .unwrap();
    assert_eq!(current_rows, 1);
    let metadata_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(metadata_tables, 0);
}

#[test]
fn metadata_storage_errors_are_returned_without_partial_mutation() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("metadata-errors.sqlite3");
    let store = SqliteStore::open(&database).expect("open store");
    let original = json!({"generation": 1});
    store.set_data("registry", &original).unwrap();

    let connection = Connection::open(&database).expect("open second connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_metadata_update BEFORE UPDATE ON metadata \
             BEGIN SELECT RAISE(ABORT, 'injected metadata update failure'); END;
             CREATE TRIGGER reject_metadata_delete BEFORE DELETE ON metadata \
             BEGIN SELECT RAISE(ABORT, 'injected metadata delete failure'); END;",
        )
        .expect("install metadata failure triggers");

    assert!(matches!(
        store.set_data("registry", &json!({"generation": 2})),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        store.delete_data("registry"),
        Err(Error::Storage(_))
    ));
    assert_eq!(store.get_data("registry").unwrap(), Some(original));

    connection
        .execute_batch("DROP TRIGGER reject_metadata_update;")
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value_json = ?1 WHERE key = ?2",
            rusqlite::params!["not-json", "registry"],
        )
        .unwrap();
    assert!(matches!(store.get_data("registry"), Err(Error::Storage(_))));
}

#[test]
fn disk_store_reopens_and_history_is_inclusive_and_chronological() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("home-assistant.sqlite3");

    {
        let store = SqliteStore::open(&database).expect("open store");
        for (value, at) in [
            ("three", timestamp(3, 0)),
            ("one", timestamp(1, 1_000)),
            ("two", timestamp(2, 500)),
        ] {
            store
                .put_state(&state(value, at), Some(&event("sensor.temperature", at)))
                .expect("persist state");
        }

        let history = store
            .history("sensor.temperature", timestamp(2, 500), timestamp(3, 0))
            .expect("read bounded history");
        assert_eq!(
            history
                .iter()
                .map(|entry| entry.state.as_str())
                .collect::<Vec<_>>(),
            ["two", "three"]
        );
        let exact = store
            .history("sensor.temperature", timestamp(2, 500), timestamp(2, 500))
            .expect("read exact boundary");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].state, "two");
    }

    let reopened = SqliteStore::open(&database).expect("reopen store");
    let states = reopened.load_states().expect("load current state");
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].state, "two");
    assert_eq!(
        states[0].attributes.get("unit_of_measurement"),
        Some(&json!("°C"))
    );
    assert_eq!(states[0].last_updated, timestamp(2, 500));
    assert_eq!(states[0].context.id, "persisted-state-context");
    assert_eq!(
        states[0].context.parent_id.as_deref(),
        Some("parent-context")
    );
    assert_eq!(states[0].context.user_id.as_deref(), Some("user-id"));

    let all_history = reopened
        .history("sensor.temperature", timestamp(0, 0), timestamp(4, 0))
        .expect("history survives reopen");
    assert_eq!(
        all_history
            .iter()
            .map(|entry| entry.state.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "three"]
    );
}

#[test]
fn state_without_event_does_not_create_history() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    let at = timestamp(5, 0);
    store
        .put_state(&state("reported", at), None)
        .expect("persist report");

    assert!(
        store
            .history("sensor.temperature", timestamp(0, 0), timestamp(10, 0))
            .expect("read history")
            .is_empty()
    );
    assert_eq!(
        store.load_states().expect("load state")[0].state,
        "reported"
    );
}

#[test]
fn runtime_recording_projection_keeps_current_and_events_full_across_reopen() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("recording-projection.sqlite3");
    let store = Arc::new(SqliteStore::open(&database).expect("open store"));
    let runtime = Runtime::new(store.clone()).expect("create runtime");
    let policy = StateRecordingPolicy::default().with_unrecorded_attributes(["tag_id"]);
    let mut attributes = Map::new();
    attributes.insert("tag_id".into(), json!("fixture-tag"));
    attributes.insert("device_id".into(), json!("scanner-1"));
    let mut public_events = runtime.subscribe();

    let changed = runtime
        .set_state_with_recording_policy(
            "tag.fixture",
            "2026-11-06T12:00:00+00:00",
            attributes,
            Context::default(),
            false,
            &policy,
        )
        .expect("write state");
    assert_eq!(changed.state.attributes["tag_id"], "fixture-tag");
    assert_eq!(
        public_events.try_recv().unwrap().data["new_state"]["attributes"]["tag_id"],
        "fixture-tag"
    );
    let history = runtime
        .history_query(&HistoryQuery {
            entity_ids: Some(vec!["tag.fixture".into()]),
            start: DateTime::<Utc>::MIN_UTC,
            end: DateTime::<Utc>::MAX_UTC,
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        })
        .expect("query history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].states.len(), 1);
    assert!(!history[0].states[0].attributes.contains_key("tag_id"));
    assert_eq!(history[0].states[0].attributes["device_id"], "scanner-1");
    let events = runtime
        .event_history_query(&EventHistoryQuery {
            start: DateTime::<Utc>::MIN_UTC,
            end: DateTime::<Utc>::MAX_UTC,
            event_types: Some(vec!["state_changed".into()]),
        })
        .expect("query event history");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].data["new_state"]["attributes"]["tag_id"],
        "fixture-tag"
    );

    drop(runtime);
    drop(store);
    let reopened_store = Arc::new(SqliteStore::open(&database).expect("reopen store"));
    let reopened_runtime = Runtime::new(reopened_store.clone()).expect("restore runtime");
    let current = reopened_runtime
        .get_state("tag.fixture")
        .expect("restored current state");
    assert_eq!(current.attributes["tag_id"], "fixture-tag");
    let history = reopened_store
        .history(
            "tag.fixture",
            DateTime::<Utc>::MIN_UTC,
            DateTime::<Utc>::MAX_UTC,
        )
        .expect("history survives reopen");
    assert_eq!(history.len(), 1);
    assert!(!history[0].attributes.contains_key("tag_id"));
    drop(reopened_runtime);
}

#[test]
fn deleting_current_state_keeps_its_prior_history() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    let at = timestamp(10, 0);
    store
        .put_state(
            &state("present", at),
            Some(&event("sensor.temperature", at)),
        )
        .expect("persist state");
    store
        .remove_state(
            "sensor.temperature",
            &event("sensor.temperature", timestamp(11, 0)),
        )
        .expect("remove current state");

    assert!(store.load_states().expect("load states").is_empty());
    let history = store
        .history("sensor.temperature", timestamp(0, 0), timestamp(20, 0))
        .expect("read retained history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].state, "present");
}

#[test]
fn multi_entity_history_query_returns_boundary_snapshot_and_missing_series() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    for (entity_id, value, seconds) in [
        ("sensor.temperature", "before", 10),
        ("sensor.temperature", "inside", 20),
        ("sensor.temperature", "at_end", 30),
        ("switch.fixture", "on", 12),
    ] {
        let at = timestamp(seconds, 0);
        let mut snapshot = state(value, at);
        snapshot.entity_id = entity_id.to_owned();
        snapshot.context.id = format!("ctx-{entity_id}-{seconds}");
        store
            .put_state(&snapshot, Some(&event(entity_id, at)))
            .expect("persist history row");
    }

    let query = HistoryQuery {
        entity_ids: Some(vec![
            "sensor.temperature".into(),
            "sensor.missing".into(),
            "switch.fixture".into(),
        ]),
        start: timestamp(15, 0),
        end: timestamp(30, 0),
        include_start_time_state: true,
        significant_changes_only: false,
        minimal_response: true,
        no_attributes: true,
        exclude_entities: vec!["switch.fixture".into()],
    };
    let series = store.history_query(&query).expect("multi-entity history");
    assert_eq!(series.len(), 3);
    assert_eq!(series[0].entity_id, "sensor.temperature");
    assert_eq!(series[0].states.len(), 2);
    let boundary = &series[0].states[0];
    assert_eq!(boundary.state, "before");
    assert_eq!(boundary.last_changed, timestamp(15, 0));
    assert_eq!(boundary.last_updated, timestamp(15, 0));
    assert_eq!(boundary.last_reported, timestamp(15, 0));
    assert_eq!(boundary.context.id, "ctx-sensor.temperature-10");
    assert_eq!(
        boundary.attributes,
        state("before", timestamp(10, 0)).attributes
    );
    assert_eq!(series[0].states[1].state, "inside");
    assert!(series[1].states.is_empty());
    assert!(series[2].states.is_empty());

    let all = store
        .history_query(&HistoryQuery {
            entity_ids: None,
            start: timestamp(0, 0),
            end: timestamp(40, 0),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: vec!["sensor.temperature".into()],
        })
        .expect("all-entity history");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].entity_id, "switch.fixture");
}

#[test]
fn event_history_query_filters_types_and_validates_persisted_keys() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    for (event_type, at) in [("edge", 10), ("wanted", 20), ("ignored", 25), ("edge", 30)] {
        store
            .append_event(&custom_event(event_type, timestamp(at, 0)))
            .expect("persist event");
    }
    let events = store
        .event_history_query(&EventHistoryQuery {
            start: timestamp(10, 0),
            end: timestamp(30, 0),
            event_types: Some(vec!["wanted".into(), "ignored".into()]),
        })
        .expect("read event history");
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["wanted", "ignored"]
    );
    assert_eq!(events[0].time_fired, timestamp(20, 0));
    assert_eq!(events[1].time_fired, timestamp(25, 0));

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("corrupt-event.sqlite");
    let corrupt = SqliteStore::open(&database).expect("open corrupt-event fixture");
    let mut mismatched = custom_event("wrong", timestamp(21, 0));
    mismatched.data.insert("payload".into(), json!("fixture"));
    let encoded = serde_json::to_string(&mismatched).unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO events(event_type, time_seconds, time_nanoseconds, event_json) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["indexed", 21, 0, encoded],
        )
        .unwrap();
    assert!(matches!(
        corrupt.event_history_query(&EventHistoryQuery {
            start: timestamp(20, 0),
            end: timestamp(22, 0),
            event_types: None,
        }),
        Err(Error::Storage(_))
    ));
}

#[test]
fn significant_history_filter_keeps_significant_domains_and_state_changes() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    let start = timestamp(10, 0);
    let mut ordinary = state("same", timestamp(20, 0));
    ordinary.entity_id = "sensor.attribute_only".into();
    ordinary.last_changed = start;
    store
        .put_state(
            &ordinary,
            Some(&event("sensor.attribute_only", timestamp(20, 0))),
        )
        .unwrap();
    let mut significant_domain = state("same", timestamp(21, 0));
    significant_domain.entity_id = "climate.attribute_only".into();
    significant_domain.last_changed = start;
    store
        .put_state(
            &significant_domain,
            Some(&event("climate.attribute_only", timestamp(21, 0))),
        )
        .unwrap();
    let mut changed_state = state("changed", timestamp(22, 0));
    changed_state.entity_id = "sensor.state_change".into();
    store
        .put_state(
            &changed_state,
            Some(&event("sensor.state_change", timestamp(22, 0))),
        )
        .unwrap();

    let result = store
        .history_query(&HistoryQuery {
            entity_ids: None,
            start: timestamp(0, 0),
            end: timestamp(30, 0),
            include_start_time_state: false,
            significant_changes_only: true,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        })
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].entity_id, "climate.attribute_only");
    assert_eq!(result[1].entity_id, "sensor.state_change");
}

#[test]
fn purge_is_transactional_and_preserves_current_state_metadata_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("purge.sqlite");
    let store = SqliteStore::open(&database).expect("open store");
    let old_at = timestamp(10, 0);
    let cutoff = timestamp(20, 500);
    let mut old = state("old", old_at);
    store
        .put_state(&old, Some(&event("sensor.temperature", old_at)))
        .unwrap();
    old = state("current", cutoff);
    store
        .put_state(&old, Some(&event("sensor.temperature", cutoff)))
        .unwrap();
    store
        .set_data("auth.fixture", &json!({"secret":"preserved"}))
        .unwrap();
    store
        .append_event(&custom_event("before_cutoff", timestamp(20, 499)))
        .unwrap();
    store
        .append_event(&custom_event("at_cutoff", cutoff))
        .unwrap();

    let purged = store.purge_before(cutoff).expect("purge old history");
    assert_eq!(purged.states_deleted, 1);
    assert_eq!(purged.events_deleted, 2);
    assert_eq!(store.load_states().unwrap(), vec![old.clone()]);
    assert_eq!(
        store.get_data("auth.fixture").unwrap(),
        Some(json!({"secret":"preserved"}))
    );
    assert!(
        store
            .history("sensor.temperature", timestamp(0, 0), timestamp(19, 0))
            .unwrap()
            .is_empty()
    );
    let boundary = store
        .history_query(&HistoryQuery {
            entity_ids: Some(vec!["sensor.temperature".into()]),
            start: cutoff,
            end: timestamp(30, 0),
            include_start_time_state: true,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        })
        .unwrap();
    assert_eq!(boundary[0].states.len(), 1);
    assert_eq!(boundary[0].states[0].last_updated, cutoff);
    drop(store);

    let reopened = SqliteStore::open(&database).expect("reopen purged store");
    assert_eq!(reopened.load_states().unwrap(), vec![old]);
    assert_eq!(
        reopened.get_data("auth.fixture").unwrap(),
        Some(json!({"secret":"preserved"}))
    );
    let events = reopened
        .event_history_query(&EventHistoryQuery {
            start: timestamp(20, 0),
            end: timestamp(21, 0),
            event_types: None,
        })
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["state_changed", "at_cutoff"]
    );
}

#[test]
fn failed_event_delete_rolls_back_state_history_purge() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("purge-rollback.sqlite");
    let store = SqliteStore::open(&database).expect("open store");
    let at = timestamp(10, 0);
    store
        .put_state(&state("old", at), Some(&event("sensor.temperature", at)))
        .unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_event_purge BEFORE DELETE ON events \
             BEGIN SELECT RAISE(ABORT, 'injected event purge failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        store.purge_before(timestamp(20, 0)),
        Err(Error::Storage(_))
    ));
    assert_eq!(
        store
            .history("sensor.temperature", timestamp(0, 0), timestamp(20, 0))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .event_history_query(&EventHistoryQuery {
                start: timestamp(0, 0),
                end: timestamp(20, 0),
                event_types: None,
            })
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn sqlite_repack_preserves_private_data_and_reopens_with_full_durability() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("repack.sqlite");
    let store = SqliteStore::open(&database).expect("open store");
    let current = state("preserved", timestamp(10, 0));
    store
        .put_state(
            &current,
            Some(&event("sensor.temperature", timestamp(10, 0))),
        )
        .unwrap();
    store
        .set_data("auth.fixture", &json!({"secret":"preserved"}))
        .unwrap();
    store
        .append_event(&custom_event("fixture", timestamp(11, 0)))
        .unwrap();

    store.repack().expect("repack SQLite store");
    assert_eq!(store.load_states().unwrap(), vec![current.clone()]);
    assert_eq!(
        store.get_data("auth.fixture").unwrap(),
        Some(json!({"secret":"preserved"}))
    );
    let connection = Connection::open(&database).unwrap();
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(synchronous, 2);
    drop(connection);

    drop(store);
    let reopened = SqliteStore::open(&database).expect("reopen repacked database");
    assert_eq!(reopened.load_states().unwrap(), vec![current]);
    assert_eq!(
        reopened.get_data("auth.fixture").unwrap(),
        Some(json!({"secret":"preserved"}))
    );
    assert_eq!(
        reopened
            .history("sensor.temperature", timestamp(0, 0), timestamp(20, 0))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .event_history_query(&EventHistoryQuery {
                start: timestamp(0, 0),
                end: timestamp(20, 0),
                event_types: None,
            })
            .unwrap()
            .len(),
        2
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let lock_file = dir.path().join("repack.sqlite.lock");
        assert_eq!(
            std::fs::metadata(lock_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn history_query_rejects_malformed_state_json_and_oversized_filter() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("corrupt-history.sqlite");
    let store = SqliteStore::open(&database).expect("open store");
    let at = timestamp(10, 0);
    store
        .put_state(&state("valid", at), Some(&event("sensor.temperature", at)))
        .unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE history SET state_json = ?1",
            ["not valid state json"],
        )
        .unwrap();
    assert!(matches!(
        store.history_query(&HistoryQuery {
            entity_ids: None,
            start: timestamp(0, 0),
            end: timestamp(20, 0),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        }),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        store.history_query(&HistoryQuery {
            entity_ids: Some(vec![
                "sensor.valid".into();
                hass_core::MAX_HISTORY_QUERY_FILTERS + 1
            ]),
            start: timestamp(0, 0),
            end: timestamp(20, 0),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        }),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn history_and_event_query_result_limits_error_instead_of_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("history-limit.sqlite");
    let store = SqliteStore::open(&database).expect("open store");
    let at = timestamp(1, 0);
    let state_json = serde_json::to_string(&state("same", at)).unwrap();
    let event_json = serde_json::to_string(&custom_event("fixture", at)).unwrap();
    let connection = Connection::open(&database).unwrap();
    let row_count = (hass_core::MAX_HISTORY_QUERY_RESULTS + 1) as i64;
    connection
        .execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1) \
             INSERT INTO history(entity_id, time_seconds, time_nanoseconds, state_json) \
             SELECT ?2, ?3, ?4, ?5 FROM seq",
            rusqlite::params![row_count, "sensor.temperature", 1, 0, state_json],
        )
        .unwrap();
    connection
        .execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1) \
             INSERT INTO events(event_type, time_seconds, time_nanoseconds, event_json) \
             SELECT ?2, ?3, ?4, ?5 FROM seq",
            rusqlite::params![row_count, "fixture", 1, 0, event_json],
        )
        .unwrap();
    assert!(matches!(
        store.history_query(&HistoryQuery {
            entity_ids: Some(vec!["sensor.temperature".into()]),
            start: timestamp(0, 0),
            end: timestamp(2, 0),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        }),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        store.event_history_query(&EventHistoryQuery {
            start: timestamp(0, 0),
            end: timestamp(2, 0),
            event_types: None,
        }),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn concurrent_writes_are_serialized_and_all_states_remain_readable() {
    let store = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
    let mut workers = Vec::new();
    for worker in 0..8 {
        let store = Arc::clone(&store);
        workers.push(thread::spawn(move || {
            for item in 0..16 {
                let mut next = state(&format!("{worker}-{item}"), timestamp(20, 0));
                next.entity_id = format!("sensor.worker_{worker}_{item}");
                store.put_state(&next, None).expect("concurrent write");
            }
        }));
    }
    for worker in workers {
        worker.join().expect("writer thread");
    }

    let states = store.load_states().expect("read concurrent states");
    assert_eq!(states.len(), 128);
    assert!(
        states
            .windows(2)
            .all(|pair| pair[0].entity_id < pair[1].entity_id)
    );
}

#[test]
fn concurrent_runtime_metadata_writes_remain_readable() {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
    let runtime = Runtime::new(store.clone()).expect("create runtime");
    let mut workers = Vec::new();
    for worker in 0..8 {
        let runtime = runtime.clone();
        workers.push(thread::spawn(move || {
            for item in 0..16 {
                let key = format!("worker.{worker}.{item}");
                let value = json!({"worker": worker, "item": item});
                runtime.set_data(&key, &value).expect("persist metadata");
            }
        }));
    }
    for worker in workers {
        worker.join().expect("metadata writer thread");
    }

    for worker in 0..8 {
        for item in 0..16 {
            let key = format!("worker.{worker}.{item}");
            assert_eq!(
                store.get_data(&key).unwrap(),
                Some(json!({"worker": worker, "item": item}))
            );
        }
    }
}

#[test]
fn state_history_and_event_roll_back_together_on_event_failure() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("rollback.sqlite3");
    let store = SqliteStore::open(&database).expect("open store");
    let connection = Connection::open(&database).expect("open second connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events \
             BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
        )
        .expect("install failure trigger");

    let at = timestamp(30, 0);
    let result = store.put_state(
        &state("must roll back", at),
        Some(&event("sensor.temperature", at)),
    );
    assert!(matches!(result, Err(Error::Storage(_))));
    assert!(
        store
            .load_states()
            .expect("check current states")
            .is_empty()
    );
    assert!(
        store
            .history("sensor.temperature", timestamp(0, 0), timestamp(40, 0))
            .expect("check history")
            .is_empty()
    );

    store
        .put_state(&state("retain this state", timestamp(31, 0)), None)
        .expect("persist state before deletion");
    let removal = store.remove_state(
        "sensor.temperature",
        &event("sensor.temperature", timestamp(32, 0)),
    );
    assert!(matches!(removal, Err(Error::Storage(_))));
    let current = store.load_states().expect("state survives failed removal");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].state, "retain this state");
}

#[test]
fn projected_state_history_and_event_roll_back_together_on_event_failure() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("projected-rollback.sqlite3");
    let store = SqliteStore::open(&database).expect("open store");
    let connection = Connection::open(&database).expect("open second connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events \
             BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
        )
        .expect("install failure trigger");

    let at = timestamp(50, 0);
    let mut current_state = state("must roll back", at);
    current_state
        .attributes
        .insert("tag_id".into(), json!("fixture-tag"));
    let mut history_state = current_state.clone();
    history_state.attributes.remove("tag_id");
    let result = store.put_state_with_history(
        &current_state,
        &history_state,
        &event("sensor.temperature", at),
    );
    assert!(matches!(result, Err(Error::Storage(_))));
    assert!(
        store
            .load_states()
            .expect("check current states")
            .is_empty()
    );
    assert!(
        store
            .history("sensor.temperature", timestamp(0, 0), timestamp(60, 0))
            .expect("check history")
            .is_empty()
    );
    assert!(
        store
            .event_history_query(&EventHistoryQuery {
                start: timestamp(0, 0),
                end: timestamp(60, 0),
                event_types: None,
            })
            .expect("check events")
            .is_empty()
    );
}

#[test]
fn foreign_and_unknown_schema_versions_are_refused() {
    let directory = tempfile::tempdir().expect("temp directory");
    let foreign_database = directory.path().join("foreign.sqlite3");
    let foreign = Connection::open(&foreign_database).expect("open foreign database");
    foreign
        .execute_batch("CREATE TABLE unrelated(value TEXT);")
        .expect("create foreign schema");
    let foreign_mode: String = foreign
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read foreign journal mode");
    drop(foreign);
    assert!(matches!(
        SqliteStore::open(&foreign_database),
        Err(Error::Storage(_))
    ));
    let foreign_after = Connection::open(&foreign_database).expect("reopen foreign database");
    let foreign_mode_after: String = foreign_after
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal mode after refusal");
    assert_eq!(foreign_mode_after, foreign_mode);

    let future_database = directory.path().join("future.sqlite3");
    let future = Connection::open(&future_database).expect("open future database");
    future
        .execute_batch("PRAGMA user_version = 42;")
        .expect("set future version");
    let future_mode: String = future
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read future journal mode");
    drop(future);
    assert!(matches!(
        SqliteStore::open(&future_database),
        Err(Error::Storage(_))
    ));
    let future_after = Connection::open(&future_database).expect("reopen future database");
    let future_mode_after: String = future_after
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read future mode after refusal");
    assert_eq!(future_mode_after, future_mode);
}

#[test]
fn corrupt_schema_and_corrupt_persisted_json_are_reported() {
    let directory = tempfile::tempdir().expect("temp directory");
    let schema_database = directory.path().join("corrupt-schema.sqlite3");
    drop(SqliteStore::open(&schema_database).expect("create valid schema"));
    let connection = Connection::open(&schema_database).expect("open raw connection");
    connection
        .execute_batch("DROP TABLE history;")
        .expect("corrupt schema");
    drop(connection);
    assert!(matches!(
        SqliteStore::open(&schema_database),
        Err(Error::Storage(_))
    ));

    let data_database = directory.path().join("corrupt-data.sqlite3");
    let store = SqliteStore::open(&data_database).expect("create data database");
    store
        .put_state(&state("before corruption", timestamp(40, 0)), None)
        .expect("persist state");
    drop(store);
    let connection = Connection::open(&data_database).expect("open raw connection");
    connection
        .execute("UPDATE states SET state_json = ?1", ["not valid JSON"])
        .expect("corrupt state JSON");
    drop(connection);
    let reopened = SqliteStore::open(&data_database).expect("open schema");
    assert!(matches!(reopened.load_states(), Err(Error::Storage(_))));
}

#[test]
fn invalid_ranges_and_states_are_rejected_before_writing() {
    let store = SqliteStore::in_memory().expect("in-memory store");
    assert!(matches!(
        store.history("sensor.temperature", timestamp(2, 0), timestamp(1, 0)),
        Err(Error::InvalidInput(_))
    ));

    let mut invalid = state("valid", timestamp(50, 0));
    invalid.entity_id = "Sensor.Bad.Entity".to_owned();
    assert!(matches!(
        store.put_state(&invalid, None),
        Err(Error::InvalidInput(_))
    ));
    assert!(
        store
            .load_states()
            .expect("load after invalid input")
            .is_empty()
    );
}
