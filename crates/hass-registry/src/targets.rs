//! Resolve Home Assistant service target selectors through the native registries.

use std::collections::BTreeSet;

use crate::{EntityEntry, Registries};
use hass_core::{Error, MAX_HISTORY_QUERY_FILTERS, Result, Runtime, ServiceCall};
use serde_json::Value;

const TARGET_FIELDS: [&str; 5] = ["entity_id", "device_id", "area_id", "floor_id", "label_id"];

/// What the caller should do after normalizing a service target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetResolution {
    /// The call did not carry target selectors. Keep legacy handler behavior.
    Unspecified,
    /// The target selected these entity IDs. The list is sorted and unique.
    Matched(Vec<String>),
    /// A selector was present but did not select any entities.
    NoMatches,
}

#[derive(Default)]
struct ParsedTargets {
    entity_ids: Vec<String>,
    all_entities: bool,
    device_ids: Vec<String>,
    area_ids: Vec<String>,
    floor_ids: Vec<String>,
    label_ids: Vec<String>,
}

/// Resolve entity, device, area, floor, and label service targets.
///
/// Selectors may be supplied in `call.target` or in the legacy flat service
/// data. A resolved target is normalized into `data.entity_id`; selector keys
/// are removed from both maps. Calls with no selectors are left untouched.
/// `NoMatches` is distinct from `Unspecified` so adapters can avoid dispatching
/// a target that would otherwise be mistaken for an unscoped legacy call.
pub fn resolve_service_targets(
    runtime: &Runtime,
    registries: Option<&Registries>,
    call: &mut ServiceCall,
) -> Result<TargetResolution> {
    if let Some(field) = call
        .target
        .keys()
        .find(|field| !TARGET_FIELDS.contains(&field.as_str()))
    {
        return Err(invalid(format!(
            "unsupported service target field {field:?}"
        )));
    }

    if call.data.contains_key("entity_id") && call.target.contains_key("entity_id") {
        return Err(invalid(
            "service entity_id must be supplied in target or data, not both",
        ));
    }

    let mut merged = serde_json::Map::new();
    for field in TARGET_FIELDS {
        if let Some(value) = call.data.get(field) {
            merged.insert(field.to_owned(), value.clone());
        }
        // Home Assistant's service call merges target over service data. In
        // particular, selectors of the same kind from target take precedence.
        if let Some(value) = call.target.get(field) {
            merged.insert(field.to_owned(), value.clone());
        }
    }
    if merged.is_empty() {
        return Ok(TargetResolution::Unspecified);
    }

    let parsed = parse_targets(&merged, registries)?;
    let resolution = if parsed.all_entities {
        resolve_all_entities(runtime, registries, &call.domain, &call.service)?
    } else {
        let selected = resolve_registry_targets(&parsed, registries)?;
        if selected.is_empty() {
            TargetResolution::NoMatches
        } else {
            TargetResolution::Matched(selected.into_iter().collect())
        }
    };

    for field in TARGET_FIELDS {
        call.data.remove(field);
    }
    call.target.clear();
    if let TargetResolution::Matched(entity_ids) = &resolution {
        call.data.insert(
            "entity_id".to_owned(),
            Value::Array(entity_ids.iter().cloned().map(Value::String).collect()),
        );
    }

    Ok(resolution)
}

/// Resolve service targets for native entity services without interpreting
/// arbitrary service data as selectors.
///
/// Explicit target maps are always resolved. Flat selector fields are
/// interpreted only for the native entity-service domains supported here;
/// this keeps payload fields such as `entity_id` in REST-command templates and
/// general script variables intact.
pub fn resolve_entity_service_targets(
    runtime: &Runtime,
    registries: Option<&Registries>,
    call: &mut ServiceCall,
) -> Result<TargetResolution> {
    if call.target.is_empty() && !is_native_entity_service(&call.domain, &call.service) {
        return Ok(TargetResolution::Unspecified);
    }
    resolve_service_targets(runtime, registries, call)
}

fn is_native_entity_service(domain: &str, service: &str) -> bool {
    matches!(
        domain,
        "input_boolean"
            | "input_number"
            | "input_text"
            | "input_select"
            | "input_button"
            | "input_datetime"
            | "counter"
            | "timer"
            | "schedule"
            | "automation"
            | "scene"
            | "switch"
            | "light"
            | "cover"
            | "fan"
            | "button"
            | "number"
            | "select"
            | "text"
            | "homeassistant"
    ) || (domain == "script" && matches!(service, "turn_on" | "turn_off" | "toggle"))
}

fn parse_targets(
    selectors: &serde_json::Map<String, Value>,
    registries: Option<&Registries>,
) -> Result<ParsedTargets> {
    let mut parsed = ParsedTargets::default();
    let mut selector_item_count = 0usize;

    if let Some(value) = selectors.get("entity_id") {
        let (entity_ids, all_entities) = parse_entity_ids(value, registries)?;
        selector_item_count += entity_ids.len().max(if all_entities { 1 } else { 0 });
        parsed.entity_ids = entity_ids;
        parsed.all_entities = all_entities;
    }
    if let Some(value) = selectors.get("device_id") {
        parsed.device_ids = parse_ids(value, "device_id")?;
        selector_item_count += parsed.device_ids.len();
    }
    if let Some(value) = selectors.get("area_id") {
        parsed.area_ids = parse_ids(value, "area_id")?;
        selector_item_count += parsed.area_ids.len();
    }
    if let Some(value) = selectors.get("floor_id") {
        parsed.floor_ids = parse_ids(value, "floor_id")?;
        selector_item_count += parsed.floor_ids.len();
    }
    if let Some(value) = selectors.get("label_id") {
        parsed.label_ids = parse_ids(value, "label_id")?;
        selector_item_count += parsed.label_ids.len();
    }
    if selector_item_count > MAX_HISTORY_QUERY_FILTERS {
        return Err(invalid(format!(
            "service target exceeds the {MAX_HISTORY_QUERY_FILTERS} selector limit"
        )));
    }

    if parsed.all_entities
        && (selectors.len() != 1
            || !parsed.device_ids.is_empty()
            || !parsed.area_ids.is_empty()
            || !parsed.floor_ids.is_empty()
            || !parsed.label_ids.is_empty())
    {
        return Err(invalid(
            "service target all cannot be combined with other selectors",
        ));
    }

    Ok(parsed)
}

fn parse_entity_ids(value: &Value, registries: Option<&Registries>) -> Result<(Vec<String>, bool)> {
    let values: Vec<&str> = match value {
        Value::String(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            if normalized == "all" {
                return Ok((Vec::new(), true));
            }
            if normalized == "none" {
                return Ok((Vec::new(), false));
            }
            value.split(',').map(str::trim).collect()
        }
        Value::Array(values) => {
            if values.len() > MAX_HISTORY_QUERY_FILTERS {
                return Err(invalid(format!(
                    "service entity_id list exceeds the {MAX_HISTORY_QUERY_FILTERS} target limit"
                )));
            }
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| invalid("service entity_id list must contain only strings"))
                })
                .collect::<Result<Vec<_>>>()?
        }
        _ => {
            return Err(invalid(
                "service entity_id must be a string or a list of strings",
            ));
        }
    };
    if values.len() > MAX_HISTORY_QUERY_FILTERS {
        return Err(invalid(format!(
            "service entity_id list exceeds the {MAX_HISTORY_QUERY_FILTERS} target limit"
        )));
    }

    let mut resolved = BTreeSet::new();
    let mut registry_entities = None;
    for value in values {
        if value.is_empty() {
            return Err(invalid("service entity_id values cannot be empty"));
        }
        let normalized = value.to_ascii_lowercase();
        if normalized == "all" {
            return Err(invalid(
                "service target all cannot be combined with entity IDs",
            ));
        }
        if normalized == "none" {
            return Err(invalid("entity_id none must be supplied as a string"));
        }
        if is_entity_id(&normalized) && normalized.len() <= 255 {
            resolved.insert(normalized);
        } else if is_registry_uuid(&normalized) {
            let entries = registry_entities
                .get_or_insert_with(|| registries.map(Registries::entities).unwrap_or_default());
            let entity_id = entries
                .iter()
                .find(|entry| entry.id == normalized)
                .map(|entry| entry.entity_id.clone())
                .ok_or_else(|| {
                    invalid("service entity UUID is not present in the entity registry")
                })?;
            resolved.insert(entity_id);
        } else {
            return Err(invalid(format!(
                "invalid service entity ID or registry UUID {value:?}"
            )));
        }
    }
    Ok((resolved.into_iter().collect(), false))
}

fn parse_ids(value: &Value, field: &str) -> Result<Vec<String>> {
    let values: Vec<&str> = match value {
        Value::String(value) => vec![value.as_str()],
        Value::Array(values) => {
            if values.len() > MAX_HISTORY_QUERY_FILTERS {
                return Err(invalid(format!(
                    "service {field} list exceeds the {MAX_HISTORY_QUERY_FILTERS} target limit"
                )));
            }
            values
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        invalid(format!("service {field} list must contain only strings"))
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
        _ => {
            return Err(invalid(format!(
                "service {field} must be a string or a list of strings"
            )));
        }
    };
    if values.len() > MAX_HISTORY_QUERY_FILTERS {
        return Err(invalid(format!(
            "service {field} list exceeds the {MAX_HISTORY_QUERY_FILTERS} target limit"
        )));
    }
    let mut ids = BTreeSet::new();
    for value in values {
        if value.is_empty()
            || value.chars().count() > 255
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(invalid(format!("invalid service {field} value {value:?}")));
        }
        ids.insert(value.to_owned());
    }
    Ok(ids.into_iter().collect())
}

fn is_registry_uuid(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_entity_id(value: &str) -> bool {
    let Some((domain, object_id)) = value.split_once('.') else {
        return false;
    };
    !object_id.contains('.') && valid_entity_component(domain) && valid_entity_component(object_id)
}

fn valid_entity_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn resolve_all_entities(
    runtime: &Runtime,
    registries: Option<&Registries>,
    domain: &str,
    service: &str,
) -> Result<TargetResolution> {
    let mut entity_ids = BTreeSet::new();
    let services = runtime.services();
    for state in runtime.states() {
        let entity_domain = state
            .entity_id
            .split_once('.')
            .map(|(entity_domain, _)| entity_domain)
            .unwrap_or("");
        let eligible = if domain == "homeassistant" {
            // Native generic controls delegate to an actual domain service.
            // Groups are redundant in an all-entity expansion and could
            // otherwise reintroduce hidden members through membership.
            !matches!(entity_domain, "homeassistant" | "group")
                && services
                    .get(entity_domain)
                    .and_then(Value::as_object)
                    .is_some_and(|entries| entries.contains_key(service))
        } else {
            entity_domain == domain
        };
        if !eligible {
            continue;
        }
        if registries
            .and_then(|registries| registries.entity(&state.entity_id))
            .is_some_and(|entry| !indirect_target_eligible(&entry))
        {
            continue;
        }
        entity_ids.insert(state.entity_id);
        if entity_ids.len() > MAX_HISTORY_QUERY_FILTERS {
            return Err(invalid(format!(
                "service target exceeds the {MAX_HISTORY_QUERY_FILTERS} entity limit"
            )));
        }
    }
    if entity_ids.is_empty() {
        Ok(TargetResolution::NoMatches)
    } else {
        Ok(TargetResolution::Matched(entity_ids.into_iter().collect()))
    }
}

fn resolve_registry_targets(
    targets: &ParsedTargets,
    registries: Option<&Registries>,
) -> Result<BTreeSet<String>> {
    let mut selected: BTreeSet<String> = targets.entity_ids.iter().cloned().collect();
    if targets.device_ids.is_empty()
        && targets.area_ids.is_empty()
        && targets.floor_ids.is_empty()
        && targets.label_ids.is_empty()
    {
        return Ok(selected);
    }
    let Some(registries) = registries else {
        return Ok(selected);
    };

    let entities = registries.entities();
    let devices = registries.devices();
    let areas = registries.areas();

    let label_ids: BTreeSet<&str> = targets.label_ids.iter().map(String::as_str).collect();
    let floor_ids: BTreeSet<&str> = targets.floor_ids.iter().map(String::as_str).collect();
    let mut area_ids: BTreeSet<String> = targets.area_ids.iter().cloned().collect();
    area_ids.extend(
        areas
            .iter()
            .filter(|area| {
                area.floor_id
                    .as_deref()
                    .is_some_and(|floor| floor_ids.contains(floor))
            })
            .map(|area| area.id.clone()),
    );
    area_ids.extend(
        areas
            .iter()
            .filter(|area| {
                area.labels
                    .iter()
                    .any(|label| label_ids.contains(label.as_str()))
            })
            .map(|area| area.id.clone()),
    );

    let requested_devices: BTreeSet<&str> = targets.device_ids.iter().map(String::as_str).collect();
    let mut device_ids: BTreeSet<String> = devices
        .iter()
        .filter(|device| {
            requested_devices.contains(device.id.as_str())
                || device
                    .labels
                    .iter()
                    .any(|label| label_ids.contains(label.as_str()))
        })
        .map(|device| device.id.clone())
        .collect();
    let parent_device_ids = device_ids.clone();
    device_ids.extend(
        devices
            .iter()
            .filter(|device| {
                device
                    .via_device_id
                    .as_deref()
                    .is_some_and(|parent| parent_device_ids.contains(parent))
            })
            .map(|device| device.id.clone()),
    );

    let area_device_ids: BTreeSet<&str> = devices
        .iter()
        .filter(|device| {
            device
                .area_id
                .as_deref()
                .is_some_and(|area| area_ids.contains(area))
        })
        .map(|device| device.id.as_str())
        .collect();

    for entry in &entities {
        if entry.disabled_by.is_none() && entry.hidden_by.is_none() {
            // Direct label-to-entity expansion includes entity-category entries.
            if entry
                .labels
                .iter()
                .any(|label| label_ids.contains(label.as_str()))
            {
                insert_selected(&mut selected, &entry.entity_id)?;
            }
        }

        if !indirect_target_eligible(entry) {
            continue;
        }
        if entry
            .device_id
            .as_deref()
            .is_some_and(|device_id| device_ids.contains(device_id))
        {
            insert_selected(&mut selected, &entry.entity_id)?;
        }
        if entry
            .area_id
            .as_deref()
            .is_some_and(|area| area_ids.contains(area))
        {
            insert_selected(&mut selected, &entry.entity_id)?;
        }
        // An explicit entity area overrides the device's area. Only entities
        // with no area of their own inherit a targeted device area.
        if entry.area_id.is_none()
            && entry
                .device_id
                .as_deref()
                .is_some_and(|device_id| area_device_ids.contains(device_id))
        {
            insert_selected(&mut selected, &entry.entity_id)?;
        }
    }
    Ok(selected)
}

fn indirect_target_eligible(entry: &EntityEntry) -> bool {
    entry.disabled_by.is_none() && entry.hidden_by.is_none() && entry.entity_category.is_none()
}

fn insert_selected(selected: &mut BTreeSet<String>, entity_id: &str) -> Result<()> {
    selected.insert(entity_id.to_owned());
    if selected.len() > MAX_HISTORY_QUERY_FILTERS {
        return Err(invalid(format!(
            "service target exceeds the {MAX_HISTORY_QUERY_FILTERS} entity limit"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::{
        AreaInput, DeviceRegistration, EntityEntry, EntityRegistration, EntityUpdate, FloorInput,
        LabelInput, Registries,
    };
    use hass_core::{Context, Error, MemoryStore, Runtime, ServiceCall};
    use serde_json::{Map, json};

    use super::{TargetResolution, resolve_entity_service_targets, resolve_service_targets};

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).expect("runtime")
    }

    fn call(data: ValueMap, target: ValueMap) -> ServiceCall {
        ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data,
            target,
            context: Context::default(),
        }
    }

    type ValueMap = Map<String, serde_json::Value>;

    fn value_map(value: serde_json::Value) -> ValueMap {
        value.as_object().expect("object").clone()
    }

    fn entity(
        registries: &Registries,
        domain: &str,
        object_id: &str,
        device_id: Option<&str>,
        area_id: Option<&str>,
        labels: &[&str],
        category: Option<&str>,
    ) -> EntityEntry {
        let registration = EntityRegistration {
            device_id: device_id.map(str::to_owned),
            area_id: area_id.map(str::to_owned),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            entity_category: category.map(str::to_owned),
            ..EntityRegistration::new(domain, "test", object_id)
        };
        registries.get_or_create_entity(registration).unwrap()
    }

    fn ids(result: TargetResolution) -> Vec<String> {
        match result {
            TargetResolution::Matched(ids) => ids,
            result => panic!("expected matched IDs, got {result:?}"),
        }
    }

    #[test]
    fn resolves_device_area_floor_and_label_selectors_with_area_override_and_union() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let floor = registries
            .create_floor(FloorInput::new("First Floor"))
            .unwrap();
        let label = registries.create_label(LabelInput::new("Work")).unwrap();
        let kitchen = registries
            .create_area(AreaInput {
                name: "Kitchen".into(),
                floor_id: Some(floor.floor_id.clone()),
                labels: BTreeSet::from([label.label_id.clone()]),
                ..AreaInput::default()
            })
            .unwrap();
        let office = registries.create_area(AreaInput::new("Office")).unwrap();
        let device = registries
            .get_or_create_device(DeviceRegistration {
                identifiers: BTreeSet::from([("serial".into(), "lamp".into())]),
                area_id: Some(kitchen.id.clone()),
                labels: BTreeSet::from([label.label_id.clone()]),
                ..DeviceRegistration::default()
            })
            .unwrap();
        let inherited = entity(
            &registries,
            "light",
            "inherited",
            Some(&device.id),
            None,
            &[],
            None,
        );
        let override_area = entity(
            &registries,
            "light",
            "override_area",
            Some(&device.id),
            Some(&office.id),
            &[],
            None,
        );
        let direct_area = entity(
            &registries,
            "light",
            "direct_area",
            None,
            Some(&kitchen.id),
            &[],
            None,
        );
        let direct_labeled = entity(
            &registries,
            "sensor",
            "direct_labeled",
            None,
            None,
            &[&label.label_id],
            Some("diagnostic"),
        );

        let mut area_call = call(Map::new(), value_map(json!({"area_id": kitchen.id})));
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut area_call).unwrap()),
            vec![direct_area.entity_id.clone(), inherited.entity_id.clone()]
        );

        let mut floor_call = call(Map::new(), value_map(json!({"floor_id": floor.floor_id})));
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut floor_call).unwrap()),
            vec![direct_area.entity_id.clone(), inherited.entity_id.clone()]
        );

        let mut device_call = call(Map::new(), value_map(json!({"device_id": device.id})));
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut device_call).unwrap()),
            vec![inherited.entity_id.clone(), override_area.entity_id.clone()]
        );

        let mut label_call = call(Map::new(), value_map(json!({"label_id": label.label_id})));
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut label_call).unwrap()),
            vec![
                direct_area.entity_id.clone(),
                inherited.entity_id.clone(),
                override_area.entity_id.clone(),
                direct_labeled.entity_id.clone(),
            ]
        );
    }

    #[test]
    fn excludes_hidden_disabled_and_indirect_categories_but_keeps_direct_references() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let label = registries.create_label(LabelInput::new("Test")).unwrap();
        let area = registries.create_area(AreaInput::new("Room")).unwrap();
        let hidden = entity(
            &registries,
            "light",
            "hidden",
            None,
            Some(&area.id),
            &[],
            None,
        );
        registries
            .update_entity(
                &hidden.entity_id,
                EntityUpdate {
                    hidden_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        let disabled = entity(
            &registries,
            "light",
            "disabled",
            None,
            Some(&area.id),
            &[],
            None,
        );
        registries
            .update_entity(
                &disabled.entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("integration".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        let category = entity(
            &registries,
            "light",
            "category",
            None,
            Some(&area.id),
            &[],
            Some("diagnostic"),
        );
        let direct_category_label = entity(
            &registries,
            "sensor",
            "category_label",
            None,
            None,
            &[&label.label_id],
            Some("diagnostic"),
        );

        let mut indirect = call(Map::new(), value_map(json!({"area_id": area.id})));
        assert_eq!(
            resolve_service_targets(&runtime, Some(&registries), &mut indirect).unwrap(),
            TargetResolution::NoMatches
        );

        let mut direct_hidden = call(
            Map::new(),
            value_map(json!({"entity_id": hidden.entity_id})),
        );
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut direct_hidden).unwrap()),
            vec![hidden.entity_id]
        );

        let mut direct_label = call(Map::new(), value_map(json!({"label_id": label.label_id})));
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut direct_label).unwrap()),
            vec![direct_category_label.entity_id.clone()]
        );
        assert_ne!(category.entity_id, direct_category_label.entity_id);
    }

    #[test]
    fn normalizes_explicit_ids_and_uuid_without_filtering_by_service_domain() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let entry = entity(&registries, "sensor", "temperature", None, None, &[], None);
        let mut call = call(
            Map::new(),
            value_map(json!({"entity_id": ["SENSOR.TEMPERATURE", entry.id]})),
        );
        let resolved = resolve_service_targets(&runtime, Some(&registries), &mut call).unwrap();
        assert_eq!(
            resolved,
            TargetResolution::Matched(vec!["sensor.temperature".into(), entry.entity_id.clone(),])
        );
        assert_eq!(
            call.data["entity_id"],
            json!(["sensor.temperature", entry.entity_id])
        );
        assert!(call.target.is_empty());
    }

    #[test]
    fn unknown_targets_are_empty_but_valid_explicit_ids_remain_referenced() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let mut unknown_area = call(Map::new(), value_map(json!({"area_id": "missing-area"})));
        assert_eq!(
            resolve_service_targets(&runtime, Some(&registries), &mut unknown_area).unwrap(),
            TargetResolution::NoMatches
        );
        assert!(!unknown_area.data.contains_key("entity_id"));
        assert!(unknown_area.target.is_empty());

        let mut empty = call(Map::new(), value_map(json!({"entity_id": []})));
        assert_eq!(
            resolve_service_targets(&runtime, Some(&registries), &mut empty).unwrap(),
            TargetResolution::NoMatches
        );
        assert!(!empty.data.contains_key("entity_id"));

        let mut explicit = call(
            Map::new(),
            value_map(json!({"entity_id": "sensor.not_registered"})),
        );
        assert_eq!(
            ids(resolve_service_targets(&runtime, Some(&registries), &mut explicit).unwrap()),
            vec!["sensor.not_registered"]
        );
    }

    #[test]
    fn rejects_malformed_or_conflicting_selectors_without_mutating_the_call() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let mut malformed = call(
            Map::new(),
            value_map(json!({"entity_id": ["not-an-entity"]})),
        );
        let original = malformed.clone();
        assert!(matches!(
            resolve_service_targets(&runtime, Some(&registries), &mut malformed),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(malformed, original);

        let mut non_string = call(Map::new(), value_map(json!({"area_id": [4]})));
        assert!(matches!(
            resolve_service_targets(&runtime, Some(&registries), &mut non_string),
            Err(Error::InvalidInput(_))
        ));

        let mut conflict = call(
            value_map(json!({"entity_id": "light.one"})),
            value_map(json!({"entity_id": "light.two"})),
        );
        assert!(matches!(
            resolve_service_targets(&runtime, Some(&registries), &mut conflict),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn no_target_keeps_legacy_data_and_missing_registry_never_broadens() {
        let runtime = runtime();
        let payload = value_map(json!({"brightness": 123}));
        let mut legacy = call(payload.clone(), Map::new());
        assert_eq!(
            resolve_service_targets(&runtime, None, &mut legacy).unwrap(),
            TargetResolution::Unspecified
        );
        assert_eq!(legacy.data, payload);

        let mut direct = call(
            Map::new(),
            value_map(json!({"entity_id": "light.explicit"})),
        );
        assert_eq!(
            resolve_service_targets(&runtime, None, &mut direct).unwrap(),
            TargetResolution::Matched(vec!["light.explicit".into()])
        );

        let mut indirect = call(Map::new(), value_map(json!({"area_id": "room"})));
        assert_eq!(
            resolve_service_targets(&runtime, None, &mut indirect).unwrap(),
            TargetResolution::NoMatches
        );
        assert!(!indirect.data.contains_key("entity_id"));
    }

    #[test]
    fn all_resolves_current_domain_states_and_keeps_registryless_helpers() {
        let runtime = runtime();
        runtime
            .set_state("light.one", "on", Map::new(), Context::default(), false)
            .unwrap();
        runtime
            .set_state("sensor.other", "on", Map::new(), Context::default(), false)
            .unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let disabled = entity(&registries, "light", "disabled", None, None, &[], None);
        registries
            .update_entity(
                &disabled.entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("integration".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        runtime
            .set_state(
                &disabled.entity_id,
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let hidden = entity(&registries, "light", "hidden", None, None, &[], None);
        registries
            .update_entity(
                &hidden.entity_id,
                EntityUpdate {
                    hidden_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        runtime
            .set_state(
                &hidden.entity_id,
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let category = entity(
            &registries,
            "light",
            "category",
            None,
            None,
            &[],
            Some("diagnostic"),
        );
        runtime
            .set_state(
                &category.entity_id,
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();

        let mut all = call(Map::new(), value_map(json!({"entity_id": "all"})));
        assert_eq!(
            resolve_service_targets(&runtime, Some(&registries), &mut all).unwrap(),
            TargetResolution::Matched(vec!["light.one".into()])
        );

        let empty_runtime = Runtime::new(Arc::new(MemoryStore::default())).expect("runtime");
        empty_runtime
            .set_state(
                "light.native_helper",
                "on",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let mut empty_all = call(Map::new(), value_map(json!({"entity_id": "all"})));
        assert_eq!(
            resolve_service_targets(&empty_runtime, None, &mut empty_all).unwrap(),
            TargetResolution::Matched(vec!["light.native_helper".into()])
        );
        assert_eq!(empty_all.data["entity_id"], json!(["light.native_helper"]));
    }

    #[test]
    fn conditional_wrapper_preserves_arbitrary_payloads_and_resolves_entity_domains() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();

        let mut rest_command = call(
            json!({"entity_id": "template.variable", "area_id": "payload-value"})
                .as_object()
                .unwrap()
                .clone(),
            Map::new(),
        );
        rest_command.domain = "rest_command".into();
        let original = rest_command.clone();
        assert_eq!(
            resolve_entity_service_targets(&runtime, Some(&registries), &mut rest_command).unwrap(),
            TargetResolution::Unspecified
        );
        assert_eq!(rest_command, original);

        let mut script_variables = call(
            json!({"entity_id": "template.variable", "area_id": "payload-value"})
                .as_object()
                .unwrap()
                .clone(),
            Map::new(),
        );
        script_variables.domain = "script".into();
        script_variables.service = "custom_action".into();
        let original = script_variables.clone();
        assert_eq!(
            resolve_entity_service_targets(&runtime, Some(&registries), &mut script_variables)
                .unwrap(),
            TargetResolution::Unspecified
        );
        assert_eq!(script_variables, original);

        let mut script_action = call(
            json!({"entity_id": "light.explicit"})
                .as_object()
                .unwrap()
                .clone(),
            Map::new(),
        );
        script_action.domain = "script".into();
        assert_eq!(
            resolve_entity_service_targets(&runtime, Some(&registries), &mut script_action)
                .unwrap(),
            TargetResolution::Matched(vec!["light.explicit".into()])
        );

        let mut explicit_target = call(
            Map::new(),
            json!({"area_id": "missing-area"})
                .as_object()
                .unwrap()
                .clone(),
        );
        explicit_target.domain = "rest_command".into();
        assert_eq!(
            resolve_entity_service_targets(&runtime, Some(&registries), &mut explicit_target)
                .unwrap(),
            TargetResolution::NoMatches
        );
    }

    #[test]
    fn generic_all_selects_real_control_domains_without_group_or_self_recursion() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        for domain in ["light", "input_boolean", "homeassistant"] {
            runtime
                .register_service(
                    domain,
                    "turn_on",
                    json!({}),
                    Arc::new(|_, _| {
                        Err(Error::Service("selector must not execute a service".into()))
                    }),
                )
                .unwrap();
        }
        for id in [
            "light.one",
            "input_boolean.helper",
            "sensor.reading",
            "group.all_lights",
            "homeassistant.fake",
        ] {
            runtime
                .set_state(id, "off", Map::new(), Context::default(), false)
                .unwrap();
        }
        let hidden = entity(
            &registries,
            "light",
            "hidden_generic",
            None,
            None,
            &[],
            None,
        );
        registries
            .update_entity(
                &hidden.entity_id,
                EntityUpdate {
                    hidden_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        runtime
            .set_state(
                &hidden.entity_id,
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let mut all = call(Map::new(), value_map(json!({"entity_id":"all"})));
        all.domain = "homeassistant".into();
        assert_eq!(
            resolve_entity_service_targets(&runtime, Some(&registries), &mut all).unwrap(),
            TargetResolution::Matched(vec!["input_boolean.helper".into(), "light.one".into()])
        );
    }
}
