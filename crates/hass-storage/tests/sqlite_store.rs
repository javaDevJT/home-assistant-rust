use std::sync::Arc;
use std::thread;

use chrono::{DateTime, TimeZone, Utc};
use hass_core::{Context, Error, Event, State, Store};
use hass_storage::SqliteStore;
use rusqlite::Connection;
use serde_json::{Map, json};

fn timestamp(seconds: i64, nanoseconds: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, nanoseconds)
        .single()
        .expect("valid test timestamp")
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
