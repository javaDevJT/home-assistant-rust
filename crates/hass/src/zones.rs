//! App ownership for YAML zone reload and live core-location updates.

use std::{path::PathBuf, sync::Arc};

use hass_auth::AuthManager;
use hass_config::ConfigEntries;
use hass_core::{Error, Result, Runtime, ServiceResult};
use hass_zone::{HomeZoneConfig, ZoneConfig, ZoneIntegration, ZoneManager};
use serde_json::json;
use tokio::{sync::watch, task::JoinHandle};

pub struct Zones {
    runtime: Runtime,
    integration: Option<ZoneIntegration>,
    manager: ZoneManager,
    owner: String,
    // Service closures hold only a weak reference to this authorization lease.
    _auth: Arc<AuthManager>,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

impl Zones {
    pub async fn start(
        runtime: Runtime,
        auth: AuthManager,
        entries: ConfigEntries,
        path: PathBuf,
        zones: Vec<ZoneConfig>,
    ) -> Result<Self> {
        let mut events = runtime.subscribe();
        let home = entries.core_config();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            HomeZoneConfig {
                name: home.location_name,
                latitude: home.latitude,
                longitude: home.longitude,
                zones,
            },
        )
        .await?;
        let manager = integration.manager();
        let owner = format!("zone.reload.{}", uuid::Uuid::new_v4().simple());
        let auth = Arc::new(auth);
        let authentication = Arc::downgrade(&auth);
        let reload_manager = manager.clone();
        runtime.register_owned_service(
            &owner,
            "zone",
            "reload",
            json!({"description":"Reload YAML zones from configuration","fields":{}}),
            Arc::new(move |_, call| {
                if let Some(user_id) = call.context.user_id.as_deref() {
                    let auth = authentication
                        .upgrade()
                        .ok_or_else(|| Error::Service("Zone reload is stopped".into()))?;
                    let user = auth.get_user(user_id)?;
                    if !user.is_active || (!user.is_owner && !user.is_admin) {
                        return Err(Error::InvalidInput(
                            "Administrator privileges are required".into(),
                        ));
                    }
                }
                if !call.data.is_empty() || !call.target.is_empty() {
                    return Err(Error::InvalidInput("Zone reload takes no fields".into()));
                }
                let zones = crate::config::Config::load_zones(&path).map_err(|error| {
                    Error::InvalidInput(format!("Cannot reload zones: {error}"))
                })?;
                reload_manager.reload(zones)?;
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: None,
                    context: call.context.clone(),
                })
            }),
        )?;
        let (cancel, mut cancellation) = watch::channel(false);
        let live_manager = manager.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.changed() => break,
                    event = events.recv() => {
                        let event = event.map_err(|error| Error::Service(format!("Zone core-config listener failed: {error}")))?;
                        if event.event_type != "core_config_updated" {
                            continue;
                        }
                        // Read the latest accepted config, so queued notifications
                        // cannot roll a newer location back to an older payload.
                        let home = entries.core_config();
                        let manager = live_manager.clone();
                        tokio::task::spawn_blocking(move || {
                            manager.update_home(home.location_name, home.latitude, home.longitude)
                        })
                        .await
                        .map_err(|_| Error::Service("Zone location update task failed".into()))??;
                    }
                }
            }
            Ok(())
        });
        Ok(Self {
            runtime,
            integration: Some(integration),
            manager,
            owner,
            _auth: auth,
            cancel,
            task: Some(task),
        })
    }

    pub fn manager(&self) -> ZoneManager {
        self.manager.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.cancel.send_replace(true);
        let unregister = self.runtime.unregister_owned_services(&self.owner);
        let listener = if let Some(task) = self.task.take() {
            task.await
                .map_err(|_| Error::Service("Zone config listener join failed".into()))
                .and_then(|result| result)
        } else {
            Ok(())
        };
        let integration = if let Some(integration) = self.integration.take() {
            integration.shutdown().await
        } else {
            Ok(())
        };
        unregister?;
        listener?;
        integration
    }
}

impl Drop for Zones {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        let _ = self.runtime.unregister_owned_services(&self.owner);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{Context, MemoryStore, ServiceCall};

    #[tokio::test]
    async fn shutdown_releases_service_and_runtime_with_retained_manager() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.yaml");
        std::fs::write(&path, "zone: []\n").unwrap();
        let zones = Zones::start(runtime.clone(), auth, entries, path, Vec::new())
            .await
            .unwrap();
        let retained = zones.manager();
        zones.shutdown().await.unwrap();
        assert!(runtime.services().get("zone").is_none());
        assert!(
            retained
                .create(
                    serde_json::from_value(json!({
                        "name":"Stopped","latitude":0,"longitude":0
                    }))
                    .unwrap()
                )
                .is_err()
        );
        drop(runtime);
        Runtime::new(store).expect("stopped zone host must release the writer lease");
    }

    #[tokio::test]
    async fn reload_checks_internal_user_authority_and_reads_only_zone_component() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "zone-host-owner-fixture")
            .unwrap();
        let user = auth
            .create_user_with_username(&owner.id, "user", "User", "zone-host-user-fixture", false)
            .unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.yaml");
        std::fs::write(&path,"zone:\n  - name: Work\n    latitude: 42\n    longitude: -83\ninput_number:\n  unrelated:\n    min: 10\n    max: 0\n").unwrap();
        let zones = Zones::start(runtime.clone(), auth, entries, path, Vec::new())
            .await
            .unwrap();
        let call = |user_id| ServiceCall {
            domain: "zone".into(),
            service: "reload".into(),
            data: Default::default(),
            target: Default::default(),
            context: Context {
                user_id: Some(user_id),
                ..Context::default()
            },
        };
        assert!(runtime.call_service(call(user.id)).is_err());
        assert!(runtime.get_state("zone.work").is_none());
        runtime.call_service(call(owner.id)).unwrap();
        assert_eq!(runtime.get_state("zone.work").unwrap().state, "0");
        zones.shutdown().await.unwrap();
    }
}
