//! Registry effects of config-entry enabled/disabled transitions.

use hass_core::Result;
use std::collections::HashSet;

use crate::{DeviceEntry, DeviceUpdate, EntityUpdate, Registries};

/// Reconcile registry disablers after a config entry's `disabled_by` state changes.
///
/// `is_disabled` is evaluated against current config-entry state, rather than a
/// requested value. `None` means that the entry no longer exists and is treated
/// as enabled. Registry writes go through the normal update APIs so persistence
/// and registry events retain their usual ordering and failure behavior.
pub fn config_entry_disabled_by_changed(
    registries: &Registries,
    entry_id: &str,
    is_disabled: impl Fn(&str) -> Option<bool>,
) -> Result<()> {
    let entry_disabled = is_disabled(entry_id).unwrap_or(false);
    let mut changed_device_ids = HashSet::new();

    // Home Assistant applies this one-entry callback independently. A device
    // shared by several config entries is disabled when the first disabled
    // entry reaches it; subsequent disables preserve that state. Enabling an
    // associated entry clears only the derived config_entry flag, even if
    // another associated config entry remains disabled.
    for device in registries.devices() {
        if !device_is_associated_with(&device, entry_id) {
            continue;
        }

        if entry_disabled && device.disabled_by.is_none() {
            let updated = registries.update_device(
                &device.id,
                DeviceUpdate {
                    disabled_by: Some(Some("config_entry".into())),
                    ..DeviceUpdate::default()
                },
            )?;
            if updated.disabled_by.as_deref() != device.disabled_by.as_deref() {
                changed_device_ids.insert(device.id.clone());
            }
        } else if !entry_disabled && device.disabled_by.as_deref() == Some("config_entry") {
            let updated = registries.update_device(
                &device.id,
                DeviceUpdate {
                    disabled_by: Some(None),
                    ..DeviceUpdate::default()
                },
            )?;
            if updated.disabled_by.as_deref() != device.disabled_by.as_deref() {
                changed_device_ids.insert(device.id.clone());
            }
        }
    }

    // Entity disabled_by is a derived projection. Keep explicit entity
    // disablers intact and give each entity's disabled config entry precedence
    // over its referenced device. A changed device also affects its entities
    // owned by other config entries, matching the device-registry event path.
    for entity in registries.entities() {
        let linked_device_changed = entity
            .device_id
            .as_deref()
            .is_some_and(|device_id| changed_device_ids.contains(device_id));
        let owned_by_changed_entry = entity.config_entry_id.as_deref() == Some(entry_id);
        if (!owned_by_changed_entry && !linked_device_changed)
            || !is_derived_disabler(entity.disabled_by.as_deref())
        {
            continue;
        }

        let entity_entry_disabled = entity
            .config_entry_id
            .as_deref()
            .and_then(&is_disabled)
            .unwrap_or(false);
        let device = entity
            .device_id
            .as_deref()
            .and_then(|device_id| registries.device(device_id));
        let derived = derive_entity_disabled_by(entity_entry_disabled, device.as_ref());
        if entity.disabled_by == derived {
            continue;
        }

        registries.update_entity(
            &entity.entity_id,
            EntityUpdate {
                disabled_by: Some(derived),
                ..EntityUpdate::default()
            },
        )?;
    }

    Ok(())
}

/// Reconcile persisted entity disablers without replaying config-entry device
/// callbacks whose historical order is not available during startup.
///
/// Device `disabled_by` values are already durable registry state and remain
/// untouched. Entity values are derived from the current owning config entry
/// and the referenced device's persisted disabled state.
pub fn synchronize_persisted_entity_disabled_by(
    registries: &Registries,
    is_disabled: impl Fn(&str) -> Option<bool>,
) -> Result<()> {
    for entity in registries.entities() {
        if !is_derived_disabler(entity.disabled_by.as_deref()) {
            continue;
        }

        let entry_disabled = entity
            .config_entry_id
            .as_deref()
            .and_then(&is_disabled)
            .unwrap_or(false);
        let device = entity
            .device_id
            .as_deref()
            .and_then(|device_id| registries.device(device_id));
        let derived = derive_entity_disabled_by(entry_disabled, device.as_ref());
        if entity.disabled_by == derived {
            continue;
        }

        registries.update_entity(
            &entity.entity_id,
            EntityUpdate {
                disabled_by: Some(derived),
                ..EntityUpdate::default()
            },
        )?;
    }

    Ok(())
}

fn device_is_associated_with(device: &DeviceEntry, entry_id: &str) -> bool {
    device.config_entry_id.as_deref() == Some(entry_id)
        || device.config_entries.iter().any(|id| id == entry_id)
}

fn is_derived_disabler(disabled_by: Option<&str>) -> bool {
    matches!(disabled_by, None | Some("config_entry" | "device"))
}

fn derive_entity_disabled_by(entry_disabled: bool, device: Option<&DeviceEntry>) -> Option<String> {
    if entry_disabled {
        Some("config_entry".into())
    } else if device.is_some_and(|device| device.disabled_by.is_some()) {
        Some("device".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use hass_core::{Event, MemoryStore, Runtime};
    use serde_json::{Value, json};

    use super::{
        config_entry_disabled_by_changed, is_derived_disabler,
        synchronize_persisted_entity_disabled_by,
    };
    use crate::{DeviceRegistration, EntityRegistration, Registries};

    fn registry_fixture() -> (Arc<MemoryStore>, Runtime, Registries) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        (store, runtime, registries)
    }

    fn register_device_and_entity(
        registries: &Registries,
        entry_id: &str,
        device_disabled_by: Option<&str>,
        entity_disabled_by: Option<&str>,
    ) -> (String, String) {
        let device = registries
            .get_or_create_device(DeviceRegistration {
                config_entry_id: Some(entry_id.to_owned()),
                identifiers: BTreeSet::from([("test".into(), entry_id.into())]),
                ..DeviceRegistration::default()
            })
            .unwrap();
        if let Some(disabled_by) = device_disabled_by {
            registries
                .update_device(
                    &device.id,
                    crate::DeviceUpdate {
                        disabled_by: Some(Some(disabled_by.into())),
                        ..crate::DeviceUpdate::default()
                    },
                )
                .unwrap();
        }
        let entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some(entry_id.to_owned()),
                device_id: Some(device.id.clone()),
                disabled_by: entity_disabled_by.map(str::to_owned),
                ..EntityRegistration::new("sensor", "test", entry_id)
            })
            .unwrap();
        (device.id, entity.entity_id)
    }

    fn drain_events(events: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    #[test]
    fn disable_and_enable_use_persistent_registry_updates_and_events() {
        let (_store, runtime, registries) = registry_fixture();
        let (device_id, entity_id) = register_device_and_entity(&registries, "entry-a", None, None);
        let mut events = runtime.subscribe();

        config_entry_disabled_by_changed(&registries, "entry-a", |_| Some(true)).unwrap();

        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        let disabled_events = drain_events(&mut events);
        assert_eq!(
            disabled_events
                .iter()
                .filter(|event| event.event_type == "device_registry_updated")
                .count(),
            1
        );
        assert_eq!(
            disabled_events
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            1
        );
        let persisted = runtime.get_data("core.registries").unwrap().unwrap();
        assert_eq!(
            persisted["devices"][0]["disabled_by"],
            json!("config_entry")
        );
        assert_eq!(
            persisted["entities"][0]["disabled_by"],
            json!("config_entry")
        );

        config_entry_disabled_by_changed(&registries, "entry-a", |_| Some(false)).unwrap();

        assert_eq!(registries.device(&device_id).unwrap().disabled_by, None);
        assert_eq!(registries.entity(&entity_id).unwrap().disabled_by, None);
        let enabled_events = drain_events(&mut events);
        assert_eq!(
            enabled_events
                .iter()
                .filter(|event| event.event_type == "device_registry_updated")
                .count(),
            1
        );
        assert_eq!(
            enabled_events
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            1
        );
        let persisted = runtime.get_data("core.registries").unwrap().unwrap();
        assert_eq!(persisted["devices"][0]["disabled_by"], Value::Null);
        assert_eq!(persisted["entities"][0]["disabled_by"], Value::Null);
    }

    #[test]
    fn explicit_entity_and_device_disablers_are_preserved() {
        let (_store, runtime, registries) = registry_fixture();
        let (device_id, user_entity_id) =
            register_device_and_entity(&registries, "entry-a", Some("integration"), Some("user"));
        let integration_entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some("entry-a".into()),
                disabled_by: Some("integration".into()),
                ..EntityRegistration::new("sensor", "test", "entry-a-integration")
            })
            .unwrap();
        let hass_entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some("entry-a".into()),
                disabled_by: Some("hass".into()),
                ..EntityRegistration::new("sensor", "test", "entry-a-hass")
            })
            .unwrap();
        let mut events = runtime.subscribe();

        config_entry_disabled_by_changed(&registries, "entry-a", |_| Some(true)).unwrap();

        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("integration")
        );
        assert_eq!(
            registries
                .entity(&user_entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("user")
        );
        assert_eq!(
            registries
                .entity(&integration_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("integration")
        );
        assert_eq!(
            registries
                .entity(&hass_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("hass")
        );
        assert!(drain_events(&mut events).is_empty());

        assert!(!is_derived_disabler(Some("hass")));
    }

    #[test]
    fn enabling_clears_only_config_entry_and_keeps_entities_disabled_by_device() {
        let (_store, runtime, registries) = registry_fixture();
        let (device_id, entity_id) =
            register_device_and_entity(&registries, "entry-a", Some("user"), Some("config_entry"));
        let mut events = runtime.subscribe();

        config_entry_disabled_by_changed(&registries, "entry-a", |_| Some(false)).unwrap();

        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("user")
        );
        assert_eq!(
            registries
                .entity(&entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
        let emitted = drain_events(&mut events);
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.event_type == "device_registry_updated")
                .count(),
            0
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            1
        );
    }

    #[test]
    fn shared_device_follows_each_entry_callback_without_an_all_entries_gate() {
        let (_store, runtime, initial) = registry_fixture();
        let (device_id, entity_id) = register_device_and_entity(&initial, "entry-a", None, None);
        let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
        snapshot["devices"][0]["config_entries"] = json!(["entry-a", "entry-b"]);
        snapshot["devices"][0]["config_entries_subentries"]["entry-b"] = json!([null]);
        runtime.set_data("core.registries", &snapshot).unwrap();
        drop(initial);
        let registries = Registries::new(runtime.clone()).unwrap();
        let mut events = runtime.subscribe();

        config_entry_disabled_by_changed(&registries, "entry-a", |id| {
            assert_eq!(id, "entry-a");
            Some(true)
        })
        .unwrap();
        config_entry_disabled_by_changed(&registries, "entry-b", |id| {
            assert_eq!(id, "entry-b");
            Some(true)
        })
        .unwrap();
        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );

        // Home Assistant's enable callback clears a config_entry device flag
        // for this entry directly, even if another associated entry is still
        // disabled. This deliberately mirrors the per-entry callback contract.
        config_entry_disabled_by_changed(&registries, "entry-a", |id| {
            assert_eq!(id, "entry-a");
            Some(false)
        })
        .unwrap();
        assert_eq!(registries.device(&device_id).unwrap().disabled_by, None);
        assert_eq!(registries.entity(&entity_id).unwrap().disabled_by, None);
        let emitted = drain_events(&mut events);
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.event_type == "device_registry_updated")
                .count(),
            2
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            2
        );
    }

    #[test]
    fn persisted_entity_sync_uses_device_snapshot_without_replaying_device_callbacks() {
        let (_store, _runtime, registries) = registry_fixture();
        let (device_id, entity_id) = register_device_and_entity(
            &registries,
            "entry-a",
            Some("config_entry"),
            Some("config_entry"),
        );
        let unowned_entity = registries
            .get_or_create_entity(EntityRegistration {
                device_id: Some(device_id.clone()),
                ..EntityRegistration::new("sensor", "test", "no-config-entry")
            })
            .unwrap();

        synchronize_persisted_entity_disabled_by(&registries, |id| {
            assert_eq!(id, "entry-a");
            Some(false)
        })
        .unwrap();

        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
        assert_eq!(
            registries
                .entity(&unowned_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
    }

    #[test]
    fn changed_device_rederives_linked_entities_using_their_own_entries() {
        let (_store, runtime, registries) = registry_fixture();
        let device = registries
            .get_or_create_device(DeviceRegistration {
                config_entry_id: Some("entry-a".into()),
                identifiers: BTreeSet::from([("test".into(), "cross-entry-device".into())]),
                ..DeviceRegistration::default()
            })
            .unwrap();
        let entry_a_entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some("entry-a".into()),
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", "test", "entry-a-linked")
            })
            .unwrap();
        let entry_b_entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some("entry-b".into()),
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", "test", "entry-b-linked")
            })
            .unwrap();
        let unowned_entity = registries
            .get_or_create_entity(EntityRegistration {
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", "test", "unowned-linked")
            })
            .unwrap();
        let explicit_entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some("entry-b".into()),
                device_id: Some(device.id.clone()),
                disabled_by: Some("integration".into()),
                ..EntityRegistration::new("sensor", "test", "explicit-linked")
            })
            .unwrap();
        let mut events = runtime.subscribe();

        config_entry_disabled_by_changed(&registries, "entry-a", |id| Some(id == "entry-a"))
            .unwrap();

        assert_eq!(
            registries
                .device(&device.id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&entry_a_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&entry_b_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
        assert_eq!(
            registries
                .entity(&unowned_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
        assert_eq!(
            registries
                .entity(&explicit_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("integration")
        );
        assert_eq!(
            drain_events(&mut events)
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            3
        );

        config_entry_disabled_by_changed(&registries, "entry-a", |_| Some(false)).unwrap();
        assert_eq!(registries.device(&device.id).unwrap().disabled_by, None);
        assert_eq!(
            registries
                .entity(&entry_a_entity.entity_id)
                .unwrap()
                .disabled_by,
            None
        );
        assert_eq!(
            registries
                .entity(&entry_b_entity.entity_id)
                .unwrap()
                .disabled_by,
            None
        );
        assert_eq!(
            registries
                .entity(&unowned_entity.entity_id)
                .unwrap()
                .disabled_by,
            None
        );
        assert_eq!(
            registries
                .entity(&explicit_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("integration")
        );
        assert_eq!(
            drain_events(&mut events)
                .iter()
                .filter(|event| event.event_type == "entity_registry_updated")
                .count(),
            3
        );
    }
}
