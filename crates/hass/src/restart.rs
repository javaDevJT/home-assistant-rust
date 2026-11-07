//! Native ownership for the `homeassistant.restart` service.
//!
//! The service requests an application restart; the main loop remains
//! responsible for stopping and joining every component before replacing the
//! process. This module never restarts the operating system.

use hass_auth::AuthManager;
use hass_core::{Error, Result, Runtime, ServiceCall, ServiceResult};
use serde_json::json;
use std::{process::Child, process::Command, sync::Arc};
use tokio::sync::watch;

const DOMAIN: &str = "homeassistant";
const SERVICE_RESTART: &str = "restart";

/// Owns the restart service and its one-shot application restart request.
pub struct RestartHost {
    runtime: Runtime,
    owner: String,
    _auth: Arc<AuthManager>,
    requested: watch::Sender<bool>,
}

impl RestartHost {
    /// Register the administrator-authorized `homeassistant.restart` service.
    ///
    /// Contexts without a user ID are treated as internal system calls, as in
    /// the existing service authorization pattern. The service accepts no
    /// target and no data except the pinned service's optional
    /// `safe_mode: false`; safe mode and other restart options are unsupported.
    pub fn start(runtime: Runtime, auth: &AuthManager) -> Result<Self> {
        let owner = format!("homeassistant.restart.{}", uuid::Uuid::new_v4().simple());
        let auth = Arc::new(auth.clone());
        let authorization = Arc::downgrade(&auth);
        let (requested, _) = watch::channel(false);
        let request = requested.clone();

        runtime.register_owned_service(
            &owner,
            DOMAIN,
            SERVICE_RESTART,
            json!({
                "description": "Gracefully stop and restart this native Home Assistant application",
                "fields": {
                    "safe_mode": {
                        "description": "Only false is supported; safe mode is not implemented",
                        "required": false,
                        "type": "boolean"
                    }
                }
            }),
            Arc::new(move |_, call| {
                authorize_restart(&authorization, call)?;
                validate_restart_call(call)?;
                request.send_replace(true);
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: None,
                    context: call.context.clone(),
                })
            }),
        )?;

        Ok(Self {
            runtime,
            owner,
            _auth: auth,
            requested,
        })
    }

    /// Return a receiver suitable for the main loop's graceful-shutdown select.
    ///
    /// The receiver starts with `false`; after the service succeeds it becomes
    /// `true` and stays true until the process exits.
    pub fn receiver(&self) -> watch::Receiver<bool> {
        self.requested.subscribe()
    }

    /// Whether the service has already requested a restart.
    pub fn requested(&self) -> bool {
        *self.requested.borrow()
    }

    /// Unregister this host's service and return the number of registrations
    /// removed. The UUID-scoped owner leaves every other service untouched.
    pub fn shutdown(self) -> Result<usize> {
        self.runtime.unregister_owned_services(&self.owner)
    }
}

impl Drop for RestartHost {
    fn drop(&mut self) {
        // A handler keeps only a weak authorization reference, so removing the
        // owned registration here also breaks the runtime/auth lifetime edge.
        let _ = self.runtime.unregister_owned_services(&self.owner);
    }
}

fn authorize_restart(
    authorization: &std::sync::Weak<AuthManager>,
    call: &ServiceCall,
) -> Result<()> {
    if let Some(user_id) = call.context.user_id.as_deref() {
        let auth = authorization.upgrade().ok_or_else(|| {
            Error::Service("Home Assistant restart authorization is stopped".into())
        })?;
        let user = auth.get_user(user_id)?;
        if !user.is_active || (!user.is_owner && !user.is_admin) {
            return Err(Error::InvalidInput(
                "Administrator privileges are required".into(),
            ));
        }
    }
    Ok(())
}

fn validate_restart_call(call: &ServiceCall) -> Result<()> {
    if !call.target.is_empty() {
        return Err(Error::InvalidInput(
            "Home Assistant restart does not accept a target".into(),
        ));
    }

    for (key, value) in &call.data {
        if key != "safe_mode" {
            return Err(Error::InvalidInput(format!(
                "Unsupported homeassistant.restart option: {key}"
            )));
        }
        match value.as_bool() {
            Some(false) => {}
            Some(true) => {
                return Err(Error::InvalidInput(
                    "homeassistant.restart safe_mode is unsupported".into(),
                ));
            }
            None => {
                return Err(Error::InvalidInput(
                    "homeassistant.restart safe_mode must be a boolean".into(),
                ));
            }
        }
    }

    Ok(())
}

/// Result for platforms where replacing the current process is unavailable.
///
/// On Unix, successful `exec` replaces this process and never returns. On
/// other platforms, the helper starts a child and returns its handle; the
/// caller must exit the old process immediately after receiving this result.
#[derive(Debug)]
pub enum RestartOutcome {
    #[cfg_attr(unix, allow(dead_code))] // Unix exec replaces the process instead.
    Spawned(Child),
}

/// Re-execute this native executable with its current arguments and environment.
///
/// Call this only after the main loop has stopped and joined all application
/// components. The new process inherits the current environment, working
/// directory, and standard streams. Unix uses `exec`; the portable fallback
/// starts a child and returns its handle so the caller can exit promptly.
pub fn exec_current_process() -> std::io::Result<RestartOutcome> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }

    #[cfg(not(unix))]
    {
        command.spawn().map(RestartOutcome::Spawned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{Context, MemoryStore, ServiceHandler};
    use serde_json::{Map, Value, json};
    use std::sync::Arc;

    fn runtime_and_auth() -> (Runtime, AuthManager) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        (runtime, auth)
    }

    fn call(data: Value, context: Context) -> ServiceCall {
        ServiceCall {
            domain: DOMAIN.into(),
            service: SERVICE_RESTART.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: Map::new(),
            context,
        }
    }

    fn handler(response: Option<Value>) -> ServiceHandler {
        Arc::new(move |_, call| {
            Ok(ServiceResult {
                changed_states: Vec::new(),
                response: response.clone(),
                context: call.context.clone(),
            })
        })
    }

    #[test]
    fn service_call_sets_request_and_preserves_context() {
        let (runtime, auth) = runtime_and_auth();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "restart-owner-fixture")
            .unwrap();
        let host = RestartHost::start(runtime.clone(), &auth).unwrap();
        let mut receiver = host.receiver();
        let context = Context {
            id: "restart-context".into(),
            parent_id: Some("repair-flow-context".into()),
            user_id: Some(owner.id),
        };

        let result = runtime
            .call_service(call(json!({}), context.clone()))
            .unwrap();

        assert_eq!(result.context, context);
        assert!(host.requested());
        assert!(receiver.has_changed().unwrap());
        assert!(*receiver.borrow_and_update());
    }

    #[test]
    fn non_admin_user_is_denied_without_requesting_restart() {
        let (runtime, auth) = runtime_and_auth();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "restart-admin-fixture")
            .unwrap();
        let user = auth
            .create_user_with_username(&owner.id, "user", "User", "restart-user-fixture", false)
            .unwrap();
        let host = RestartHost::start(runtime.clone(), &auth).unwrap();

        let error = runtime
            .call_service(call(
                json!({}),
                Context {
                    user_id: Some(user.id),
                    ..Context::default()
                },
            ))
            .unwrap_err();

        assert!(matches!(error, Error::InvalidInput(message) if message.contains("Administrator")));
        assert!(!host.requested());
    }

    #[test]
    fn internal_system_context_can_request_restart() {
        let (runtime, auth) = runtime_and_auth();
        let host = RestartHost::start(runtime.clone(), &auth).unwrap();
        let mut receiver = host.receiver();

        let result = runtime
            .call_service(call(json!({}), Context::default()))
            .unwrap();

        assert_eq!(result.context.user_id, None);
        assert!(host.requested());
        assert!(receiver.has_changed().unwrap());
        assert!(*receiver.borrow_and_update());
    }

    #[test]
    fn unsupported_restart_options_have_no_effect() {
        let (runtime, auth) = runtime_and_auth();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "restart-schema-fixture")
            .unwrap();
        let host = RestartHost::start(runtime.clone(), &auth).unwrap();
        let context = Context {
            user_id: Some(owner.id),
            ..Context::default()
        };

        for options in [
            json!({"safe_mode": true}),
            json!({"safe_mode": "yes"}),
            json!({"unknown_option": false}),
        ] {
            assert!(
                runtime
                    .call_service(call(options, context.clone()))
                    .is_err()
            );
            assert!(!host.requested());
        }

        let mut targeted = call(json!({}), context.clone());
        targeted
            .target
            .insert("entity_id".into(), json!("switch.example"));
        assert!(runtime.call_service(targeted).is_err());
        assert!(!host.requested());

        runtime
            .call_service(call(json!({"safe_mode": false}), context))
            .unwrap();
        assert!(host.requested());
    }

    #[test]
    fn failed_duplicate_start_preserves_the_existing_owner() {
        let (runtime, auth) = runtime_and_auth();
        let foreign = json!({"foreign": true});
        runtime
            .register_owned_service(
                "foreign-restart-owner",
                DOMAIN,
                SERVICE_RESTART,
                json!({}),
                handler(Some(foreign.clone())),
            )
            .unwrap();

        assert!(RestartHost::start(runtime.clone(), &auth).is_err());
        let result = runtime
            .call_service(call(json!({}), Context::default()))
            .unwrap();
        assert_eq!(result.response, Some(foreign));
    }

    #[test]
    fn shutdown_and_drop_remove_only_the_owned_handler() {
        let (runtime, auth) = runtime_and_auth();
        runtime
            .register_owned_service(
                "independent-owner",
                "test",
                "keep",
                json!({}),
                handler(None),
            )
            .unwrap();

        let host = RestartHost::start(runtime.clone(), &auth).unwrap();
        assert_eq!(host.shutdown().unwrap(), 1);
        assert!(
            runtime
                .call_service(call(json!({}), Context::default()))
                .is_err()
        );
        assert!(
            runtime
                .call_service(ServiceCall {
                    domain: "test".into(),
                    service: "keep".into(),
                    data: Map::new(),
                    target: Map::new(),
                    context: Context::default(),
                })
                .is_ok()
        );

        let host = RestartHost::start(runtime.clone(), &auth).unwrap();
        drop(host);
        assert!(
            runtime
                .call_service(call(json!({}), Context::default()))
                .is_err()
        );
        assert!(
            runtime
                .call_service(ServiceCall {
                    domain: "test".into(),
                    service: "keep".into(),
                    data: Map::new(),
                    target: Map::new(),
                    context: Context::default(),
                })
                .is_ok()
        );
    }
}
