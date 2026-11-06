//! Native Home Assistant core service handlers.
//!
//! The generic turn services expand active groups, split the resulting entity
//! IDs by domain, and call only services that have a real runtime handler. They
//! never synthesize a device state to imply that a command succeeded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceHandler, ServiceResult};
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
            let handler: ServiceHandler =
                Arc::new(move |runtime, call| handle_turn_service(runtime, call, &groups));
            let description = json!({
                "description": format!("Call {service} on each targeted entity's domain")
            });
            if let Err(error) =
                runtime.register_owned_service(&owner, DOMAIN, service, description, handler)
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

fn handle_turn_service(
    runtime: &Runtime,
    call: &ServiceCall,
    groups: &GroupLookup,
) -> Result<ServiceResult> {
    if !TURN_SERVICES.contains(&call.service.as_str()) {
        return Err(Error::Service(format!(
            "unsupported Home Assistant turn service {:?}",
            call.service
        )));
    }

    validate_normalized_targets(call)?;
    let excluded = parse_excluded_entity_ids(runtime, call, groups)?;
    let Some(target_value) = selected_entity_value(call)? else {
        // Upstream accepts a service call with no target and logs an error.
        // Keep the call a no-op while preserving the causal context.
        tracing::error!(service = %call.service, "Home Assistant generic service called without a target");
        return Ok(empty_result(call.context.clone()));
    };

    let requested = parse_entity_ids(target_value, ENTITY_ID)?;
    if requested.is_empty() {
        return Ok(empty_result(call.context.clone()));
    }

    let mut expanded = expand_entity_ids(runtime, groups, &requested)?;
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

    let mut changed_states = Vec::new();
    for (domain, entity_ids) in by_domain {
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

        let result = runtime.call_service(ServiceCall {
            domain,
            service: call.service.clone(),
            data,
            target: Map::new(),
            context: call.context.clone(),
        })?;
        changed_states.extend(result.changed_states);
    }

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

    use hass_core::{Context, MemoryStore, Runtime, ServiceCall, ServiceResult};
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
    #[test]
    fn mixed_domain_dispatch_preserves_payload_context_and_provider_results() {
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

        let result = runtime.call_service(call).unwrap();
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

        let result = runtime.call_service(call).unwrap();
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
    #[test]
    fn unavailable_known_entity_is_dispatched_but_unknown_unsupported_domain_is_skipped() {
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

        let result = runtime.call_service(call).unwrap();
        assert!(result.changed_states.is_empty());
        assert_eq!(observations.lock().unwrap().len(), 1);
        assert_eq!(
            runtime.get_state("switch.offline").unwrap().state,
            "unavailable"
        );
    }

    #[test]
    fn unknown_target_in_a_supported_domain_errors_before_provider_mutation() {
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

        assert!(runtime.call_service(call).is_err());
        assert!(observations.lock().unwrap().is_empty());
        assert_eq!(runtime.get_state("switch.known").unwrap().state, "off");
    }

    #[test]
    fn no_target_is_a_context_preserving_noop() {
        let runtime = runtime();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let context = Context {
            id: "empty-target".into(),
            parent_id: None,
            user_id: Some("user-2".into()),
        };
        let result = runtime
            .call_service(ServiceCall {
                domain: "homeassistant".into(),
                service: "turn_off".into(),
                data: empty_map(),
                target: empty_map(),
                context: context.clone(),
            })
            .unwrap();
        assert!(result.changed_states.is_empty());
        assert_eq!(result.response, None);
        assert_eq!(result.context, context);
    }

    #[test]
    fn toggle_is_forwarded_as_toggle_to_the_entity_domain() {
        let runtime = runtime();
        let observations = Arc::new(Mutex::new(Vec::new()));
        register_recording_service(&runtime, "light", "toggle", Arc::clone(&observations));
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();

        let result = runtime
            .call_service(service_call(
                "toggle",
                json!("light.ceiling"),
                Context::default(),
            ))
            .unwrap();
        assert!(result.changed_states.is_empty());
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].domain, "light");
        assert_eq!(observations[0].service, "toggle");
    }

    #[test]
    fn entity_id_exclude_is_applied_before_any_domain_dispatch() {
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

        let result = runtime.call_service(call).unwrap();
        assert_eq!(result.changed_states.len(), 1);
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].target["entity_id"],
            json!(["light.ceiling"])
        );
        assert!(!observations[0].data.contains_key(EXCLUDE));
    }

    #[test]
    fn malformed_or_unimplemented_excludes_fail_before_dispatch() {
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
            assert!(runtime.call_service(call).is_err());
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

    #[test]
    fn target_map_takes_precedence_over_service_data_only_after_validation() {
        let runtime = runtime();
        let _services = HomeAssistantServices::start(runtime.clone()).unwrap();
        let mut call = service_call("turn_on", json!("light.ceiling"), Context::default());
        call.target = json!({"entity_id": "light.other"})
            .as_object()
            .unwrap()
            .clone();
        assert!(runtime.call_service(call).is_err());
    }
}
