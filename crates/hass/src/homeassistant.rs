//! Native Home Assistant core service handlers.
//!
//! The generic turn services expand active groups, split the resulting entity
//! IDs by domain, and call only services that have a real runtime handler. They
//! never synthesize a device state to imply that a command succeeded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use hass_core::{
    AsyncServiceHandler, Context, Error, Result, Runtime, ServiceCall,
    ServiceCallCancellationHandle, ServiceResult,
};
use hass_group::{GroupLookup, expand_entity_ids};
use serde_json::{Map, Value, json};
use uuid::Uuid;

const DOMAIN: &str = "homeassistant";
const ENTITY_ID: &str = "entity_id";
const EXCLUDE: &str = "exclude";
const TURN_SERVICES: [&str; 3] = ["turn_on", "turn_off", "toggle"];
const SELECTOR_FIELDS: [&str; 4] = ["device_id", "area_id", "floor_id", "label_id"];
const MAX_TARGETS: usize = 4096;

/// Owns Home Assistant's generic `turn_on`, `turn_off`, and `toggle` services.
///
/// This component only dispatches calls to registered domain services. A
/// supported domain callback or entity-service provider remains responsible
/// for reporting any state change that it actually observed.
pub struct HomeAssistantServices {
    runtime: Runtime,
    owner: String,
}

struct DomainCallCancellationGuard {
    handles: Vec<ServiceCallCancellationHandle>,
    armed: bool,
}

impl DomainCallCancellationGuard {
    fn new() -> Self {
        Self {
            handles: Vec::new(),
            armed: true,
        }
    }

    fn push(&mut self, handle: ServiceCallCancellationHandle) {
        self.handles.push(handle);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DomainCallCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            for handle in &self.handles {
                handle.cancel();
            }
        }
    }
}

impl HomeAssistantServices {
    /// Register the generic services without an active group provider.
    ///
    /// The application uses [`Self::start_with_groups`]; this remains useful
    /// for embedders that do not instantiate the group integration.
    #[allow(dead_code)]
    pub fn start(runtime: Runtime) -> Result<Self> {
        Self::start_with_groups(runtime, GroupLookup::default())
    }

    /// Register the generic services with the application's active groups.
    pub fn start_with_groups(runtime: Runtime, groups: GroupLookup) -> Result<Self> {
        let owner = Uuid::new_v4().to_string();

        for service in TURN_SERVICES {
            let groups = groups.clone();
            let handler: AsyncServiceHandler = Arc::new(move |runtime, call| {
                let groups = groups.clone();
                Box::pin(async move { handle_turn_service_async(runtime, call, groups).await })
            });
            let description = json!({
                "description": format!("Call {service} on each targeted entity's domain")
            });
            if let Err(error) =
                runtime.register_owned_async_service(&owner, DOMAIN, service, description, handler)
            {
                let _ = runtime.unregister_owned_services(&owner);
                return Err(error);
            }
        }

        Ok(Self { runtime, owner })
    }

    /// Remove this component's registrations and return their count.
    pub fn shutdown(self) -> Result<usize> {
        self.runtime.unregister_owned_services(&self.owner)
    }
}

impl Drop for HomeAssistantServices {
    fn drop(&mut self) {
        let _ = self.runtime.unregister_owned_services(&self.owner);
    }
}

async fn handle_turn_service_async(
    runtime: Runtime,
    call: ServiceCall,
    groups: GroupLookup,
) -> Result<ServiceResult> {
    if !TURN_SERVICES.contains(&call.service.as_str()) {
        return Err(Error::Service(format!(
            "unsupported Home Assistant turn service {:?}",
            call.service
        )));
    }

    validate_normalized_targets(&call)?;
    let excluded = parse_excluded_entity_ids(&runtime, &call, &groups)?;
    let Some(target_value) = selected_entity_value(&call)? else {
        // Upstream accepts a service call with no target and logs an error.
        // Keep the call a no-op while preserving the causal context.
        tracing::error!(service = %call.service, "Home Assistant generic service called without a target");
        return Ok(empty_result(call.context.clone()));
    };

    let requested = parse_entity_ids(target_value, ENTITY_ID)?;
    if requested.is_empty() {
        return Ok(empty_result(call.context.clone()));
    }

    let mut expanded = expand_entity_ids(&runtime, &groups, &requested)?;
    if expanded.len() > MAX_TARGETS {
        return Err(Error::InvalidInput(format!(
            "expanded service target exceeds the {MAX_TARGETS} entity limit"
        )));
    }
    // GroupLookup normalizes and de-duplicates active groups. Validate its
    // output as well as direct IDs before any domain service can run.
    let mut target_ids = BTreeSet::new();
    for entity_id in expanded.drain(..) {
        target_ids.insert(normalize_entity_id(&entity_id)?);
    }

    for entity_id in excluded {
        target_ids.remove(&entity_id);
    }
    if target_ids.is_empty() {
        return Ok(empty_result(call.context.clone()));
    }

    let mut by_domain: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unsupported_entities = Vec::new();
    let registered_services = runtime.services();
    for entity_id in target_ids {
        let domain = entity_id
            .split_once('.')
            .expect("entity IDs were validated")
            .0
            .to_owned();
        // Calling the generic service recursively would never terminate.
        if domain == DOMAIN {
            tracing::warn!(
                service = %call.service,
                entity = %entity_id,
                "skipping recursive Home Assistant generic service target"
            );
            continue;
        }

        if !domain_has_service(&registered_services, &domain, &call.service) {
            unsupported_entities.push(entity_id);
            continue;
        }
        by_domain.entry(domain).or_default().push(entity_id);
    }

    if !unsupported_entities.is_empty() {
        tracing::warn!(
            service = %call.service,
            entities = %unsupported_entities.join(", "),
            "Home Assistant generic service has unsupported entities"
        );
    }

    let service_calls = by_domain
        .into_iter()
        .map(|(domain, entity_ids)| {
            let mut data = call.data.clone();
            // Runtime target resolution normally moves target fields into service
            // data before invoking this handler. Preserve that placement for direct
            // callers as well, with target taking precedence as in Home Assistant.
            for (field, value) in &call.target {
                data.insert(field.clone(), value.clone());
            }
            data.remove(EXCLUDE);
            data.insert(
                ENTITY_ID.to_owned(),
                Value::Array(entity_ids.into_iter().map(Value::String).collect()),
            );

            ServiceCall {
                domain,
                service: call.service.clone(),
                data,
                target: Map::new(),
                context: call.context.clone(),
            }
        })
        .collect::<Vec<_>>();
    if service_calls.is_empty() {
        return Ok(empty_result(call.context.clone()));
    }

    let mut cancellation = DomainCallCancellationGuard::new();
    let mut calls = Vec::with_capacity(service_calls.len());
    for service_call in service_calls {
        let call = runtime.spawn_service_call_async(service_call)?;
        cancellation.push(call.cancellation_handle());
        calls.push(call);
    }

    let call_count = calls.len();
    let mut result_waiters = FuturesUnordered::new();
    for (index, call) in calls.iter_mut().enumerate() {
        result_waiters.push(async move { (index, call.result().await) });
    }
    let mut ordered_results = (0..call_count)
        .map(|_| None)
        .collect::<Vec<Option<ServiceResult>>>();
    while let Some((index, result)) = result_waiters.next().await {
        match result {
            Ok(result) => ordered_results[index] = Some(result),
            Err(error) => {
                // Core retains the other domain calls. They must finish without
                // being canceled when one provider returns an error.
                drop(result_waiters);
                cancellation.disarm();
                return Err(error);
            }
        }
    }
    cancellation.disarm();
    let changed_states = ordered_results
        .into_iter()
        .flatten()
        .flat_map(|result| result.changed_states)
        .collect();

    // Home Assistant's generic turn handler awaits domain calls but does not
    // return their service responses. Preserve only states reported by those
    // actual calls.
    Ok(ServiceResult {
        changed_states,
        response: None,
        context: call.context.clone(),
    })
}

fn selected_entity_value(call: &ServiceCall) -> Result<Option<&Value>> {
    let data_entity = call.data.get(ENTITY_ID);
    let target_entity = call.target.get(ENTITY_ID);
    if data_entity.is_some() && target_entity.is_some() {
        return Err(Error::InvalidInput(
            "service entity_id must be supplied in target or data, not both".into(),
        ));
    }
    Ok(target_entity.or(data_entity))
}

fn validate_normalized_targets(call: &ServiceCall) -> Result<()> {
    for field in SELECTOR_FIELDS {
        if call.data.contains_key(field) || call.target.contains_key(field) {
            return Err(Error::InvalidInput(format!(
                "unresolved Home Assistant target selector {field:?}"
            )));
        }
    }
    if let Some(field) = call.target.keys().find(|field| field.as_str() != ENTITY_ID) {
        return Err(Error::InvalidInput(format!(
            "unsupported Home Assistant service target field {field:?}"
        )));
    }
    Ok(())
}

fn parse_entity_ids(value: &Value, field: &str) -> Result<Vec<String>> {
    let values: Vec<&str> = match value {
        Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(Error::InvalidInput(format!(
                    "{field} must contain at least one entity ID"
                )));
            }
            if trimmed.eq_ignore_ascii_case("all") {
                return Err(Error::InvalidInput(
                    "entity_id all must be expanded by the service target resolver".into(),
                ));
            }
            if trimmed.eq_ignore_ascii_case("none") {
                return Err(Error::InvalidInput(
                    "entity_id none is not a valid generic turn target".into(),
                ));
            }
            value.split(',').map(str::trim).collect()
        }
        Value::Array(values) => {
            if values.is_empty() {
                return Err(Error::InvalidInput(format!(
                    "{field} cannot be an empty list"
                )));
            }
            values
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        Error::InvalidInput(format!("{field} list must contain only strings"))
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
        _ => {
            return Err(Error::InvalidInput(format!(
                "{field} must be a string or a list of strings"
            )));
        }
    };

    if values.len() > MAX_TARGETS {
        return Err(Error::InvalidInput(format!(
            "{field} exceeds the {MAX_TARGETS} entity limit"
        )));
    }
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        if value.trim().is_empty() {
            return Err(Error::InvalidInput(format!(
                "{field} values cannot be empty"
            )));
        }
        ids.push(normalize_entity_id(value.trim())?);
    }
    Ok(ids)
}

fn normalize_entity_id(value: &str) -> Result<String> {
    let entity_id = value.to_ascii_lowercase();
    let Some((domain, object_id)) = entity_id.split_once('.') else {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {value:?}: expected domain.object_id"
        )));
    };
    if object_id.contains('.')
        || !valid_entity_component(domain)
        || !valid_entity_component(object_id)
    {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {value:?}: expected lowercase domain.object_id"
        )));
    }
    Ok(entity_id)
}

fn valid_entity_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Resolve the supported generic-service exclusion shape before dispatch.
/// Pinned Core also accepts registry-backed device, area, floor, and label
/// exclusions; this runtime handler currently supports `exclude.entity_id`.
/// Other exclusion selectors fail explicitly instead of leaking downstream.
fn parse_excluded_entity_ids(
    runtime: &Runtime,
    call: &ServiceCall,
    groups: &GroupLookup,
) -> Result<BTreeSet<String>> {
    let Some(exclude) = call.data.get(EXCLUDE) else {
        return Ok(BTreeSet::new());
    };
    let Value::Object(exclude) = exclude else {
        return Err(Error::InvalidInput(
            "exclude must be an object containing entity_id".into(),
        ));
    };
    if exclude.contains_key(EXCLUDE) {
        return Err(Error::InvalidInput(
            "nested exclude selectors are not supported".into(),
        ));
    }
    for field in exclude.keys() {
        if field != ENTITY_ID {
            return Err(Error::InvalidInput(format!(
                "unsupported exclude selector {field:?}; only entity_id is supported"
            )));
        }
    }
    let Some(value) = exclude.get(ENTITY_ID) else {
        return Ok(BTreeSet::new());
    };
    let requested = parse_entity_ids(value, "exclude.entity_id")?;
    let expanded = expand_entity_ids(runtime, groups, &requested)?;
    let mut excluded = BTreeSet::new();
    for entity_id in expanded {
        excluded.insert(normalize_entity_id(&entity_id)?);
    }
    Ok(excluded)
}

fn domain_has_service(services: &Value, domain: &str, service: &str) -> bool {
    services
        .get(domain)
        .and_then(Value::as_object)
        .is_some_and(|services| services.contains_key(service))
}

fn empty_result(context: Context) -> ServiceResult {
    ServiceResult {
        changed_states: Vec::new(),
        response: None,
        context,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    use hass_core::{
        AsyncServiceHandler, Context, Error, MemoryStore, Runtime, ServiceCall, ServiceResult,
    };
    use hass_group::{GroupConfig, GroupIntegration, GroupLookup, expand_entity_ids};
    use serde_json::{Map, Value, json};

    use super::{EXCLUDE, HomeAssistantServices};

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn empty_map() -> Map<String, Value> {
        Map::new()
    }

    fn set_state(runtime: &Runtime, entity_id: &str, state: &str, context: Context) {
        runtime
            .set_state(entity_id, state, empty_map(), context, false)
            .unwrap();
    }

    fn register_state_service(
        runtime: &Runtime,
        domain: &str,
        service: &str,
        observations: Arc<Mutex<Vec<ServiceCall>>>,
    ) {
        let owner = format!("fixture-{domain}-{service}");
        let prefix = format!("{domain}.");
        let state = if service == "turn_off" { "off" } else { "on" };
        let observed = Arc::clone(&observations);
        runtime
            .register_entity_service(
                &owner,
                domain,
                service,
                json!({"description": "fixture native provider"}),
                Arc::new(move |entity_id| entity_id.starts_with(&prefix)),
                Arc::new(move |runtime, call| {
                    observed.lock().unwrap().push(call.clone());
                    let entity_ids = call
                        .target
                        .get("entity_id")
                        .and_then(Value::as_array)
                        .expect("Runtime passes provider entity IDs in target")
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect::<Vec<_>>();
                    let mut changed_states = Vec::new();
                    for entity_id in entity_ids {
                        let change = runtime.set_state(
                            &entity_id,
                            state,
                            empty_map(),
                            call.context.clone(),
                            false,
                        )?;
                        if change.changed {
                            changed_states.push(change.state);
                        }
                    }
                    Ok(ServiceResult {
                        changed_states,
                        response: Some(json!({"called": true})),
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
    }

    fn register_recording_service(
        runtime: &Runtime,
        domain: &str,
        service: &str,
        observations: Arc<Mutex<Vec<ServiceCall>>>,
    ) {
        let owner = format!("fixture-{domain}-{service}");
        let prefix = format!("{domain}.");
        runtime
            .register_entity_service(
                &owner,
                domain,
                service,
                json!({"description": "fixture native provider"}),
                Arc::new(move |entity_id| entity_id.starts_with(&prefix)),
                Arc::new(move |_runtime, call| {
                    observations.lock().unwrap().push(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
    }

    fn service_call(service: &str, entity_ids: Value, context: Context) -> ServiceCall {
        ServiceCall {
            domain: "homeassistant".into(),
            service: service.into(),
            data: json!({"entity_id": entity_ids})
                .as_object()
                .unwrap()
                .clone(),
            target: empty_map(),
            context,
        }
    }

    // Fixture cases are drawn from Home Assistant Core
    // cbf2c02c9bff331537c19bff69da8dc11c3fbfbf, from
    // components/homeassistant/services.py::_async_handle_turn_service and
    // tests/components/homeassistant/test_init.py::{test_turn_on_without_entities,
    // test_turn_on_skips_domains_without_service,test_not_allowing_recursion}.
    #[tokio::test]
    async fn mixed_domain_dispatch_preserves_payload_context_and_provider_results() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(&runtime, "light.ceiling", "off", Context::default());
        set_state(&runtime, "switch.pump", "off", Context::default());
        register_state_service(&runtime, "light", "turn_on", Arc::clone(&observations));
        register_state_service(&runtime, "switch", "turn_on", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "context-generic-turn".into(),
            parent_id: Some("parent-action".into()),
            user_id: Some("user-7".into()),
        };
        let mut call = service_call(
            "turn_on",
            json!(["switch.pump", "light.ceiling", "light.ceiling"]),
            context.clone(),
        );
        call.data.insert("brightness".into(), json!(137));
        call.data.insert("transition".into(), json!(2.5));

        let result = runtime.call_service_async(call).await.unwrap();
        assert_eq!(result.context, context);
        assert_eq!(result.response, None);
        assert_eq!(
            result
                .changed_states
                .iter()
                .map(|state| (state.entity_id.as_str(), state.state.as_str()))
                .collect::<Vec<_>>(),
            vec![("light.ceiling", "on"), ("switch.pump", "on")]
        );

        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 2);
        for call in observations.iter() {
            assert_eq!(call.context, context);
            assert_eq!(call.data["brightness"], json!(137));
            assert_eq!(call.data["transition"], json!(2.5));
            assert!(!call.data.contains_key("entity_id"));
        }
        assert_eq!(observations[0].domain, "light");
        assert_eq!(
            observations[0].target["entity_id"],
            json!(["light.ceiling"])
        );
        assert_eq!(observations[1].domain, "switch");
        assert_eq!(observations[1].target["entity_id"], json!(["switch.pump"]));
    }

    #[tokio::test]
    async fn generic_turn_waits_for_async_provider_and_preserves_context() {
        let runtime = runtime();
        set_state(&runtime, "light.ceiling", "off", Context::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let handler: AsyncServiceHandler = Arc::new(move |runtime, call| {
            let started_tx = Arc::clone(&started_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("provider should only run once")
                    .send(call.clone())
                    .expect("test should still observe the provider call");
                let release = {
                    release_rx
                        .lock()
                        .unwrap()
                        .take()
                        .expect("provider should only run once")
                };
                release
                    .await
                    .expect("test should release the held provider");

                let change = runtime.set_state(
                    "light.ceiling",
                    "on",
                    empty_map(),
                    call.context.clone(),
                    false,
                )?;
                Ok(ServiceResult {
                    changed_states: change.changed.then_some(change.state).into_iter().collect(),
                    response: Some(json!({"provider": "completed"})),
                    context: call.context.clone(),
                })
            })
        });
        runtime
            .register_entity_async_service(
                "fixture-light-held-turn-on",
                "light",
                "turn_on",
                json!({"description": "held async provider fixture"}),
                Arc::new(|entity_id| entity_id == "light.ceiling"),
                handler,
            )
            .unwrap();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "held-provider-context".into(),
            parent_id: Some("parent-action".into()),
            user_id: Some("user-7".into()),
        };
        let outer = tokio::spawn({
            let runtime = runtime.clone();
            let context = context.clone();
            async move {
                runtime
                    .call_service_async(service_call("turn_on", json!("light.ceiling"), context))
                    .await
            }
        });

        let provider_call = tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .expect("generic service should start its async provider")
            .expect("async provider should report its call");
        assert_eq!(provider_call.context, context);
        assert_eq!(provider_call.target["entity_id"], json!(["light.ceiling"]));
        assert!(
            !outer.is_finished(),
            "outer service must await the provider"
        );
        assert_eq!(runtime.get_state("light.ceiling").unwrap().state, "off");

        release_tx.send(()).unwrap();
        let result = outer.await.unwrap().unwrap();
        assert_eq!(result.context, context);
        assert_eq!(result.response, None);
        assert_eq!(result.changed_states.len(), 1);
        assert_eq!(runtime.get_state("light.ceiling").unwrap().state, "on");
    }

    #[tokio::test]
    async fn domain_calls_start_together_and_provider_error_does_not_cancel_sibling() {
        let runtime = runtime();
        set_state(&runtime, "light.ceiling", "off", Context::default());
        set_state(&runtime, "switch.pump", "off", Context::default());
        let (light_started_tx, light_started_rx) = tokio::sync::oneshot::channel();
        let (light_release_tx, light_release_rx) = tokio::sync::oneshot::channel();
        let (switch_started_tx, switch_started_rx) = tokio::sync::oneshot::channel();
        let (switch_release_tx, switch_release_rx) = tokio::sync::oneshot::channel();
        let (switch_finished_tx, switch_finished_rx) = tokio::sync::oneshot::channel();
        let light_started_tx = Arc::new(Mutex::new(Some(light_started_tx)));
        let light_release_rx = Arc::new(Mutex::new(Some(light_release_rx)));
        let light_handler: AsyncServiceHandler = Arc::new(move |_runtime, call| {
            let started_tx = Arc::clone(&light_started_tx);
            let release_rx = Arc::clone(&light_release_rx);
            Box::pin(async move {
                started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("light provider should only run once")
                    .send(call)
                    .expect("test should still observe the light provider");
                let release = {
                    release_rx
                        .lock()
                        .unwrap()
                        .take()
                        .expect("light provider should only run once")
                };
                release
                    .await
                    .expect("test should release the light provider");
                Err(Error::Service("light fixture failure".into()))
            })
        });
        runtime
            .register_entity_async_service(
                "fixture-light-held-failure",
                "light",
                "turn_on",
                json!({"description": "held failing provider fixture"}),
                Arc::new(|entity_id| entity_id == "light.ceiling"),
                light_handler,
            )
            .unwrap();

        let switch_started_tx = Arc::new(Mutex::new(Some(switch_started_tx)));
        let switch_release_rx = Arc::new(Mutex::new(Some(switch_release_rx)));
        let switch_finished_tx = Arc::new(Mutex::new(Some(switch_finished_tx)));
        let switch_handler: AsyncServiceHandler = Arc::new(move |runtime, call| {
            let started_tx = Arc::clone(&switch_started_tx);
            let release_rx = Arc::clone(&switch_release_rx);
            let finished_tx = Arc::clone(&switch_finished_tx);
            Box::pin(async move {
                started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("switch provider should only run once")
                    .send(call.clone())
                    .expect("test should still observe the switch provider");
                let release = {
                    release_rx
                        .lock()
                        .unwrap()
                        .take()
                        .expect("switch provider should only run once")
                };
                release
                    .await
                    .expect("test should release the switch provider");
                let change = runtime.set_state(
                    "switch.pump",
                    "on",
                    empty_map(),
                    call.context.clone(),
                    false,
                )?;
                finished_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("switch provider should only finish once")
                    .send(())
                    .expect("test should still observe switch completion");
                Ok(ServiceResult {
                    changed_states: change.changed.then_some(change.state).into_iter().collect(),
                    response: None,
                    context: call.context.clone(),
                })
            })
        });
        runtime
            .register_entity_async_service(
                "fixture-switch-held-success",
                "switch",
                "turn_on",
                json!({"description": "held successful provider fixture"}),
                Arc::new(|entity_id| entity_id == "switch.pump"),
                switch_handler,
            )
            .unwrap();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "two-domain-held-context".into(),
            parent_id: Some("parent-action".into()),
            user_id: Some("user-9".into()),
        };
        let outer = tokio::spawn({
            let runtime = runtime.clone();
            let context = context.clone();
            async move {
                runtime
                    .call_service_async(service_call(
                        "turn_on",
                        json!(["light.ceiling", "switch.pump"]),
                        context,
                    ))
                    .await
            }
        });

        let (light_call, switch_call) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(5), light_started_rx,),
            tokio::time::timeout(std::time::Duration::from_secs(5), switch_started_rx,),
        );
        let light_call = light_call
            .expect("generic service should start its light provider")
            .expect("light provider should report its call");
        let switch_call = switch_call
            .expect("generic service should start its switch provider")
            .expect("switch provider should report its call");
        assert_eq!(light_call.context, context);
        assert_eq!(switch_call.context, context);
        assert_eq!(light_call.target["entity_id"], json!(["light.ceiling"]));
        assert_eq!(switch_call.target["entity_id"], json!(["switch.pump"]));
        assert!(
            !outer.is_finished(),
            "generic call should await provider results"
        );

        light_release_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), outer)
            .await
            .expect("first provider failure should be returned promptly")
            .unwrap();
        assert!(
            matches!(result, Err(Error::Service(message)) if message.contains("light fixture failure"))
        );
        assert_eq!(runtime.get_state("switch.pump").unwrap().state, "off");

        switch_release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), switch_finished_rx)
            .await
            .expect("remaining provider should continue after sibling failure")
            .expect("switch provider should finish");
        assert_eq!(runtime.get_state("switch.pump").unwrap().state, "on");
    }

    #[tokio::test]
    async fn canceling_generic_call_cancels_owned_domain_provider() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let runtime = runtime();
        set_state(&runtime, "light.ceiling", "off", Context::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let dropped_tx = Arc::new(Mutex::new(Some(dropped_tx)));
        let handler: AsyncServiceHandler = Arc::new(move |_runtime, call| {
            let started_tx = Arc::clone(&started_tx);
            let dropped_tx = Arc::clone(&dropped_tx);
            Box::pin(async move {
                let _drop_signal = DropSignal(dropped_tx.lock().unwrap().take());
                started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("provider should only start once")
                    .send(call.context.clone())
                    .expect("test should still observe provider start");
                std::future::pending::<std::result::Result<ServiceResult, Error>>().await
            })
        });
        runtime
            .register_entity_async_service(
                "fixture-light-canceled-turn-on",
                "light",
                "turn_on",
                json!({"description": "canceled async provider fixture"}),
                Arc::new(|entity_id| entity_id == "light.ceiling"),
                handler,
            )
            .unwrap();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "canceled-provider-context".into(),
            parent_id: Some("parent-action".into()),
            user_id: Some("user-10".into()),
        };
        let outer = tokio::spawn({
            let runtime = runtime.clone();
            let context = context.clone();
            async move {
                runtime
                    .call_service_async(service_call("turn_on", json!("light.ceiling"), context))
                    .await
            }
        });

        let started_context = tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .expect("generic service should start its async provider")
            .expect("provider should report its start");
        assert_eq!(started_context, context);
        outer.abort();
        assert!(outer.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
            .await
            .expect("outer cancellation should cancel its owned provider")
            .expect("provider future should be dropped");
        assert_eq!(runtime.get_state("light.ceiling").unwrap().state, "off");
    }

    #[tokio::test]
    async fn active_groups_expand_deduplicate_and_dispatch_real_members() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(&runtime, "light.ceiling", "off", Context::default());
        set_state(&runtime, "switch.pump", "off", Context::default());
        register_state_service(&runtime, "light", "turn_on", Arc::clone(&observations));
        register_state_service(&runtime, "switch", "turn_on", Arc::clone(&observations));

        let groups = BTreeMap::from([
            (
                "nested".to_owned(),
                GroupConfig::new(vec!["light.ceiling", "switch.pump"]),
            ),
            (
                "all_rooms".to_owned(),
                GroupConfig::new(vec!["group.nested", "light.ceiling"]),
            ),
        ]);
        let group_integration = GroupIntegration::start(runtime.clone(), groups)
            .await
            .unwrap();
        assert_eq!(
            expand_entity_ids(
                &runtime,
                &GroupLookup::default(),
                &["group.all_rooms".to_owned()],
            )
            .unwrap(),
            vec!["light.ceiling", "switch.pump"]
        );
        let _services =
            HomeAssistantServices::start_with_groups(runtime.clone(), group_integration.lookup())
                .unwrap();
        let call = service_call(
            "turn_on",
            json!(["group.all_rooms", "group.nested", "light.ceiling"]),
            Context::default(),
        );

        let result = runtime.call_service_async(call).await.unwrap();
        assert_eq!(result.changed_states.len(), 2);
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].domain, "light");
        assert_eq!(
            observations[0].target["entity_id"],
            json!(["light.ceiling"])
        );
        assert_eq!(observations[1].domain, "switch");
        assert_eq!(observations[1].target["entity_id"], json!(["switch.pump"]));
    }

    #[tokio::test]
    async fn group_cycle_fixture_is_rejected_before_any_group_state_is_created() {
        let runtime = runtime();
        let cyclic = BTreeMap::from([
            ("loop_a".to_owned(), GroupConfig::new(vec!["group.loop_b"])),
            ("loop_b".to_owned(), GroupConfig::new(vec!["group.loop_a"])),
        ]);
        assert!(
            GroupIntegration::start(runtime.clone(), cyclic)
                .await
                .is_err()
        );
        assert!(runtime.get_state("group.loop_a").is_none());
        assert!(runtime.get_state("group.loop_b").is_none());
    }

    // Upstream does not filter unavailable entities before dispatch; it only
    // skips targets whose domain has no matching service.
    #[tokio::test]
    async fn unavailable_known_entity_is_dispatched_but_unknown_unsupported_domain_is_skipped() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(
            &runtime,
            "switch.offline",
            "unavailable",
            Context::default(),
        );
        register_recording_service(&runtime, "switch", "turn_on", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let call = service_call(
            "turn_on",
            json!(["switch.offline", "sensor.missing"]),
            Context::default(),
        );

        let result = runtime.call_service_async(call).await.unwrap();
        assert!(result.changed_states.is_empty());
        assert_eq!(observations.lock().unwrap().len(), 1);
        assert_eq!(
            runtime.get_state("switch.offline").unwrap().state,
            "unavailable"
        );
    }

    #[tokio::test]
    async fn unknown_target_in_a_supported_domain_errors_before_provider_mutation() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(&runtime, "switch.known", "off", Context::default());
        let observed = Arc::clone(&observations);
        runtime
            .register_entity_service(
                "fixture-switch-known-only",
                "switch",
                "turn_on",
                json!({"description": "fixture native provider"}),
                Arc::new(|entity_id| entity_id == "switch.known"),
                Arc::new(move |_runtime, call| {
                    observed.lock().unwrap().push(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let call = service_call(
            "turn_on",
            json!(["switch.known", "switch.unknown"]),
            Context::default(),
        );

        assert!(runtime.call_service_async(call).await.is_err());
        assert!(observations.lock().unwrap().is_empty());
        assert_eq!(runtime.get_state("switch.known").unwrap().state, "off");
    }

    #[tokio::test]
    async fn no_target_is_a_context_preserving_noop() {
        let runtime = runtime();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "empty-target".into(),
            parent_id: None,
            user_id: Some("user-2".into()),
        };
        let result = runtime
            .call_service_async(ServiceCall {
                domain: "homeassistant".into(),
                service: "turn_off".into(),
                data: empty_map(),
                target: empty_map(),
                context: context.clone(),
            })
            .await
            .unwrap();
        assert!(result.changed_states.is_empty());
        assert_eq!(result.response, None);
        assert_eq!(result.context, context);
    }

    #[tokio::test]
    async fn toggle_is_forwarded_as_toggle_to_the_entity_domain() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        register_recording_service(&runtime, "light", "toggle", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();

        let result = runtime
            .call_service_async(service_call(
                "toggle",
                json!("light.ceiling"),
                Context::default(),
            ))
            .await
            .unwrap();
        assert!(result.changed_states.is_empty());
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].domain, "light");
        assert_eq!(observations[0].service, "toggle");
    }

    #[tokio::test]
    async fn entity_id_exclude_is_applied_before_any_domain_dispatch() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(&runtime, "light.ceiling", "on", Context::default());
        set_state(&runtime, "light.kitchen", "on", Context::default());
        register_state_service(&runtime, "light", "turn_off", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let mut call = service_call(
            "turn_off",
            json!(["light.ceiling", "light.kitchen"]),
            Context::default(),
        );
        call.data
            .insert(EXCLUDE.into(), json!({"entity_id": "light.kitchen"}));

        let result = runtime.call_service_async(call).await.unwrap();
        assert_eq!(result.changed_states.len(), 1);
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].target["entity_id"],
            json!(["light.ceiling"])
        );
        assert!(!observations[0].data.contains_key(EXCLUDE));
    }

    #[tokio::test]
    async fn malformed_or_unimplemented_excludes_fail_before_dispatch() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        set_state(&runtime, "light.ceiling", "off", Context::default());
        register_recording_service(&runtime, "light", "turn_on", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();

        for exclude in [
            json!("light.ceiling"),
            json!({"area_id": "living_room"}),
            json!({"exclude": {}}),
            json!({"entity_id": "all"}),
        ] {
            let mut call = service_call("turn_on", json!("light.ceiling"), Context::default());
            call.data.insert(EXCLUDE.into(), exclude);
            assert!(runtime.call_service_async(call).await.is_err());
        }
        assert!(observations.lock().unwrap().is_empty());
    }

    #[test]
    fn drop_and_shutdown_unregister_only_owned_generic_services() {
        let runtime = runtime();
        runtime
            .register_service(
                "homeassistant",
                "custom",
                json!({"description": "not owned by generic services"}),
                Arc::new(|_runtime, call| {
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();

        let services = HomeAssistantServices::start(runtime.clone()).unwrap();
        assert_eq!(
            runtime.services()["homeassistant"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "custom".to_owned(),
                "toggle".to_owned(),
                "turn_off".to_owned(),
                "turn_on".to_owned(),
            ])
        );
        drop(services);
        let services = runtime.services();
        assert!(services["homeassistant"].get("turn_on").is_none());
        assert!(services["homeassistant"].get("turn_off").is_none());
        assert!(services["homeassistant"].get("toggle").is_none());
        assert!(services["homeassistant"].get("custom").is_some());

        let services = HomeAssistantServices::start(runtime.clone()).unwrap();
        assert_eq!(services.shutdown().unwrap(), 3);
        let services = runtime.services();
        assert!(services["homeassistant"].get("turn_on").is_none());
        assert!(services["homeassistant"].get("custom").is_some());
    }

    #[tokio::test]
    async fn target_map_takes_precedence_over_service_data_only_after_validation() {
        let runtime = runtime();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let mut call = service_call("turn_on", json!("light.ceiling"), Context::default());
        call.target = json!({"entity_id": "light.other"})
            .as_object()
            .unwrap()
            .clone();
        assert!(runtime.call_service_async(call).await.is_err());
    }
}
