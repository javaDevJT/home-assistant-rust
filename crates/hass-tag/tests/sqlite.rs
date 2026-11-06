//! Actual tag provider recording policy through the native SQLite store.

use std::sync::Arc;

use chrono::{Duration, Utc};
use hass_core::{Context, EventHistoryQuery, Runtime, Store};
use hass_entity::EntityLifecycle;
use hass_registry::Registries;
use hass_storage::SqliteStore;
use hass_tag::{EVENT_TAG_SCANNED, TagCreate, TagIntegration};
use serde_json::json;

#[tokio::test]
async fn actual_tag_keeps_live_attributes_and_projects_history_across_sqlite_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tag.sqlite");
    let store = Arc::new(SqliteStore::open(&path).unwrap());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let integration =
        TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
    let manager = integration.manager();
    let tag_id = "sqlite-recording-fixture";
    manager
        .create(TagCreate {
            tag_id: Some(tag_id.into()),
            name: Some("Hall reader".into()),
            ..Default::default()
        })
        .unwrap();
    let entry = registries
        .entities()
        .into_iter()
        .find(|entry| entry.platform == "tag" && entry.unique_id == tag_id)
        .unwrap();
    let mut public = runtime.subscribe();
    let context = Context::default();
    manager
        .scan(tag_id, Some("native-reader-fixture"), context.clone())
        .unwrap();
    let current = runtime.get_state(&entry.entity_id).unwrap();
    assert_eq!(current.attributes["tag_id"], tag_id);
    assert_eq!(
        current.attributes["last_scanned_by_device_id"],
        "native-reader-fixture"
    );
    let observed = std::iter::from_fn(|| public.try_recv().ok()).collect::<Vec<_>>();
    let scanned = observed
        .iter()
        .find(|event| event.event_type == EVENT_TAG_SCANNED)
        .unwrap();
    assert_eq!(scanned.context, context);
    assert_eq!(scanned.data["tag_id"], tag_id);
    assert!(
        observed
            .iter()
            .any(|event| event.event_type == "state_changed"
                && event.data["new_state"]["attributes"]["tag_id"] == tag_id)
    );
    let start = Utc::now() - Duration::minutes(1);
    let end = Utc::now() + Duration::minutes(1);
    let history = store.history(&entry.entity_id, start, end).unwrap();
    assert!(history.len() >= 2);
    assert!(
        history
            .iter()
            .all(|state| !state.attributes.contains_key("tag_id"))
    );
    assert_eq!(
        history.last().unwrap().attributes["last_scanned_by_device_id"],
        "native-reader-fixture"
    );
    let events = store
        .event_history_query(&EventHistoryQuery {
            start,
            end,
            event_types: None,
        })
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EVENT_TAG_SCANNED && event.data["tag_id"] == tag_id)
    );
    // Native event history deliberately retains full event snapshots. This is
    // separate from the filtered state-history contract and HA's event table.
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "state_changed"
                && event.data["new_state"]["attributes"]["tag_id"] == tag_id)
    );
    lifecycle.shutdown().await.unwrap();
    drop(integration);
    drop(manager);
    drop(public);
    drop(lifecycle);
    drop(registries);
    drop(runtime);
    drop(store);

    let store = Arc::new(SqliteStore::open(&path).unwrap());
    let restored = store
        .load_states()
        .unwrap()
        .into_iter()
        .find(|state| state.entity_id == entry.entity_id)
        .unwrap();
    assert_eq!(restored.attributes["tag_id"], tag_id);
    assert_eq!(restored.state, current.state);
    assert!(
        store
            .history(&entry.entity_id, start, end)
            .unwrap()
            .iter()
            .all(|state| !state.attributes.contains_key("tag_id"))
    );
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let integration =
        TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
    let after = registries
        .entities()
        .into_iter()
        .find(|item| item.platform == "tag" && item.unique_id == tag_id)
        .unwrap();
    assert_eq!(after.id, entry.id);
    assert_eq!(after.entity_id, entry.entity_id);
    assert_eq!(
        runtime.get_state(&entry.entity_id).unwrap().attributes["tag_id"],
        json!(tag_id)
    );
    assert_eq!(
        integration.manager().list().unwrap()[0].name.as_deref(),
        Some("Hall reader")
    );
    integration.shutdown().unwrap();
    lifecycle.shutdown().await.unwrap();
}
