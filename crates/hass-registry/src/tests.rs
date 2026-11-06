use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use hass_core::{Error, Event, MemoryStore, Runtime, State, Store};
use serde_json::{Value, json};

use crate::{
    AreaInput, AreaUpdate, DeviceRegistration, DeviceUpdate, EntityRegistration, EntityUpdate,
    FloorInput, LabelInput, Registries,
};

#[derive(Debug)]
struct LegacyDeviceFixture {
    composite_id: String,
    parent_id: String,
    child_id: String,
    area_id: String,
    label_id: String,
    entity_a_id: String,
    entity_b_id: String,
    unmatched_entity_id: String,
}

fn register_legacy_device_fixture(registries: &Registries) -> LegacyDeviceFixture {
    let label = registries
        .create_label(LabelInput::new("Legacy label"))
        .unwrap();
    let area = registries
        .create_area(AreaInput {
            name: "Legacy area".into(),
            labels: BTreeSet::from([label.label_id.clone()]),
            ..AreaInput::default()
        })
        .unwrap();
    let parent = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "parent".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let composite = registries
        .get_or_create_device(DeviceRegistration {
            entry_type: Some("service".into()),
            config_entry_id: Some("entry-a".into()),
            identifiers: BTreeSet::from([("serial".into(), "legacy-device".into())]),
            connections: BTreeSet::from([("mac".into(), "02:00:00:00:00:01".into())]),
            name: Some("Provider name".into()),
            manufacturer: Some("Original manufacturer".into()),
            via_device_id: Some(parent.id.clone()),
            area_id: Some(area.id.clone()),
            labels: BTreeSet::from([label.label_id.clone()]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    registries
        .update_device(
            &composite.id,
            DeviceUpdate {
                name_by_user: Some(Some("User override".into())),
                ..DeviceUpdate::default()
            },
        )
        .unwrap();
    let child = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-b".into()),
            identifiers: BTreeSet::from([("serial".into(), "child".into())]),
            via_device_id: Some(composite.id.clone()),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let entity_a = registries
        .get_or_create_entity(EntityRegistration {
            config_entry_id: Some("entry-a".into()),
            config_subentry_id: Some("sub-a".into()),
            device_id: Some(composite.id.clone()),
            ..EntityRegistration::new("sensor", "demo", "legacy-entry-a")
        })
        .unwrap();
    let entity_b = registries
        .get_or_create_entity(EntityRegistration {
            config_entry_id: Some("entry-b".into()),
            config_subentry_id: Some("sub-b".into()),
            device_id: Some(composite.id.clone()),
            ..EntityRegistration::new("sensor", "demo", "legacy-entry-b")
        })
        .unwrap();
    let unmatched_entity = registries
        .get_or_create_entity(EntityRegistration {
            config_entry_id: Some("entry-c".into()),
            device_id: Some(composite.id.clone()),
            ..EntityRegistration::new("sensor", "demo", "legacy-entry-unmatched")
        })
        .unwrap();
    LegacyDeviceFixture {
        composite_id: composite.id,
        parent_id: parent.id,
        child_id: child.id,
        area_id: area.id,
        label_id: label.label_id,
        entity_a_id: entity_a.entity_id,
        entity_b_id: entity_b.entity_id,
        unmatched_entity_id: unmatched_entity.entity_id,
    }
}

fn set_legacy_multi_entry_snapshot(runtime: &Runtime, composite_id: &str, version: u8) {
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    snapshot["version"] = json!(version);
    let devices = snapshot["devices"].as_array_mut().unwrap();
    let composite = devices
        .iter_mut()
        .find(|device| device["id"] == composite_id)
        .unwrap();
    composite["config_entry_id"] = json!("entry-a");
    composite["config_subentry_id"] = Value::Null;
    composite["config_entries"] = json!(["entry-a", "entry-b"]);
    composite["config_entries_subentries"] = json!({
        "entry-a": [null, "sub-a", "sub-a-2"],
        "entry-b": ["sub-b"]
    });
    for device in devices {
        if let Some(object) = device.as_object_mut() {
            object.remove("composite_device_id");
            object.remove("composite_primary_config_entry");
            object.remove("split_at");
        }
    }
    runtime.set_data("core.registries", &snapshot).unwrap();
}

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
fn legacy_multi_entry_device_is_split_and_persisted_on_registry_upgrade() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    set_legacy_multi_entry_snapshot(&runtime, &fixture.composite_id, 1);
    drop(registries);

    let registries = Registries::new(runtime.clone()).unwrap();
    let splits = registries.devices_for_composite_device_id(&fixture.composite_id);
    assert_eq!(splits.len(), 2);
    assert!(registries.device(&fixture.composite_id).is_none());
    assert!(
        registries
            .devices_for_composite_device_id("unknown-composite-id")
            .is_empty()
    );
    let split_a = splits
        .iter()
        .find(|device| device.config_entry_id.as_deref() == Some("entry-a"))
        .unwrap();
    let split_b = splits
        .iter()
        .find(|device| device.config_entry_id.as_deref() == Some("entry-b"))
        .unwrap();
    assert_eq!(split_a.config_subentry_id.as_deref(), Some("sub-a"));
    assert_eq!(split_b.config_subentry_id.as_deref(), Some("sub-b"));
    assert!(split_a.config_entries.is_empty());
    assert_eq!(
        split_a.config_entries_subentries,
        BTreeMap::from([(
            "entry-a".into(),
            vec![None, Some("sub-a".into()), Some("sub-a-2".into())]
        )])
    );
    assert_eq!(
        split_a.composite_device_id.as_deref(),
        Some(fixture.composite_id.as_str())
    );
    assert_eq!(
        split_b.composite_primary_config_entry.as_deref(),
        Some("entry-a")
    );
    assert!(split_a.split_at.is_some());
    assert_eq!(split_a.split_at, split_b.split_at);
    assert!(split_a.has_composite_identifiers);
    assert!(split_b.has_composite_identifiers);
    assert_eq!(split_a.name_by_user.as_deref(), Some("User override"));
    assert_eq!(split_a.name.as_deref(), Some("Provider name"));
    assert_eq!(
        split_a.manufacturer.as_deref(),
        Some("Original manufacturer")
    );
    assert_eq!(split_a.entry_type.as_deref(), Some("service"));
    assert_eq!(split_a.area_id.as_deref(), Some(fixture.area_id.as_str()));
    assert_eq!(split_a.labels, BTreeSet::from([fixture.label_id.clone()]));
    assert!(
        split_a
            .identifiers
            .contains(&("serial".into(), "legacy-device".into()))
    );
    assert!(
        split_a
            .connections
            .contains(&("mac".into(), "02:00:00:00:00:01".into()))
    );
    assert_eq!(
        split_a.via_device_id.as_deref(),
        Some(fixture.parent_id.as_str())
    );
    assert_eq!(
        registries
            .entity(&fixture.entity_a_id)
            .unwrap()
            .device_id
            .as_deref(),
        Some(split_a.id.as_str())
    );
    assert_eq!(
        registries
            .entity(&fixture.entity_b_id)
            .unwrap()
            .device_id
            .as_deref(),
        Some(split_b.id.as_str())
    );
    assert_eq!(
        registries
            .entity(&fixture.unmatched_entity_id)
            .unwrap()
            .device_id,
        None
    );
    assert_eq!(
        registries
            .device(&fixture.child_id)
            .unwrap()
            .via_device_id
            .as_deref(),
        Some(split_b.id.as_str())
    );
    let composite_splits = registries.composite_splits();
    assert_eq!(
        composite_splits[&fixture.composite_id].split_ids,
        splits
            .iter()
            .map(|device| device.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        composite_splits[&fixture.composite_id]
            .primary_id
            .as_deref(),
        Some(split_a.id.as_str())
    );
    let snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    assert_eq!(snapshot["version"], 2);
    drop(registries);
    drop(runtime);

    let restarted_runtime = Runtime::new(store).unwrap();
    let restarted = Registries::new(restarted_runtime).unwrap();
    let restarted_splits = restarted.devices_for_composite_device_id(&fixture.composite_id);
    assert_eq!(
        restarted_splits
            .iter()
            .map(|device| device.id.clone())
            .collect::<Vec<_>>(),
        splits
            .iter()
            .map(|device| device.id.clone())
            .collect::<Vec<_>>()
    );
    let listed_splits = restarted
        .handle_ws(&json!({"type":"config/device_registry/list_composite_splits"}))
        .unwrap();
    assert_eq!(
        listed_splits[&fixture.composite_id]["split_ids"],
        json!(composite_splits[&fixture.composite_id].split_ids)
    );
    assert_eq!(
        listed_splits[&fixture.composite_id]["primary_id"],
        json!(split_a.id)
    );
    let listed_devices = restarted
        .handle_ws(&json!({"type":"config/device_registry/list"}))
        .unwrap();
    let listed_split_a = listed_devices
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == split_a.id)
        .unwrap();
    assert_eq!(listed_split_a["composite_device_id"], fixture.composite_id);
    assert_eq!(listed_split_a["composite_primary_config_entry"], "entry-a");
    assert!(listed_split_a["split_at"].is_string());
}

#[test]
fn first_provider_registration_replaces_copied_composite_identities_once() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    set_legacy_multi_entry_snapshot(&runtime, &fixture.composite_id, 1);
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    let composite = snapshot["devices"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|device| device["id"] == fixture.composite_id)
        .unwrap();
    composite["identifiers"] = json!([
        ["serial", "legacy-device"],
        ["serial", "stale-composite-identifier"]
    ]);
    composite["connections"] = json!([["mac", "02:00:00:00:00:01"], ["mac", "02:00:00:00:00:02"]]);
    runtime.set_data("core.registries", &snapshot).unwrap();
    drop(registries);

    let registries = Registries::new(runtime).unwrap();
    let split_a = registries
        .devices_for_composite_device_id(&fixture.composite_id)
        .into_iter()
        .find(|device| device.config_entry_id.as_deref() == Some("entry-a"))
        .unwrap();
    let refreshed = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-a".into()),
            identifiers: BTreeSet::from([
                ("serial".into(), "legacy-device".into()),
                ("serial".into(), "current-device-identifier".into()),
            ]),
            connections: BTreeSet::from([("mac".into(), "02:00:00:00:00:03".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(refreshed.id, split_a.id);
    assert!(!refreshed.has_composite_identifiers);
    assert_eq!(
        refreshed.identifiers,
        BTreeSet::from([
            ("serial".into(), "legacy-device".into()),
            ("serial".into(), "current-device-identifier".into()),
        ])
    );
    assert_eq!(
        refreshed.connections,
        BTreeSet::from([("mac".into(), "02:00:00:00:00:03".into())])
    );

    let refreshed_again = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-a".into()),
            identifiers: BTreeSet::from([
                ("serial".into(), "current-device-identifier".into()),
                ("serial".into(), "next-identifier".into()),
            ]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(refreshed_again.id, split_a.id);
    assert_eq!(
        refreshed_again.identifiers,
        BTreeSet::from([
            ("serial".into(), "legacy-device".into()),
            ("serial".into(), "current-device-identifier".into()),
            ("serial".into(), "next-identifier".into()),
        ])
    );
    assert_eq!(
        refreshed_again.connections,
        BTreeSet::from([("mac".into(), "02:00:00:00:00:03".into())])
    );
    assert!(!refreshed_again.has_composite_identifiers);
}

#[test]
fn migration_maps_composite_parents_by_entry_then_domain_and_detaches_stale_links() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    let same_domain_child = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-c".into()),
            identifiers: BTreeSet::from([("serial".into(), "same-domain-child".into())]),
            via_device_id: Some(fixture.composite_id.clone()),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let dangling_child = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-d".into()),
            identifiers: BTreeSet::from([("serial".into(), "dangling-child".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    set_legacy_multi_entry_snapshot(&runtime, &fixture.composite_id, 1);
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    let devices = snapshot["devices"].as_array_mut().unwrap();
    let composite = devices
        .iter_mut()
        .find(|device| device["id"] == fixture.composite_id)
        .unwrap();
    composite["via_device_id"] = json!(fixture.composite_id);
    let dangling = devices
        .iter_mut()
        .find(|device| device["id"] == dangling_child.id)
        .unwrap();
    dangling["via_device_id"] = json!("missing-parent-id");
    runtime.set_data("core.registries", &snapshot).unwrap();
    drop(registries);

    let resolver: Arc<crate::ConfigEntryDomainResolver> = Arc::new(|entry_id| match entry_id {
        "entry-a" => Some("alpha".into()),
        "entry-b" | "entry-c" => Some("beta".into()),
        _ => None,
    });
    let registries =
        Registries::new_with_config_entry_domain_resolver(runtime, resolver.clone()).unwrap();
    let splits = registries.devices_for_composite_device_id(&fixture.composite_id);
    let split_a = splits
        .iter()
        .find(|device| device.config_entry_id.as_deref() == Some("entry-a"))
        .unwrap();
    let split_b = splits
        .iter()
        .find(|device| device.config_entry_id.as_deref() == Some("entry-b"))
        .unwrap();
    assert_eq!(split_a.via_device_id, None);
    assert_eq!(split_b.via_device_id, None);
    assert_eq!(
        registries
            .device(&fixture.child_id)
            .unwrap()
            .via_device_id
            .as_deref(),
        Some(split_b.id.as_str())
    );
    assert_eq!(
        registries
            .device(&same_domain_child.id)
            .unwrap()
            .via_device_id
            .as_deref(),
        Some(split_b.id.as_str())
    );
    assert_eq!(
        registries.device(&dangling_child.id).unwrap().via_device_id,
        None
    );

    let newly_registered_child = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-c".into()),
            identifiers: BTreeSet::from([("serial".into(), "new-child".into())]),
            via_device_id: Some(fixture.composite_id.clone()),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(
        newly_registered_child.via_device_id.as_deref(),
        Some(split_b.id.as_str())
    );
}

#[test]
fn legacy_deleted_composite_device_migrates_to_independent_owner_tombstones() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    registries.remove_device(&fixture.composite_id).unwrap();
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    snapshot["version"] = json!(1);
    let deleted = snapshot["deleted_devices"].as_array_mut().unwrap();
    let composite = deleted
        .iter_mut()
        .find(|device| device["id"] == fixture.composite_id)
        .unwrap();
    composite["config_entry_id"] = json!("entry-a");
    composite["config_subentry_id"] = Value::Null;
    composite["config_entries"] = json!(["entry-a", "entry-b"]);
    composite["config_entries_subentries"] = json!({
        "entry-a": [null, "sub-a", "sub-a-2"],
        "entry-b": ["sub-b"]
    });
    for device in snapshot["devices"].as_array_mut().unwrap() {
        if let Some(object) = device.as_object_mut() {
            object.remove("composite_device_id");
            object.remove("composite_primary_config_entry");
            object.remove("split_at");
        }
    }
    runtime.set_data("core.registries", &snapshot).unwrap();
    drop(registries);

    let registries = Registries::new(runtime.clone()).unwrap();
    let migrated = runtime.get_data("core.registries").unwrap().unwrap();
    let tombstones = migrated["deleted_devices"].as_array().unwrap();
    assert_eq!(tombstones.len(), 2);
    let deleted_a = tombstones
        .iter()
        .find(|device| device["config_entry_id"] == "entry-a")
        .unwrap();
    let deleted_b = tombstones
        .iter()
        .find(|device| device["config_entry_id"] == "entry-b")
        .unwrap();
    assert_ne!(deleted_a["id"], deleted_b["id"]);
    assert_eq!(
        deleted_a["config_entries_subentries"],
        json!({"entry-a":[null,"sub-a","sub-a-2"]})
    );
    assert!(deleted_a["composite_device_id"].is_null());
    assert!(deleted_b["composite_device_id"].is_null());
    assert_eq!(deleted_a["name_by_user"], "User override");
    assert_eq!(deleted_b["name_by_user"], "User override");
    assert_eq!(deleted_a["area_id"], fixture.area_id);
    assert_eq!(deleted_b["area_id"], fixture.area_id);
    let expected_id_a = deleted_a["id"].as_str().unwrap().to_owned();
    let expected_id_b = deleted_b["id"].as_str().unwrap().to_owned();

    let restored_b = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-b".into()),
            identifiers: BTreeSet::from([("serial".into(), "legacy-device".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let restored_a = registries
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-a".into()),
            identifiers: BTreeSet::from([("serial".into(), "legacy-device".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    assert_eq!(restored_b.id, expected_id_b);
    assert_eq!(restored_a.id, expected_id_a);
    assert_ne!(restored_a.id, restored_b.id);
}

#[test]
fn split_legacy_device_uses_transact_and_emits_registry_lifecycle_events() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    set_legacy_multi_entry_snapshot(&runtime, &fixture.composite_id, 2);
    drop(registries);

    let registries = Registries::new(runtime.clone()).unwrap();
    let mut events = runtime.subscribe();
    let members = registries
        .split_legacy_device(&fixture.composite_id)
        .unwrap();
    let split_ids = members
        .iter()
        .map(|member| member.id.clone())
        .collect::<BTreeSet<_>>();
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    let removed_at = emitted
        .iter()
        .position(|event| {
            event.event_type == "device_registry_updated"
                && event.data["action"] == "remove"
                && event.data["device_id"] == fixture.composite_id
                && event.data["device"]["id"] == fixture.composite_id
        })
        .unwrap();
    let created_ids = emitted
        .iter()
        .filter(|event| {
            event.event_type == "device_registry_updated" && event.data["action"] == "create"
        })
        .map(|event| event.data["device_id"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(created_ids, split_ids);
    let created_at = emitted
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.event_type == "device_registry_updated" && event.data["action"] == "create"
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let references_updated_at = emitted
        .iter()
        .position(|event| {
            (event.event_type == "entity_registry_updated"
                && event.data["entity_id"] == fixture.entity_a_id)
                || (event.event_type == "device_registry_updated"
                    && event.data["device_id"] == fixture.child_id
                    && event.data["changes"]["via_device_id"] == fixture.composite_id)
        })
        .unwrap();
    // The pinned startup migration rewrites the persisted snapshot without
    // publishing events. For the explicit runtime split API, publish removal,
    // replacement creation, then references to the replacement IDs.
    assert!(removed_at < created_at[0]);
    assert!(created_at.last().unwrap() < &references_updated_at);
    assert!(emitted.iter().any(|event| {
        event.event_type == "device_registry_updated"
            && event.data["device_id"] == fixture.child_id
            && event.data["changes"]["via_device_id"] == fixture.composite_id
    }));
    assert!(emitted.iter().any(|event| {
        event.event_type == "entity_registry_updated"
            && event.data["entity_id"] == fixture.entity_a_id
            && event.data["changes"]["device_id"] == fixture.composite_id
    }));
    let repeated = registries
        .split_legacy_device(&fixture.composite_id)
        .unwrap();
    assert_eq!(repeated, members);
    assert!(matches!(
        registries.handle_ws(&json!({
            "type":"config/device_registry/update",
            "device_id":fixture.composite_id,
            "name_by_user":"invalid composite update"
        })),
        Err(Error::NotAllowed(_))
    ));
    assert!(matches!(
        registries.remove_device(&fixture.composite_id),
        Err(Error::NotAllowed(_))
    ));
    assert!(events.try_recv().is_err());
}

#[test]
fn split_legacy_device_failure_leaves_snapshot_and_events_unchanged() {
    let store = Arc::new(FaultStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let fixture = register_legacy_device_fixture(&registries);
    set_legacy_multi_entry_snapshot(&runtime, &fixture.composite_id, 2);
    drop(registries);

    let registries = Registries::new(runtime.clone()).unwrap();
    let mut events = runtime.subscribe();
    let before = runtime.get_data("core.registries").unwrap().unwrap();
    store.fail_next_set.store(true, Ordering::SeqCst);
    assert!(matches!(
        registries.split_legacy_device(&fixture.composite_id),
        Err(Error::Storage(_))
    ));
    assert_eq!(
        runtime.get_data("core.registries").unwrap().unwrap(),
        before
    );
    assert!(registries.device(&fixture.composite_id).is_some());
    assert!(
        registries
            .devices_for_composite_device_id(&fixture.composite_id)
            .is_empty()
    );
    assert!(events.try_recv().is_err());
}

#[test]
fn split_legacy_device_rejects_unknown_or_unowned_devices_without_mutation() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let device = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "unowned".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let mut events = runtime.subscribe();
    assert!(matches!(
        registries.split_legacy_device("missing-composite"),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        registries.split_legacy_device(&device.id),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(registries.device(&device.id), Some(device));
    assert!(events.try_recv().is_err());
}

#[test]
fn registry_rejects_composite_aliases_that_conflict_with_live_device_ids() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let first = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "first".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let second = registries
        .get_or_create_device(DeviceRegistration {
            identifiers: BTreeSet::from([("serial".into(), "second".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    drop(registries);
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    let device = snapshot["devices"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|device| device["id"] == first.id)
        .unwrap();
    device["composite_device_id"] = json!(second.id);
    device["composite_primary_config_entry"] = Value::Null;
    device["split_at"] = json!(Utc::now().to_rfc3339());
    runtime.set_data("core.registries", &snapshot).unwrap();
    assert!(matches!(Registries::new(runtime), Err(Error::Storage(_))));
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
