use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use hass_core::{Error, Event, MemoryStore, Runtime, State, Store};
use serde_json::{Value, json};

use crate::{
    AreaInput, AreaUpdate, DeviceRegistration, DeviceUpdate, EntityRegistration, EntityUpdate,
    FloorInput, LabelInput, Registries,
};

#[test]
fn service_device_type_survives_provider_refresh_removal_and_restore() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let mut registration = DeviceRegistration {
        entry_type: Some("service".into()),
        config_entry_id: Some("uptime-entry".into()),
        identifiers: BTreeSet::from([("uptime".into(), "uptime-entry".into())]),
        name: Some("Uptime".into()),
        ..Default::default()
    };
    let device = registries
        .get_or_create_device(registration.clone())
        .unwrap();
    assert_eq!(device.entry_type.as_deref(), Some("service"));
    registries
        .update_device(
            &device.id,
            DeviceUpdate {
                name_by_user: Some(Some("My service".into())),
                ..Default::default()
            },
        )
        .unwrap();
    registration.name = Some("Provider refreshed".into());
    let updated = registries
        .get_or_create_device(registration.clone())
        .unwrap();
    assert_eq!(updated.name_by_user.as_deref(), Some("My service"));
    assert_eq!(updated.entry_type.as_deref(), Some("service"));
    registries.remove_device(&device.id).unwrap();
    drop(registries);
    drop(runtime);
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime).unwrap();
    let restored = registries
        .get_or_create_device(registration.clone())
        .unwrap();
    assert_eq!(restored.id, device.id);
    assert_eq!(restored.entry_type.as_deref(), Some("service"));
    assert_eq!(restored.name_by_user.as_deref(), Some("My service"));
    registration.entry_type = Some("unsupported-type".into());
    assert!(matches!(
        registries.get_or_create_device(registration),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(registries.devices().len(), 1);
}

#[test]
fn persists_registry_state_and_stable_identity_across_restart() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let mut registration = EntityRegistration::new("sensor", "demo", "stable-unique-id");
    registration.suggested_object_id = Some("Desk Temperature".into());
    let first = registries
        .get_or_create_entity(registration.clone())
        .unwrap();
    let repeated = registries
        .get_or_create_entity(registration.clone())
        .unwrap();
    assert_eq!(first.id, repeated.id);
    assert_eq!(first.entity_id, "sensor.desk_temperature");
    assert_eq!(registries.entities().len(), 1);
    let device_registration = DeviceRegistration {
        config_entry_id: Some("demo-entry".into()),
        identifiers: BTreeSet::from([("serial".into(), "removed-device".into())]),
        ..DeviceRegistration::default()
    };
    let device = registries
        .get_or_create_device(device_registration.clone())
        .unwrap();
    registries.remove_device(&device.id).unwrap();

    drop(registries);
    drop(runtime);
    let restarted_runtime = Runtime::new(store).unwrap();
    let restarted = Registries::new(restarted_runtime).unwrap();
    let restored = restarted.get_or_create_entity(registration).unwrap();
    assert_eq!(restored.id, first.id);
    assert_eq!(restored.entity_id, first.entity_id);
    assert_eq!(restarted.entities().len(), 1);
    let restored_device = restarted.get_or_create_device(device_registration).unwrap();
    assert_eq!(restored_device.id, device.id);
}

#[test]
fn validates_cross_references_and_cleans_them_on_deletion() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime).unwrap();

    let invalid_area = AreaInput {
        name: "No Floor".into(),
        floor_id: Some("missing-floor".into()),
        ..AreaInput::default()
    };
    assert!(matches!(
        registries.create_area(invalid_area),
        Err(Error::InvalidInput(_))
    ));
    assert!(registries.areas().is_empty());

    let floor = registries
        .create_floor(FloorInput::new("First Floor"))
        .unwrap();
    let label = registries.create_label(LabelInput::new("Climate")).unwrap();
    let area = registries
        .create_area(AreaInput {
            name: "Office".into(),
            floor_id: Some(floor.floor_id.clone()),
            labels: BTreeSet::from([label.label_id.clone()]),
            ..AreaInput::default()
        })
        .unwrap();
    let device = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "abc".into())]),
            area_id: Some(area.id.clone()),
            labels: BTreeSet::from([label.label_id.clone()]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let entity = registries
        .get_or_create_entity(EntityRegistration {
            device_id: Some(device.id.clone()),
            area_id: Some(area.id.clone()),
            labels: BTreeSet::from([label.label_id.clone()]),
            original_device_class: Some("temperature".into()),
            ..EntityRegistration::new("sensor", "demo", "office-temperature")
        })
        .unwrap();
    registries
        .update_area(
            &area.id,
            AreaUpdate {
                temperature_entity_id: Some(Some(entity.entity_id.clone())),
                ..AreaUpdate::default()
            },
        )
        .unwrap();

    registries.delete_area(&area.id).unwrap();
    assert_eq!(registries.entity(&entity.entity_id).unwrap().area_id, None);
    assert_eq!(registries.device(&device.id).unwrap().area_id, None);
    registries.delete_label(&label.label_id).unwrap();
    assert!(
        registries
            .entity(&entity.entity_id)
            .unwrap()
            .labels
            .is_empty()
    );
    assert!(registries.device(&device.id).unwrap().labels.is_empty());
    registries.delete_floor(&floor.floor_id).unwrap();
    registries.remove_device(&device.id).unwrap();
    assert_eq!(
        registries.entity(&entity.entity_id).unwrap().device_id,
        None
    );
    registries.remove_entity(&entity.entity_id).unwrap();
    let restored = registries
        .get_or_create_entity(EntityRegistration::new(
            "sensor",
            "demo",
            "office-temperature",
        ))
        .unwrap();
    assert_eq!(restored.id, entity.id);
}

#[test]
fn identity_collisions_reuse_matching_devices_and_reject_ambiguous_matches() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime).unwrap();
    let first = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "first".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let same = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "first".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(first.id, same.id);
    let second = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "second".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let ambiguous = registries.get_or_create_device(DeviceRegistration {
        identifiers: BTreeSet::from([
            ("serial".into(), "first".into()),
            ("serial".into(), "second".into()),
        ]),
        ..DeviceRegistration::default()
    });
    assert!(matches!(ambiguous, Err(Error::InvalidInput(_))));
    assert_eq!(registries.devices().len(), 2);
    assert_ne!(first.id, second.id);

    registries.remove_device(&first.id).unwrap();
    let restored = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "first".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(restored.id, first.id);

    let linked_left = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("integration-a".into()),
            identifiers: BTreeSet::from([("serial".into(), "shared".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let linked_right = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("integration-b".into()),
            identifiers: BTreeSet::from([("serial".into(), "shared".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_ne!(linked_left.id, linked_right.id);
    let linked = registries
        .handle_ws(&json!({
            "type": "config/device_registry/list_linked_devices",
            "device_id": linked_left.id
        }))
        .unwrap();
    assert_eq!(linked["linked_devices"][0], linked_right.id);
}

#[test]
fn update_events_use_native_event_names_and_old_value_payloads() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let mut events = runtime.subscribe();

    let entity = registries
        .get_or_create_entity(EntityRegistration::new("sensor", "demo", "event-id"))
        .unwrap();
    assert_eq!(events.try_recv().unwrap().data["action"], "create");
    registries
        .update_entity(
            &entity.entity_id,
            EntityUpdate {
                name: Some(Some("Office temperature".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(event.event_type, "entity_registry_updated");
    assert_eq!(event.data["changes"]["name"], Value::Null);

    let renamed = registries
        .update_entity(
            &entity.entity_id,
            EntityUpdate {
                new_entity_id: Some("sensor.office_temperature".into()),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(event.data["entity_id"], renamed.entity_id);
    assert_eq!(event.data["old_entity_id"], entity.entity_id);
    assert_eq!(event.data["changes"]["entity_id"], entity.entity_id);

    let device = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "event-device".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let _ = events.try_recv().unwrap();
    registries
        .update_device(
            &device.id,
            DeviceUpdate {
                name_by_user: Some(Some("Desk".into())),
                ..DeviceUpdate::default()
            },
        )
        .unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(event.event_type, "device_registry_updated");
    assert_eq!(event.data["changes"]["name_by_user"], Value::Null);

    let area = registries.create_area(AreaInput::new("Office")).unwrap();
    let _ = events.try_recv().unwrap();
    registries
        .update_area(
            &area.id,
            AreaUpdate {
                name: Some("Study".into()),
                ..AreaUpdate::default()
            },
        )
        .unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(event.event_type, "area_registry_updated");
    assert_eq!(event.data["action"], "update");
    assert_eq!(event.data["area_id"], area.id);
    assert_eq!(event.data.len(), 2);
}

#[test]
fn websocket_commands_match_registry_result_shapes_and_reject_unknown_fields() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime).unwrap();
    let floor = registries
        .handle_ws(
            &json!({"id": 1, "type": "config/floor_registry/create", "name": "Ground Floor"}),
        )
        .unwrap();
    let floor_id = floor["floor_id"].as_str().unwrap();
    assert_eq!(floor["name"], "Ground Floor");
    let area = registries
        .handle_ws(&json!({"id": 2, "type": "config/area_registry/create", "name": "Kitchen", "floor_id": floor_id}))
        .unwrap();
    let area_id = area["area_id"].as_str().unwrap();
    assert_eq!(area["floor_id"], floor_id);
    let listed = registries
        .handle_ws(&json!({"id": 3, "type": "config/area_registry/list"}))
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["area_id"], area_id);
    let deleted = registries
        .handle_ws(&json!({"id": 4, "type": "config/area_registry/delete", "area_id": area_id}))
        .unwrap();
    assert_eq!(deleted, Value::String("success".into()));
    assert!(matches!(
        registries.handle_ws(&json!({"type": "config/label_registry/list", "unexpected": true})),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn websocket_events_follow_successful_persistence_and_failed_writes_are_atomic() {
    let store = Arc::new(FaultStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let mut events = runtime.subscribe();

    store.fail_next_set.store(true, Ordering::SeqCst);
    assert!(matches!(
        registries.create_label(LabelInput::new("Unavailable")),
        Err(Error::Storage(_))
    ));
    assert!(registries.labels().is_empty());
    assert!(events.try_recv().is_err());

    store.fail_event.store(true, Ordering::SeqCst);
    assert!(matches!(
        registries.create_label(LabelInput::new("No Event")),
        Err(Error::Storage(_))
    ));
    assert!(registries.labels().is_empty());
    assert!(events.try_recv().is_err());

    let created = registries.create_label(LabelInput::new("Ready")).unwrap();
    let event = events.try_recv().unwrap();
    assert_eq!(event.event_type, "label_registry_updated");
    assert_eq!(event.data["action"], "create");
    assert_eq!(event.data["label_id"], created.label_id);
    assert_eq!(registries.labels().len(), 1);
}

#[derive(Default)]
struct FaultStore {
    inner: MemoryStore,
    fail_next_set: AtomicBool,
    fail_event: AtomicBool,
}

impl Store for FaultStore {
    fn load_states(&self) -> hass_core::Result<Vec<State>> {
        self.inner.load_states()
    }

    fn put_state(&self, state: &State, event: Option<&Event>) -> hass_core::Result<()> {
        self.inner.put_state(state, event)
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> hass_core::Result<()> {
        self.inner.remove_state(entity_id, event)
    }

    fn append_event(&self, event: &Event) -> hass_core::Result<()> {
        if self.fail_event.swap(false, Ordering::SeqCst) {
            return Err(Error::Storage("injected event failure".into()));
        }
        self.inner.append_event(event)
    }

    fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
        self.inner.get_data(key)
    }

    fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
        if self.fail_next_set.swap(false, Ordering::SeqCst) {
            return Err(Error::Storage("injected metadata failure".into()));
        }
        self.inner.set_data(key, value)
    }

    fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
        self.inner.delete_data(key)
    }

    fn repack(&self) -> hass_core::Result<()> {
        self.inner.repack()
    }

    fn history_query(
        &self,
        query: &hass_core::HistoryQuery,
    ) -> hass_core::Result<Vec<hass_core::HistorySeries>> {
        self.inner.history_query(query)
    }
    fn event_history_query(
        &self,
        query: &hass_core::EventHistoryQuery,
    ) -> hass_core::Result<Vec<hass_core::Event>> {
        self.inner.event_history_query(query)
    }
    fn purge_before(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> hass_core::Result<hass_core::PurgeResult> {
        self.inner.purge_before(cutoff)
    }

    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> hass_core::Result<Vec<State>> {
        self.inner.history(entity_id, start, end)
    }
}
