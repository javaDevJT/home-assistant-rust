//! App wiring for config-entry changes that derive registry disabled flags.

use hass_config::{
    ConfigEntries, ConfigLifecycleEvent, ConfigLifecycleObserver, ConfigLifecycleObserverGuard,
};
use hass_core::Result;
use hass_registry::{
    Registries,
    config_lifecycle::{
        config_entry_disabled_by_changed, synchronize_persisted_entity_disabled_by,
    },
};
use std::sync::{Arc, Mutex, Weak};

/// Installs registry synchronization for committed config-entry enable/disable
/// transitions and reconciles derived entity flags already persisted at startup.
pub struct RegistryLifecycleHost {
    // Unregister the callback before releasing its weak resolver's owner.
    _observer: ConfigLifecycleObserverGuard,
    _entries: Arc<ConfigEntries>,
}

struct LifecycleObserver {
    entries: Weak<ConfigEntries>,
    registries: Registries,
    transitions: Arc<Mutex<()>>,
}

impl ConfigLifecycleObserver for LifecycleObserver {
    fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()> {
        let ConfigLifecycleEvent::EntryRegistryDisabledByChanged { entry_id } = event else {
            return Ok(());
        };

        let Some(entries) = self.entries.upgrade() else {
            return Ok(());
        };
        // Config callbacks run outside manager locks and may arrive from
        // concurrent API requests. Serialize their registry projections, then
        // resolve the latest committed entry values inside that ordering.
        let _transition = self
            .transitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        config_entry_disabled_by_changed(&self.registries, entry_id, |current_entry_id| {
            entries
                .get_entry(current_entry_id)
                .map(|entry| entry.disabled_by.is_some())
        })
    }
}

impl RegistryLifecycleHost {
    /// Synchronize persisted entity flags, then observe future transitions.
    pub fn start(entries: ConfigEntries, registries: Registries) -> Result<Self> {
        let entries = Arc::new(entries);
        let transitions = Arc::new(Mutex::new(()));
        let observer: Arc<dyn ConfigLifecycleObserver> = Arc::new(LifecycleObserver {
            entries: Arc::downgrade(&entries),
            registries: registries.clone(),
            transitions: Arc::clone(&transitions),
        });
        let guard = entries.observe_lifecycle(observer)?;

        {
            let _transition = transitions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            synchronize_persisted_entity_disabled_by(&registries, |entry_id| {
                entries
                    .get_entry(entry_id)
                    .map(|entry| entry.disabled_by.is_some())
            })?;
        }

        Ok(Self {
            _observer: guard,
            _entries: entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use hass_config::{
        ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
        NewConfigEntry, SetupOutcome,
    };
    use hass_core::{Error, MemoryStore, Runtime};
    use hass_registry::{DeviceRegistration, DeviceUpdate, EntityRegistration, Registries};
    use serde_json::Map;

    use super::RegistryLifecycleHost;

    const DOMAIN: &str = "registry_lifecycle_test";

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RegistrySnapshot {
        phase: &'static str,
        device_disabled_by: Option<String>,
        entity_disabled_by: Option<String>,
    }

    struct ObserveRegistryDriver {
        registries: Registries,
        device_id: String,
        entity_id: String,
        snapshots: Arc<Mutex<Vec<RegistrySnapshot>>>,
    }

    impl ObserveRegistryDriver {
        fn record(&self, phase: &'static str) {
            self.snapshots.lock().unwrap().push(RegistrySnapshot {
                phase,
                device_disabled_by: self
                    .registries
                    .device(&self.device_id)
                    .and_then(|device| device.disabled_by),
                entity_disabled_by: self
                    .registries
                    .entity(&self.entity_id)
                    .and_then(|entity| entity.disabled_by),
            });
        }
    }

    impl ConfigEntryDriver for ObserveRegistryDriver {
        fn domain(&self) -> &str {
            DOMAIN
        }

        fn setup(
            &self,
            _runtime: &Runtime,
            _entry: &ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            self.record("setup");
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
            self.record("unload");
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            Err(Error::InvalidInput("test driver has no config flow".into()))
        }
    }

    fn runtime_and_registries() -> (Runtime, Registries) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        (runtime, registries)
    }

    fn register_entity(registries: &Registries, entry_id: &str) -> (String, String) {
        let device = registries
            .get_or_create_device(DeviceRegistration {
                config_entry_id: Some(entry_id.into()),
                identifiers: BTreeSet::from([("test".into(), entry_id.into())]),
                ..DeviceRegistration::default()
            })
            .unwrap();
        let entity = registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some(entry_id.into()),
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", DOMAIN, "temperature")
            })
            .unwrap();
        (device.id, entity.entity_id)
    }

    #[test]
    fn committed_transition_observer_matches_unload_and_setup_order() {
        let (runtime, registries) = runtime_and_registries();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let entry = entries
            .create_entry(NewConfigEntry::new(
                DOMAIN,
                "Registry lifecycle",
                Map::new(),
            ))
            .unwrap();
        let (device_id, entity_id) = register_entity(&registries, &entry.entry_id);
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        entries
            .register_driver(Arc::new(ObserveRegistryDriver {
                registries: registries.clone(),
                device_id: device_id.clone(),
                entity_id: entity_id.clone(),
                snapshots: Arc::clone(&snapshots),
            }))
            .unwrap();
        entries.setup_entry(&entry.entry_id).unwrap();
        let _host = RegistryLifecycleHost::start(entries.clone(), registries.clone()).unwrap();
        snapshots.lock().unwrap().clear();

        assert!(
            entries
                .disable_entry(&entry.entry_id, Some("user"))
                .unwrap()
        );
        let unload = snapshots
            .lock()
            .unwrap()
            .iter()
            .find(|snapshot| snapshot.phase == "unload")
            .cloned()
            .unwrap();
        assert_eq!(unload.device_disabled_by, None);
        assert_eq!(unload.entity_disabled_by, None);
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

        assert!(entries.disable_entry(&entry.entry_id, None).unwrap());
        let setup = snapshots
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|snapshot| snapshot.phase == "setup")
            .cloned()
            .unwrap();
        assert_eq!(setup.device_disabled_by, None);
        assert_eq!(setup.entity_disabled_by, None);
    }

    #[test]
    fn startup_reconciles_persisted_disabled_entries() {
        let (runtime, registries) = runtime_and_registries();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let entry = entries
            .create_entry(NewConfigEntry::new(DOMAIN, "Persisted", Map::new()))
            .unwrap();
        let (device_id, entity_id) = register_entity(&registries, &entry.entry_id);
        entries
            .disable_entry(&entry.entry_id, Some("user"))
            .unwrap();

        let _host = RegistryLifecycleHost::start(entries, registries.clone()).unwrap();

        assert_eq!(
            registries
                .device(&device_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            None
        );
        assert_eq!(
            registries
                .entity(&entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
    }

    #[test]
    fn startup_preserves_persisted_shared_device_disabler_history() {
        let (runtime, initial_registries) = runtime_and_registries();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let disabled_entry = entries
            .create_entry(NewConfigEntry::new(DOMAIN, "Disabled owner", Map::new()))
            .unwrap();
        let enabled_entry = entries
            .create_entry(NewConfigEntry::new(DOMAIN, "Enabled owner", Map::new()))
            .unwrap();
        let disabled_entry_id = disabled_entry.entry_id.clone();
        let enabled_entry_id = enabled_entry.entry_id.clone();
        let device = initial_registries
            .get_or_create_device(DeviceRegistration {
                config_entry_id: Some(disabled_entry.entry_id.clone()),
                identifiers: BTreeSet::from([("test-shared".into(), "shared-device".into())]),
                ..DeviceRegistration::default()
            })
            .unwrap();
        let disabled_owner_entity = initial_registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some(disabled_entry.entry_id.clone()),
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", DOMAIN, "disabled-owner")
            })
            .unwrap();
        let enabled_owner_entity = initial_registries
            .get_or_create_entity(EntityRegistration {
                config_entry_id: Some(enabled_entry.entry_id.clone()),
                device_id: Some(device.id.clone()),
                ..EntityRegistration::new("sensor", DOMAIN, "enabled-owner")
            })
            .unwrap();
        initial_registries
            .update_device(
                &device.id,
                DeviceUpdate {
                    disabled_by: Some(Some("config_entry".into())),
                    ..DeviceUpdate::default()
                },
            )
            .unwrap();
        let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
        snapshot["devices"][0]["config_entries"] =
            serde_json::json!([disabled_entry_id.clone(), enabled_entry_id.clone()]);
        snapshot["devices"][0]["config_entries_subentries"][&enabled_entry_id] =
            serde_json::json!([null]);
        runtime.set_data("core.registries", &snapshot).unwrap();
        drop(initial_registries);

        let registries = Registries::new(runtime.clone()).unwrap();
        entries
            .disable_entry(&disabled_entry_id, Some("user"))
            .unwrap();
        drop(entries);
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let mut events = runtime.subscribe();

        let _host = RegistryLifecycleHost::start(entries, registries.clone()).unwrap();

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
                .entity(&disabled_owner_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("config_entry")
        );
        assert_eq!(
            registries
                .entity(&enabled_owner_entity.entity_id)
                .unwrap()
                .disabled_by
                .as_deref(),
            Some("device")
        );
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.event_type == "device_registry_updated")
                .count(),
            0
        );
    }
}
