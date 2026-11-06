//! Person integration ownership and authorized component-only YAML reload.

use std::{path::PathBuf, sync::Arc};

use hass_auth::AuthManager;
use hass_core::{Error, Result, Runtime, ServiceResult};
use hass_person::{
    PersonConfig, PersonEntityResolver, PersonIntegration, PersonManager, PersonUserValidator,
};
use hass_registry::{EntityRegistration, Registries};
use serde_json::json;

pub struct Persons {
    runtime: Runtime,
    integration: Option<PersonIntegration>,
    manager: PersonManager,
    owner: String,
    _auth: Arc<AuthManager>,
}

impl Persons {
    pub async fn start(
        runtime: Runtime,
        auth: AuthManager,
        registries: Registries,
        path: PathBuf,
        people: Vec<PersonConfig>,
    ) -> Result<Self> {
        let auth = Arc::new(auth);
        let users = Arc::downgrade(&auth);
        let validate_user: PersonUserValidator = Arc::new(move |id| {
            let auth = users
                .upgrade()
                .ok_or_else(|| Error::Service("Person user provider is stopped".into()))?;
            match auth.get_user(id) {
                Ok(_) => Ok(true),
                Err(Error::NotFound(_)) => Ok(false),
                Err(error) => Err(error),
            }
        });
        let resolve_entity: PersonEntityResolver = Arc::new(move |person| {
            let mut registration = EntityRegistration::new("person", "person", &person.id);
            registration.suggested_object_id = Some(person.name.clone());
            registration.original_name = Some(person.name.clone());
            let entry = registries.get_or_create_entity(registration)?;
            Ok(entry.disabled_by.is_none().then_some(entry.entity_id))
        });
        let integration =
            PersonIntegration::start(runtime.clone(), people, validate_user, resolve_entity)
                .await?;
        let manager = integration.manager();
        let owner = format!("person.reload.{}", uuid::Uuid::new_v4().simple());
        let authentication = Arc::downgrade(&auth);
        let reload = manager.clone();
        runtime.register_owned_service(
            &owner,
            "person",
            "reload",
            json!({"description":"Reload YAML people from configuration","fields":{}}),
            Arc::new(move |_, call| {
                if let Some(user_id) = call.context.user_id.as_deref() {
                    let auth = authentication
                        .upgrade()
                        .ok_or_else(|| Error::Service("Person reload is stopped".into()))?;
                    let user = auth.get_user(user_id)?;
                    if !user.is_active || (!user.is_owner && !user.is_admin) {
                        return Err(Error::InvalidInput(
                            "Administrator privileges are required".into(),
                        ));
                    }
                }
                if !call.data.is_empty() || !call.target.is_empty() {
                    return Err(Error::InvalidInput("Person reload takes no fields".into()));
                }
                let people = crate::config::Config::load_people(&path).map_err(|error| {
                    Error::InvalidInput(format!("Cannot reload people: {error}"))
                })?;
                reload.reload_yaml(people)?;
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: None,
                    context: call.context.clone(),
                })
            }),
        )?;
        Ok(Self {
            runtime,
            integration: Some(integration),
            manager,
            owner,
            _auth: auth,
        })
    }

    pub fn manager(&self) -> PersonManager {
        self.manager.clone()
    }

    pub async fn mark_started(&self) -> Result<()> {
        let manager = self.manager.clone();
        tokio::task::spawn_blocking(move || manager.mark_started())
            .await
            .map_err(|_| Error::Service("Person startup task failed".into()))?
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let unregister = self.runtime.unregister_owned_services(&self.owner);
        let stopped = if let Some(integration) = self.integration.take() {
            integration.shutdown().await
        } else {
            Ok(())
        };
        unregister?;
        stopped
    }
}

impl Drop for Persons {
    fn drop(&mut self) {
        let _ = self.runtime.unregister_owned_services(&self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;

    #[tokio::test]
    async fn shutdown_releases_user_registry_callbacks_and_runtime_lease() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.yaml");
        std::fs::write(&path, "person: []\n").unwrap();
        let people = Persons::start(runtime.clone(), auth, registries, path, Vec::new())
            .await
            .unwrap();
        let retained = people.manager();
        people.mark_started().await.unwrap();
        people.shutdown().await.unwrap();
        assert!(runtime.services().get("person").is_none());
        assert!(retained.list().is_err());
        drop(runtime);
        Runtime::new(store)
            .expect("person callbacks must release their registry/auth/runtime leases");
    }
}
