//! Duplicate Home Assistant Zeroconf instance ID Repairs host.
//!
//! This adapter observes raw service updates before ordinary Zeroconf matcher
//! dispatch. It owns only its two listener guards and its exact Repairs
//! platform registration; listener and platform callbacks keep weak references
//! to the host state.

use std::{
    collections::{BTreeSet, HashSet},
    net::IpAddr,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak},
};

use hass_core::{Context, Error, Runtime, ServiceCall};
use hass_repairs::{
    FixFlowRequest, IssueDraft, RepairsError, RepairsFlow, RepairsFlowContext, RepairsFlowStep,
    RepairsManager, RepairsPlatform, RepairsPlatformRegistrationGuard, RepairsResult,
};
use hass_zeroconf::{
    HOME_ASSISTANT_SERVICE_TYPE, ListenerGuard, RawZeroconfServiceInfo, ZeroconfManager,
    ZeroconfServiceInfo,
};
use serde_json::{Map, Value, json};

const REPAIRS_DOMAIN: &str = "zeroconf";
const DUPLICATE_INSTANCE_ID: &str = "duplicate_instance_id";
const CONFIRM_STEP: &str = "confirm_recreate";
const RESTART_DOMAIN: &str = "homeassistant";
const RESTART_SERVICE: &str = "restart";

/// Owns the native host for Zeroconf duplicate-instance Repairs.
///
/// The host does not keep the Zeroconf integration or Repairs integration
/// alive: both managers are weak application handles. Its callbacks likewise
/// capture weak host state, so the runtime cannot retain this host through a
/// registration cycle.
pub struct ZeroconfRepairsHost {
    state: Arc<HostState>,
    _update_listener: ListenerGuard,
    _removed_listener: ListenerGuard,
    _platform_registration: RepairsPlatformRegistrationGuard,
}

impl ZeroconfRepairsHost {
    /// Register raw update/removal listeners and the `zeroconf` fix-flow
    /// platform.
    pub fn start(
        runtime: Runtime,
        repairs_manager: RepairsManager,
        zeroconf_manager: ZeroconfManager,
    ) -> hass_core::Result<Self> {
        let state = Arc::new(HostState::new(runtime, repairs_manager));

        let platform = Arc::new(DuplicateInstanceRepairPlatform {
            state: Arc::downgrade(&state),
        });
        let platform_registration = state
            .repairs
            .register_platform_guard(REPAIRS_DOMAIN, platform)
            .map_err(repairs_error_to_core)?;

        let update_state = Arc::downgrade(&state);
        let update_listener = zeroconf_manager.register_service_update_listener(move |record| {
            let Some(state) = update_state.upgrade() else {
                return Ok(());
            };
            let Some(_lease) = HostState::begin_callback(&state) else {
                return Ok(());
            };
            state.on_update(record)
        })?;

        let remove_state = Arc::downgrade(&state);
        let removed_listener =
            match zeroconf_manager.register_service_removed_listener(move |name| {
                let Some(state) = remove_state.upgrade() else {
                    return Ok(());
                };
                let Some(_lease) = HostState::begin_callback(&state) else {
                    return Ok(());
                };
                state.on_removed(name)
            }) {
                Ok(listener) => listener,
                Err(error) => {
                    drop(update_listener);
                    drop(platform_registration);
                    return Err(error);
                }
            };

        Ok(Self {
            state,
            _update_listener: update_listener,
            _removed_listener: removed_listener,
            _platform_registration: platform_registration,
        })
    }

    /// Install the local advertisement identity used by duplicate detection.
    ///
    /// Call this once the local Home Assistant service information and its
    /// addresses are known, before Zeroconf browsing can deliver callbacks.
    /// Until then, duplicate detection intentionally does nothing.
    pub fn set_local_identity<I>(&self, uuid: impl Into<String>, local_ips: I)
    where
        I: IntoIterator<Item = IpAddr>,
    {
        let mut policy = lock(&self.state.policy);
        policy.local_identity = Some(LocalIdentity {
            uuid: uuid.into(),
            ips: local_ips.into_iter().collect(),
        });
    }

    /// Stop new work, wait for callbacks already admitted, and release the
    /// listener and exact Repairs platform guards.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for ZeroconfRepairsHost {
    fn drop(&mut self) {
        self.state.stop_and_wait();
    }
}

struct HostState {
    runtime: Runtime,
    repairs: RepairsManager,
    policy: Mutex<PolicyState>,
    lifecycle: Mutex<Lifecycle>,
    callbacks_drained: Condvar,
}

impl HostState {
    fn new(runtime: Runtime, repairs: RepairsManager) -> Self {
        Self {
            runtime,
            repairs,
            policy: Mutex::new(PolicyState::default()),
            lifecycle: Mutex::new(Lifecycle {
                accepting_callbacks: true,
                in_flight_callbacks: 0,
            }),
            callbacks_drained: Condvar::new(),
        }
    }

    fn begin_callback(state: &Arc<Self>) -> Option<CallbackLease> {
        let mut lifecycle = lock(&state.lifecycle);
        if !lifecycle.accepting_callbacks {
            return None;
        }
        lifecycle.in_flight_callbacks += 1;
        Some(CallbackLease {
            state: Arc::clone(state),
        })
    }

    fn stop_and_wait(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.accepting_callbacks = false;
        while lifecycle.in_flight_callbacks != 0 {
            lifecycle = self
                .callbacks_drained
                .wait(lifecycle)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn on_update(&self, raw: &RawZeroconfServiceInfo) -> hass_core::Result<()> {
        if raw.service_type != HOME_ASSISTANT_SERVICE_TYPE {
            return Ok(());
        }

        let Some(info) = ZeroconfServiceInfo::from_raw(raw.clone()) else {
            return Ok(());
        };
        let Some(discovered_uuid) = info.properties.get("uuid").filter(|uuid| !uuid.is_empty())
        else {
            return Ok(());
        };

        let local = lock(&self.policy).local_identity.clone();
        let Some(local) = local else {
            return Ok(());
        };

        let discovered_ips: HashSet<IpAddr> = info
            .ip_addresses
            .iter()
            .map(|address| address.address)
            .collect();
        if !discovered_ips.is_disjoint(&local.ips) {
            // Home Assistant's own reflected advertisement shares at least
            // one local address; preserve any existing conflict state.
            return Ok(());
        }

        if discovered_uuid != local.uuid {
            let no_conflicts_remain = {
                let mut policy = lock(&self.policy);
                policy.conflicting_names.remove(&info.name);
                policy.conflicting_names.is_empty()
            };
            if no_conflicts_remain {
                self.repairs
                    .delete_issue(REPAIRS_DOMAIN, DUPLICATE_INSTANCE_ID)
                    .map_err(repairs_error_to_core)?;
            }
            return Ok(());
        }

        // Track the name before evaluating hostname formatting. The pinned
        // Python path adds the conflict first, then fails at
        // `hostname.rstrip(".")` if hostname is absent.
        lock(&self.policy)
            .conflicting_names
            .insert(info.name.clone());

        let Some(hostname) = info.hostname.as_deref() else {
            let detail = format!(
                "duplicate Home Assistant instance {name:?} has no hostname; source hostname.rstrip('.') failed",
                name = info.name
            );
            return Err(Error::Service(detail));
        };

        let other_ip = joined_addresses(&info);
        let mut placeholders = Map::new();
        placeholders.insert("instance_id".into(), Value::String(local.uuid));
        placeholders.insert("other_ip".into(), Value::String(other_ip));
        placeholders.insert(
            "other_host_url".into(),
            Value::String(hostname.trim_end_matches('.').to_owned()),
        );

        let mut issue = IssueDraft::new(
            REPAIRS_DOMAIN,
            DUPLICATE_INSTANCE_ID,
            true,
            "error",
            DUPLICATE_INSTANCE_ID,
        );
        issue.is_persistent = false;
        issue.translation_placeholders = Some(placeholders);
        self.repairs
            .create_issue(issue)
            .map_err(repairs_error_to_core)?;
        Ok(())
    }

    fn on_removed(&self, name: &str) -> hass_core::Result<()> {
        let last_conflict_removed = {
            let mut policy = lock(&self.policy);
            policy.conflicting_names.remove(name) && policy.conflicting_names.is_empty()
        };
        if last_conflict_removed {
            self.repairs
                .delete_issue(REPAIRS_DOMAIN, DUPLICATE_INSTANCE_ID)
                .map_err(repairs_error_to_core)?;
        }
        Ok(())
    }
}

impl Drop for CallbackLease {
    fn drop(&mut self) {
        let mut lifecycle = lock(&self.state.lifecycle);
        lifecycle.in_flight_callbacks -= 1;
        if lifecycle.in_flight_callbacks == 0 {
            self.state.callbacks_drained.notify_all();
        }
    }
}

struct CallbackLease {
    state: Arc<HostState>,
}

#[derive(Default)]
struct PolicyState {
    local_identity: Option<LocalIdentity>,
    conflicting_names: HashSet<String>,
}

#[derive(Clone)]
struct LocalIdentity {
    uuid: String,
    ips: HashSet<IpAddr>,
}

struct Lifecycle {
    accepting_callbacks: bool,
    in_flight_callbacks: usize,
}

struct DuplicateInstanceRepairPlatform {
    state: Weak<HostState>,
}

impl RepairsPlatform for DuplicateInstanceRepairPlatform {
    fn create_fix_flow(&self, request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
        if request.handler != REPAIRS_DOMAIN || request.issue_id != DUPLICATE_INSTANCE_ID {
            return Err(RepairsError::UnknownIssue {
                domain: request.handler,
                issue_id: request.issue_id,
            });
        }
        Ok(Box::new(DuplicateInstanceRepairFlow {
            state: self.state.clone(),
            issue_id: request.issue_id,
        }))
    }
}

struct DuplicateInstanceRepairFlow {
    state: Weak<HostState>,
    issue_id: String,
}

impl RepairsFlow for DuplicateInstanceRepairFlow {
    fn step(
        &mut self,
        context: &RepairsFlowContext,
        user_input: Option<Value>,
    ) -> RepairsResult<RepairsFlowStep> {
        let Some(state) = self.state.upgrade() else {
            return Err(stopped_flow_error());
        };
        let Some(_lease) = HostState::begin_callback(&state) else {
            return Err(stopped_flow_error());
        };

        if user_input.is_none() {
            let issue = state
                .repairs
                .get_issue(REPAIRS_DOMAIN, &self.issue_id)?
                .ok_or_else(|| RepairsError::UnknownIssue {
                    domain: REPAIRS_DOMAIN.into(),
                    issue_id: self.issue_id.clone(),
                })?;
            return Ok(RepairsFlowStep::Form {
                step_id: CONFIRM_STEP.into(),
                data_schema: Some(json!([])),
                errors: None,
                description_placeholders: Some(issue.translation_placeholders.unwrap_or_default()),
            });
        }

        let caller_context = caller_context(context)?;
        crate::instance_id::recreate(&state.runtime).map_err(|error| {
            RepairsError::Core(Error::Service(format!(
                "could not recreate Home Assistant instance ID: {error:#}"
            )))
        })?;
        state.runtime.call_service(ServiceCall {
            domain: RESTART_DOMAIN.into(),
            service: RESTART_SERVICE.into(),
            data: Map::new(),
            target: Map::new(),
            context: caller_context,
        })?;

        Ok(RepairsFlowStep::CreateEntry {
            title: Some(String::new()),
            data: Map::new(),
            description: None,
            description_placeholders: None,
            next_flow: None,
        })
    }
}

fn caller_context(flow_context: &RepairsFlowContext) -> RepairsResult<Context> {
    let value = flow_context
        .metadata
        .get("caller_context")
        .or_else(|| flow_context.metadata.get("context"))
        .cloned()
        .unwrap_or_else(|| Value::Object(flow_context.metadata.clone()));
    serde_json::from_value(value).map_err(|error| {
        RepairsError::Core(Error::InvalidInput(format!(
            "invalid Repairs caller context: {error}"
        )))
    })
}

fn joined_addresses(info: &ZeroconfServiceInfo) -> String {
    info.ip_addresses
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

fn stopped_flow_error() -> RepairsError {
    RepairsError::Core(Error::Service(
        "Zeroconf duplicate-instance repair host is stopped".into(),
    ))
}

fn repairs_error_to_core(error: RepairsError) -> Error {
    match error {
        RepairsError::Core(error) => error,
        other => Error::Service(format!("Zeroconf Repairs operation failed: {other}")),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIRM_STEP, DUPLICATE_INSTANCE_ID, REPAIRS_DOMAIN, RESTART_DOMAIN, RESTART_SERVICE,
        ZeroconfRepairsHost, lock,
    };

    use hass_config::ConfigEntries;
    use hass_core::{MemoryStore, Runtime, ServiceCall, ServiceHandler, ServiceResult};
    use hass_repairs::{
        FixFlowRequest, IssueDraft, IssueEntry, RepairsError, RepairsFlow, RepairsFlowContext,
        RepairsFlowProgressStep, RepairsFlowStep, RepairsIntegration, RepairsManager,
        RepairsPlatform, RepairsResult,
    };
    use hass_zeroconf::{
        HOME_ASSISTANT_SERVICE_TYPE, MatcherIndex, RawTxtEntry, RawZeroconfServiceInfo,
        ResolverFuture, ScopedIpAddress, ServiceStateChange, ZeroconfBackend, ZeroconfIntegration,
        ZeroconfManager,
    };
    use serde_json::{Map, Value, json};
    use std::{
        collections::HashMap,
        net::IpAddr,
        str::FromStr,
        sync::{Arc, Mutex},
    };
    use tokio::runtime::Handle;

    #[derive(Default)]
    struct MemoryBackend {
        records: Mutex<HashMap<(String, String), RawZeroconfServiceInfo>>,
    }

    impl MemoryBackend {
        fn insert(&self, record: RawZeroconfServiceInfo) {
            lock(&self.records).insert((record.service_type.clone(), record.name.clone()), record);
        }
    }

    impl ZeroconfBackend for MemoryBackend {
        fn complete_cached(
            &self,
            service_type: &str,
            name: &str,
        ) -> Option<RawZeroconfServiceInfo> {
            lock(&self.records)
                .get(&(service_type.to_owned(), name.to_owned()))
                .cloned()
        }

        fn cached_names(&self, service_type: &str) -> Vec<String> {
            lock(&self.records)
                .keys()
                .filter(|(kind, _)| kind == service_type)
                .map(|(_, name)| name.clone())
                .collect()
        }

        fn resolve(&self, _service_type: String, _name: String) -> ResolverFuture {
            Box::pin(async { Ok(None) })
        }
    }

    struct TestEnvironment {
        runtime: Runtime,
        _repairs_integration: RepairsIntegration,
        repairs: RepairsManager,
        _zeroconf_integration: ZeroconfIntegration,
        zeroconf: ZeroconfManager,
        backend: Arc<MemoryBackend>,
        host: Option<ZeroconfRepairsHost>,
    }

    fn environment() -> TestEnvironment {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config = ConfigEntries::new(runtime.clone()).unwrap();
        let repairs_integration =
            RepairsIntegration::start(runtime.clone(), config, "2026.11.7").unwrap();
        let repairs = repairs_integration.manager();

        let backend = Arc::new(MemoryBackend::default());
        let zeroconf_integration = ZeroconfIntegration::with_matchers(
            backend.clone(),
            MatcherIndex::default(),
            None,
            Handle::current(),
        );
        let zeroconf = zeroconf_integration.manager();
        let host =
            ZeroconfRepairsHost::start(runtime.clone(), repairs.clone(), zeroconf.clone()).unwrap();

        TestEnvironment {
            runtime,
            _repairs_integration: repairs_integration,
            repairs,
            _zeroconf_integration: zeroconf_integration,
            zeroconf,
            backend,
            host: Some(host),
        }
    }

    fn ip(value: &str) -> IpAddr {
        IpAddr::from_str(value).unwrap()
    }

    fn service(
        name: &str,
        addresses: &[&str],
        uuid: &str,
        hostname: Option<&str>,
    ) -> RawZeroconfServiceInfo {
        RawZeroconfServiceInfo {
            ip_addresses: addresses
                .iter()
                .map(|value| ScopedIpAddress::new(ip(value), 0))
                .collect(),
            port: Some(8123),
            hostname: hostname.map(str::to_owned),
            service_type: HOME_ASSISTANT_SERVICE_TYPE.into(),
            name: name.into(),
            txt_entries: vec![RawTxtEntry {
                key: b"uuid".to_vec(),
                value: Some(uuid.as_bytes().to_vec()),
            }],
        }
    }

    fn publish(env: &TestEnvironment, record: RawZeroconfServiceInfo) {
        let name = record.name.clone();
        env.backend.insert(record);
        env.zeroconf
            .service_update(
                HOME_ASSISTANT_SERVICE_TYPE,
                name,
                ServiceStateChange::Updated,
            )
            .unwrap();
    }

    fn issue(env: &TestEnvironment) -> Option<IssueEntry> {
        env.repairs
            .get_issue(REPAIRS_DOMAIN, DUPLICATE_INSTANCE_ID)
            .unwrap()
    }

    fn set_local(env: &TestEnvironment) {
        env.host
            .as_ref()
            .unwrap()
            .set_local_identity("local-uuid", [ip("192.168.1.10")]);
    }

    fn register_fake_restart(runtime: &Runtime, calls: Arc<Mutex<Vec<ServiceCall>>>) {
        let handler: ServiceHandler = Arc::new(move |_, call| {
            lock(&calls).push(call.clone());
            Ok(ServiceResult {
                changed_states: Vec::new(),
                response: None,
                context: call.context.clone(),
            })
        });
        runtime
            .register_service(RESTART_DOMAIN, RESTART_SERVICE, json!({}), handler)
            .unwrap();
    }

    fn add_duplicate(env: &TestEnvironment, name: &str, hostname: Option<&str>) {
        publish(
            env,
            service(name, &["192.168.1.20"], "local-uuid", hostname),
        );
    }

    #[tokio::test]
    async fn detects_disjoint_duplicate_with_nonpersistent_error_and_source_placeholders() {
        let env = environment();
        set_local(&env);
        add_duplicate(
            &env,
            "living-room._home-assistant._tcp.local.",
            Some("ha.local..."),
        );

        let issue = issue(&env).expect("duplicate-instance issue");
        assert_eq!(issue.domain, REPAIRS_DOMAIN);
        assert_eq!(issue.issue_id, DUPLICATE_INSTANCE_ID);
        assert_eq!(issue.is_fixable, Some(true));
        assert!(!issue.is_persistent);
        assert_eq!(issue.severity.as_deref(), Some("error"));
        assert_eq!(
            issue.translation_key.as_deref(),
            Some(DUPLICATE_INSTANCE_ID)
        );
        assert_eq!(
            issue.translation_placeholders.unwrap(),
            Map::from_iter([
                ("instance_id".into(), json!("local-uuid")),
                ("other_ip".into(), json!("192.168.1.20")),
                ("other_host_url".into(), json!("ha.local")),
            ])
        );
    }

    #[tokio::test]
    async fn duplicate_detection_waits_for_local_identity_and_ignores_reflections() {
        let env = environment();
        add_duplicate(&env, "ha._home-assistant._tcp.local.", Some("ha.local."));
        assert!(issue(&env).is_none());

        set_local(&env);
        publish(
            &env,
            service(
                "reflected._home-assistant._tcp.local.",
                &["192.168.1.10", "192.168.1.20"],
                "local-uuid",
                Some("ha.local."),
            ),
        );
        assert!(issue(&env).is_none());
    }

    #[tokio::test]
    async fn requires_a_truthy_exact_uuid_and_usable_converted_service_info() {
        let env = environment();
        set_local(&env);

        publish(
            &env,
            service(
                "empty-uuid._home-assistant._tcp.local.",
                &["192.168.1.20"],
                "",
                Some("remote.local."),
            ),
        );
        assert!(issue(&env).is_none());

        let mut wrong_key = service(
            "wrong-key._home-assistant._tcp.local.",
            &["192.168.1.21"],
            "local-uuid",
            Some("remote.local."),
        );
        wrong_key.txt_entries[0].key = b"UUID".to_vec();
        publish(&env, wrong_key);
        assert!(issue(&env).is_none());

        let mut unusable_address = service(
            "unusable._home-assistant._tcp.local.",
            &["169.254.10.20"],
            "local-uuid",
            Some("remote.local."),
        );
        unusable_address.ip_addresses = vec![ScopedIpAddress::new(ip("169.254.10.20"), 0)];
        publish(&env, unusable_address);
        assert!(issue(&env).is_none());
    }

    #[tokio::test]
    async fn a_discovered_uuid_change_clears_its_conflict() {
        let env = environment();
        set_local(&env);
        let name = "remote._home-assistant._tcp.local.";
        add_duplicate(&env, name, Some("remote.local."));
        assert!(issue(&env).is_some());

        publish(
            &env,
            service(name, &["192.168.1.20"], "new-uuid", Some("remote.local.")),
        );
        assert!(issue(&env).is_none());
    }

    #[tokio::test]
    async fn issue_remains_until_the_last_conflicting_name_is_removed() {
        let env = environment();
        set_local(&env);
        let first = "one._home-assistant._tcp.local.";
        let second = "two._home-assistant._tcp.local.";
        env.repairs
            .create_issue(IssueDraft::new(
                REPAIRS_DOMAIN,
                DUPLICATE_INSTANCE_ID,
                true,
                "error",
                DUPLICATE_INSTANCE_ID,
            ))
            .unwrap();
        env.zeroconf
            .service_update(
                HOME_ASSISTANT_SERVICE_TYPE,
                "not-conflicting._home-assistant._tcp.local.",
                ServiceStateChange::Removed,
            )
            .unwrap();
        assert!(issue(&env).is_some());

        add_duplicate(&env, first, Some("one.local."));
        add_duplicate(&env, second, Some("two.local."));
        assert!(issue(&env).is_some());

        env.zeroconf
            .service_update(
                HOME_ASSISTANT_SERVICE_TYPE,
                first,
                ServiceStateChange::Removed,
            )
            .unwrap();
        assert!(issue(&env).is_some());

        env.zeroconf
            .service_update(
                HOME_ASSISTANT_SERVICE_TYPE,
                second,
                ServiceStateChange::Removed,
            )
            .unwrap();
        assert!(issue(&env).is_none());
    }

    #[tokio::test]
    async fn missing_hostname_errors_after_tracking_the_conflicting_name() {
        let env = environment();
        set_local(&env);
        let missing = "missing-host._home-assistant._tcp.local.";
        add_duplicate(&env, missing, None);
        assert!(issue(&env).is_none());

        add_duplicate(
            &env,
            "other._home-assistant._tcp.local.",
            Some("other.local."),
        );
        assert!(issue(&env).is_some());

        env.zeroconf
            .service_update(
                HOME_ASSISTANT_SERVICE_TYPE,
                missing,
                ServiceStateChange::Removed,
            )
            .unwrap();
        assert!(issue(&env).is_some());
    }

    #[tokio::test]
    async fn confirmation_recreates_shared_uuid_then_calls_real_restart_service_with_caller_context()
     {
        let mut env = environment();
        set_local(&env);
        add_duplicate(
            &env,
            "remote._home-assistant._tcp.local.",
            Some("remote.local."),
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        register_fake_restart(&env.runtime, calls.clone());

        let previous_uuid = crate::instance_id::get(&env.runtime).unwrap();
        let flow = env
            .repairs
            .init_fix_flow(
                REPAIRS_DOMAIN,
                Some(Map::from_iter([
                    ("issue_id".into(), json!(DUPLICATE_INSTANCE_ID)),
                    (
                        "context".into(),
                        json!({
                            "id": "caller-context-id",
                            "parent_id": "parent-context-id",
                            "user_id": "admin-user",
                        }),
                    ),
                ])),
                None,
            )
            .unwrap();
        assert!(matches!(
            flow.step,
            RepairsFlowProgressStep::Form { ref step_id, ref description_placeholders, .. }
                if step_id == CONFIRM_STEP
                    && description_placeholders.as_ref().unwrap().get("instance_id") == Some(&json!("local-uuid"))
        ));

        let result = env.repairs.submit_step(&flow.flow_id, json!({})).unwrap();
        assert!(matches!(
            result.step,
            RepairsFlowProgressStep::CreateEntry { ref title, .. }
                if title.as_deref() == Some("")
        ));
        let replacement_uuid = crate::instance_id::get(&env.runtime).unwrap();
        assert_ne!(replacement_uuid, previous_uuid);

        let calls = lock(&calls);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].domain, RESTART_DOMAIN);
        assert_eq!(calls[0].service, RESTART_SERVICE);
        assert_eq!(calls[0].context.id, "caller-context-id");
        assert_eq!(
            calls[0].context.parent_id.as_deref(),
            Some("parent-context-id")
        );
        assert_eq!(calls[0].context.user_id.as_deref(), Some("admin-user"));
        drop(calls);
        drop(env.host.take());
    }

    #[tokio::test]
    async fn showing_and_cancelling_confirmation_does_not_change_identity_or_restart() {
        let mut env = environment();
        set_local(&env);
        add_duplicate(
            &env,
            "remote._home-assistant._tcp.local.",
            Some("remote.local."),
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        register_fake_restart(&env.runtime, calls.clone());
        let previous_uuid = crate::instance_id::get(&env.runtime).unwrap();

        let flow = env
            .repairs
            .start_fix_flow(REPAIRS_DOMAIN, DUPLICATE_INSTANCE_ID)
            .unwrap();
        assert!(matches!(
            flow.step,
            RepairsFlowProgressStep::Form { ref step_id, .. } if step_id == CONFIRM_STEP
        ));
        env.repairs.abort_flow(&flow.flow_id).unwrap();

        assert_eq!(
            crate::instance_id::get(&env.runtime).unwrap(),
            previous_uuid
        );
        assert!(lock(&calls).is_empty());
        drop(env.host.take());
    }

    #[tokio::test]
    async fn unknown_issues_do_not_receive_a_generic_confirmation_flow() {
        let env = environment();
        let wrong_issue =
            IssueDraft::new(REPAIRS_DOMAIN, "some_other_issue", true, "error", "other");
        env.repairs.create_issue(wrong_issue).unwrap();

        assert!(matches!(
            env.repairs.start_fix_flow(REPAIRS_DOMAIN, "some_other_issue"),
            Err(RepairsError::UnknownIssue { issue_id, .. }) if issue_id == "some_other_issue"
        ));
        assert!(matches!(
            env.repairs.start_fix_flow(REPAIRS_DOMAIN, "missing_issue"),
            Err(RepairsError::UnknownIssue { issue_id, .. }) if issue_id == "missing_issue"
        ));
    }

    #[tokio::test]
    async fn teardown_unregisters_owned_callbacks_and_platform_without_a_strong_host_cycle() {
        let mut env = environment();
        set_local(&env);
        add_duplicate(
            &env,
            "remote._home-assistant._tcp.local.",
            Some("remote.local."),
        );
        let weak_state = Arc::downgrade(&env.host.as_ref().unwrap().state);

        // Replace the host's platform registration before dropping it. Its
        // exact registration guard must leave this newer registration intact.
        assert!(env.repairs.unregister_platform(REPAIRS_DOMAIN).unwrap());
        let replacement = Arc::new(ReplacementPlatform);
        let replacement_guard = env
            .repairs
            .register_platform_guard(REPAIRS_DOMAIN, replacement)
            .unwrap();
        drop(env.host.take());
        assert!(weak_state.upgrade().is_none());

        let after_drop = publish_and_read_issue_after_drop(&env);
        assert!(
            after_drop.is_some(),
            "the dropped listener must not clear the issue"
        );

        let progress = env
            .repairs
            .start_fix_flow(REPAIRS_DOMAIN, DUPLICATE_INSTANCE_ID)
            .unwrap();
        assert!(matches!(
            progress.step,
            RepairsFlowProgressStep::Form { ref step_id, .. } if step_id == "replacement"
        ));
        drop(replacement_guard);
    }

    fn publish_and_read_issue_after_drop(env: &TestEnvironment) -> Option<IssueEntry> {
        publish(
            env,
            service(
                "remote._home-assistant._tcp.local.",
                &["192.168.1.20"],
                "changed-uuid",
                Some("remote.local."),
            ),
        );
        issue(env)
    }

    struct ReplacementPlatform;

    impl RepairsPlatform for ReplacementPlatform {
        fn create_fix_flow(&self, _request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
            Ok(Box::new(ReplacementFlow))
        }
    }

    struct ReplacementFlow;

    impl RepairsFlow for ReplacementFlow {
        fn step(
            &mut self,
            _context: &RepairsFlowContext,
            _user_input: Option<Value>,
        ) -> RepairsResult<RepairsFlowStep> {
            Ok(RepairsFlowStep::Form {
                step_id: "replacement".into(),
                data_schema: Some(json!([])),
                errors: None,
                description_placeholders: None,
            })
        }
    }
}
