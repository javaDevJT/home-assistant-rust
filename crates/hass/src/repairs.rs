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
        // Install both actual cleanup consumers before replaying committed
        // removals whose previous registry/issue write failed or was interrupted.
        entries.replay_pending_cleanup()?;
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
    use hass_core::{MemoryStore, Store};
    use hass_registry::EntityRegistration;
    use hass_repairs::IssueDraft;
    use serde_json::Map;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailingRegistryStore {
        inner: hass_storage::SqliteStore,
        fail_next_registry: AtomicBool,
    }

    impl Store for FailingRegistryStore {
        fn load_states(&self) -> hass_core::Result<Vec<hass_core::State>> {
            self.inner.load_states()
        }
        fn put_state(
            &self,
            state: &hass_core::State,
            event: Option<&hass_core::Event>,
        ) -> hass_core::Result<()> {
            self.inner.put_state(state, event)
        }
        fn put_state_with_history(
            &self,
            state: &hass_core::State,
            history: &hass_core::State,
            event: &hass_core::Event,
        ) -> hass_core::Result<()> {
            self.inner.put_state_with_history(state, history, event)
        }
        fn remove_state(&self, id: &str, event: &hass_core::Event) -> hass_core::Result<()> {
            self.inner.remove_state(id, event)
        }
        fn append_event(&self, event: &hass_core::Event) -> hass_core::Result<()> {
            self.inner.append_event(event)
        }
        fn get_data(&self, key: &str) -> hass_core::Result<Option<serde_json::Value>> {
            self.inner.get_data(key)
        }
        fn set_data(&self, key: &str, value: &serde_json::Value) -> hass_core::Result<()> {
            if key == "core.registries" && self.fail_next_registry.swap(false, Ordering::SeqCst) {
                return Err(Error::Storage("injected registry write failure".into()));
            }
            self.inner.set_data(key, value)
        }
        fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
            self.inner.delete_data(key)
        }
        fn history(
            &self,
            id: &str,
            start: chrono::DateTime<chrono::Utc>,
            end: chrono::DateTime<chrono::Utc>,
        ) -> hass_core::Result<Vec<hass_core::State>> {
            self.inner.history(id, start, end)
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
        fn repack(&self) -> hass_core::Result<()> {
            self.inner.repack()
        }
    }

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
    fn failed_registry_cleanup_replays_from_sqlite_after_parent_or_child_removal() {
        for remove_parent in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("cleanup.sqlite");
            let (parent_id, sibling_id, child_entity, sibling_entity, other_entity) = {
                let store = Arc::new(FailingRegistryStore {
                    inner: hass_storage::SqliteStore::open(&database).unwrap(),
                    fail_next_registry: AtomicBool::new(false),
                });
                let runtime = Runtime::new(store.clone()).unwrap();
                let entries = ConfigEntries::new(runtime.clone()).unwrap();
                entries.register_driver(Arc::new(FixtureDriver)).unwrap();
                let resolver = entries.clone();
                let registries = Registries::new_with_config_entry_domain_resolver(
                    runtime.clone(),
                    Arc::new(move |id| resolver.get_entry(id).map(|entry| entry.domain)),
                )
                .unwrap();
                let parent = entries
                    .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
                    .unwrap();
                let other = entries
                    .create_entry(NewConfigEntry::new("fixture", "Other", Map::new()))
                    .unwrap();
                let child = entries
                    .create_subentry(
                        &parent.entry_id,
                        NewConfigSubentry::new("child", "Removed child", Map::new()),
                    )
                    .unwrap();
                let sibling = entries
                    .create_subentry(
                        &parent.entry_id,
                        NewConfigSubentry::new("child", "Sibling", Map::new()),
                    )
                    .unwrap();
                let child_entity = entity(
                    &registries,
                    "child",
                    &parent.entry_id,
                    Some(&child.subentry_id),
                );
                let sibling_entity = entity(
                    &registries,
                    "sibling",
                    &parent.entry_id,
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
                let host = RepairsHost::start(runtime.clone(), entries.clone(), registries.clone())
                    .unwrap();
                let manager = host.manager();
                issue(
                    &manager,
                    "removed",
                    &parent.entry_id,
                    Some(&child.subentry_id),
                );
                store.fail_next_registry.store(true, Ordering::SeqCst);
                let result = if remove_parent {
                    entries.remove_entry(&parent.entry_id)
                } else {
                    entries
                        .remove_subentry(&parent.entry_id, &child.subentry_id)
                        .map(|_| ())
                };
                let error = result.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("injected registry write failure"),
                    "{error}"
                );
                assert_eq!(entries.get_entry(&parent.entry_id).is_none(), remove_parent);
                assert!(!entries.subentry_exists(&parent.entry_id, &child.subentry_id));
                assert!(
                    registries.entity(&child_entity).is_some(),
                    "failed snapshot write must preserve the stale registry row for replay"
                );
                assert!(
                    manager.get_issue("fixture", "removed").unwrap().is_none(),
                    "issue cleanup must still be attempted when registry cleanup fails"
                );
                (
                    parent.entry_id,
                    sibling.subentry_id,
                    child_entity,
                    sibling_entity,
                    other_entity,
                )
            };
            // Every old owner and the file-backed writer have left scope.
            // Reopen actual SQLite; recovery cannot depend on old process memory.
            let runtime = Runtime::new(Arc::new(
                hass_storage::SqliteStore::open(&database).unwrap(),
            ))
            .unwrap();
            let entries = ConfigEntries::new(runtime.clone()).unwrap();
            let resolver = entries.clone();
            let registries = Registries::new_with_config_entry_domain_resolver(
                runtime.clone(),
                Arc::new(move |id| resolver.get_entry(id).map(|entry| entry.domain)),
            )
            .unwrap();
            assert!(registries.entity(&child_entity).is_some());
            let _host =
                RepairsHost::start(runtime.clone(), entries.clone(), registries.clone()).unwrap();
            assert!(registries.entity(&child_entity).is_none());
            assert_eq!(registries.entity(&sibling_entity).is_none(), remove_parent);
            assert!(registries.entity(&other_entity).is_some());
            assert_eq!(
                entries.subentry_exists(&parent_id, &sibling_id),
                !remove_parent
            );
            assert_eq!(runtime.get_state(&child_entity).unwrap().state, "foreign");
            assert_eq!(
                entries.replay_pending_cleanup().unwrap(),
                0,
                "successful replay must durably acknowledge the pending record"
            );
        }
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
