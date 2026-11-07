//! App-owned Repairs registry and committed config lifecycle cleanup.

use anyhow::Result;
use hass_config::{
    ConfigEntries, ConfigLifecycleEvent, ConfigLifecycleObserver, ConfigLifecycleObserverGuard,
};
use hass_core::{Error, Runtime};
use hass_registry::Registries;
use hass_repairs::{ConfigSubentryResolver, RepairsIntegration, RepairsManager};
use std::sync::Arc;

// Issue version/dismissal semantics follow the pinned compatibility target,
// independently of this fork's native package version.
const COMPATIBILITY_VERSION: &str = "2026.12.0.dev0";

pub struct RepairsHost {
    // Unregister the callback before releasing its weak manager's owner.
    _observer: ConfigLifecycleObserverGuard,
    integration: RepairsIntegration,
}

struct Subentries(ConfigEntries);
impl ConfigSubentryResolver for Subentries {
    fn subentry_exists(&self, entry_id: &str, subentry_id: &str) -> bool {
        self.0.subentry_exists(entry_id, subentry_id)
    }
}

struct Lifecycle {
    repairs: RepairsManager,
    registries: Registries,
}

impl ConfigLifecycleObserver for Lifecycle {
    fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> hass_core::Result<()> {
        let mut failures = Vec::new();
        match event {
            ConfigLifecycleEvent::EntryRemoved { entry_id, domain } => {
                if let Err(error) = self.registries.clear_config_entry(entry_id, domain) {
                    failures.push(format!("registry cleanup: {error}"));
                }
                if let Err(error) = self.repairs.clear_config_entry(entry_id) {
                    failures.push(format!("issue cleanup: {error}"));
                }
            }
            ConfigLifecycleEvent::SubentryRemoved {
                entry_id,
                subentry_id,
            } => {
                if let Err(error) = self.registries.clear_config_subentry(entry_id, subentry_id) {
                    failures.push(format!("registry child cleanup: {error}"));
                }
                if let Err(error) = self.repairs.clear_config_subentry(entry_id, subentry_id) {
                    failures.push(format!("issue child cleanup: {error}"));
                }
            }
            ConfigLifecycleEvent::EntryDisabledByChanged { entry_id, .. } => {
                if let Err(error) = self.repairs.config_entry_disabled_by_changed(entry_id) {
                    failures.push(format!("issue visibility refresh: {error}"));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(failures.join("; ")))
        }
    }
}

impl RepairsHost {
    pub fn start(runtime: Runtime, entries: ConfigEntries, registries: Registries) -> Result<Self> {
        let integration = RepairsIntegration::start_with_resolvers(
            runtime,
            Arc::new(entries.clone()),
            Some(Arc::new(Subentries(entries.clone()))),
            COMPATIBILITY_VERSION,
        )?;
        let observer = entries.observe_lifecycle(Arc::new(Lifecycle {
            repairs: integration.manager(),
            registries,
        }))?;
        Ok(Self {
            _observer: observer,
            integration,
        })
    }

    pub fn manager(&self) -> RepairsManager {
        self.integration.manager()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_config::{
        ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult, NewConfigEntry,
        NewConfigSubentry, SetupOutcome,
    };
    use hass_core::MemoryStore;
    use hass_registry::EntityRegistration;
    use hass_repairs::IssueDraft;
    use serde_json::Map;

    struct FixtureDriver;
    impl ConfigEntryDriver for FixtureDriver {
        fn domain(&self) -> &str {
            "fixture"
        }
        fn setup(
            &self,
            _runtime: &Runtime,
            _entry: &ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }
        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
            Ok(())
        }
        fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form("user", None))
        }
    }

    fn setup() -> (Runtime, ConfigEntries, Registries) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        entries.register_driver(Arc::new(FixtureDriver)).unwrap();
        let resolver = entries.clone();
        let registries = Registries::new_with_config_entry_domain_resolver(
            runtime.clone(),
            Arc::new(move |id| resolver.get_entry(id).map(|entry| entry.domain)),
        )
        .unwrap();
        (runtime, entries, registries)
    }

    fn issue(manager: &RepairsManager, id: &str, entry: &str, child: Option<&str>) {
        let mut draft = IssueDraft::new("fixture", id, false, "warning", "fixture");
        draft.is_persistent = true;
        draft.config_entry_id = Some(entry.into());
        draft.config_subentry_id = child.map(str::to_owned);
        manager.create_issue(draft).unwrap();
    }

    fn entity(registries: &Registries, id: &str, entry: &str, child: Option<&str>) -> String {
        let mut registration = EntityRegistration::new("sensor", "fixture", id);
        registration.suggested_object_id = Some(format!("fixture_{id}"));
        registration.config_entry_id = Some(entry.into());
        registration.config_subentry_id = child.map(str::to_owned);
        registries
            .get_or_create_entity(registration)
            .unwrap()
            .entity_id
    }

    #[test]
    fn committed_entry_and_subentry_changes_update_actual_issue_visibility_and_cleanup() {
        let (runtime, entries, registries) = setup();
        let first = entries
            .create_entry(NewConfigEntry::new("fixture", "First", Map::new()))
            .unwrap();
        let other = entries
            .create_entry(NewConfigEntry::new("fixture", "Other", Map::new()))
            .unwrap();
        let child = entries
            .create_subentry(
                &first.entry_id,
                NewConfigSubentry::new("child", "First child", Map::new()),
            )
            .unwrap();
        let sibling = entries
            .create_subentry(
                &first.entry_id,
                NewConfigSubentry::new("child", "Sibling", Map::new()),
            )
            .unwrap();
        let child_entity = entity(
            &registries,
            "child",
            &first.entry_id,
            Some(&child.subentry_id),
        );
        let sibling_entity = entity(
            &registries,
            "sibling",
            &first.entry_id,
            Some(&sibling.subentry_id),
        );
        let other_entity = entity(&registries, "other", &other.entry_id, None);
        runtime
            .set_state(
                &child_entity,
                "foreign",
                Map::new(),
                hass_core::Context::default(),
                false,
            )
            .unwrap();
        let host =
            RepairsHost::start(runtime.clone(), entries.clone(), registries.clone()).unwrap();
        let manager = host.manager();
        issue(&manager, "child", &first.entry_id, Some(&child.subentry_id));
        issue(
            &manager,
            "sibling",
            &first.entry_id,
            Some(&sibling.subentry_id),
        );
        issue(&manager, "parent", &first.entry_id, None);
        issue(&manager, "other", &other.entry_id, None);
        assert_eq!(manager.list_issues().unwrap().len(), 4);
        entries
            .disable_entry(&first.entry_id, Some("user"))
            .unwrap();
        let visible = manager.list_issues().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].issue_id, "other");
        assert!(manager.get_issue("fixture", "parent").unwrap().is_some());
        entries.disable_entry(&first.entry_id, None).unwrap();
        assert_eq!(manager.list_issues().unwrap().len(), 4);
        entries
            .remove_subentry(&first.entry_id, &child.subentry_id)
            .unwrap();
        assert!(manager.get_issue("fixture", "child").unwrap().is_none());
        assert!(manager.get_issue("fixture", "sibling").unwrap().is_some());
        assert!(registries.entity(&child_entity).is_none());
        assert!(registries.entity(&sibling_entity).is_some());
        assert_eq!(runtime.get_state(&child_entity).unwrap().state, "foreign");
        entries.remove_entry(&first.entry_id).unwrap();
        assert!(registries.entity(&sibling_entity).is_none());
        assert!(registries.entity(&other_entity).is_some());
        let visible = manager.list_issues().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].issue_id, "other");
    }

    #[test]
    fn dropping_host_unregisters_its_observer_before_same_manager_restart() {
        let (runtime, entries, registries) = setup();
        let entry = entries
            .create_entry(NewConfigEntry::new("fixture", "First", Map::new()))
            .unwrap();
        let host =
            RepairsHost::start(runtime.clone(), entries.clone(), registries.clone()).unwrap();
        let manager = host.manager();
        issue(&manager, "retained", &entry.entry_id, None);
        drop(host);
        assert!(manager.list_issues().is_err());
        entries
            .disable_entry(&entry.entry_id, Some("user"))
            .unwrap();
        let host = RepairsHost::start(runtime, entries.clone(), registries).unwrap();
        assert!(host.manager().list_issues().unwrap().is_empty());
        entries.disable_entry(&entry.entry_id, None).unwrap();
        assert_eq!(host.manager().list_issues().unwrap().len(), 1);
        entries.remove_entry(&entry.entry_id).unwrap();
        assert!(host.manager().list_issues().unwrap().is_empty());
    }
}
