//! Home Assistant registry websocket command payloads.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use crate::{
    AreaInput, AreaUpdate, DeviceUpdate, EntityUpdate, FloorInput, FloorUpdate, LabelInput,
    LabelUpdate, Registries, Result, invalid,
};
use hass_core::Error;

pub(super) fn handle(registries: &Registries, command: &Value) -> Result<Value> {
    let object = command
        .as_object()
        .ok_or_else(|| invalid("registry command must be a JSON object"))?;
    let command_type = required_string(object, "type")?;
    match command_type {
        "config/entity_registry/list" => {
            check_fields(object, &["type", "id"])?;
            Ok(Value::Array(
                registries
                    .entities()
                    .iter()
                    .map(|entry| entry.websocket_value())
                    .collect(),
            ))
        }
        "config/entity_registry/list_for_display" => {
            check_fields(object, &["type", "id"])?;
            let entries = registries
                .entities()
                .into_iter()
                .filter(|entry| entry.disabled_by.is_none())
                .map(|entry| entry.display_value())
                .collect::<Vec<_>>();
            Ok(json!({"entity_categories":{"0":"config","1":"diagnostic"},"entities":entries}))
        }
        "config/entity_registry/get" => {
            check_fields(object, &["type", "id", "entity_id"])?;
            let entity_id = required_string(object, "entity_id")?;
            registries
                .entity(entity_id)
                .map(|entry| entry.websocket_value())
                .ok_or_else(|| Error::NotFound(format!("entity {entity_id}")))
        }
        "config/entity_registry/get_entries" => {
            check_fields(object, &["type", "id", "entity_ids"])?;
            let mut result = Map::new();
            for entity_id in string_array(object, "entity_ids")? {
                let value = registries
                    .entity(&entity_id)
                    .map(|entry| entry.websocket_value())
                    .unwrap_or(Value::Null);
                result.insert(entity_id, value);
            }
            Ok(Value::Object(result))
        }
        "config/entity_registry/update" => entity_update(registries, object),
        "config/entity_registry/remove" => {
            check_fields(object, &["type", "id", "entity_id"])?;
            registries.remove_entity(required_string(object, "entity_id")?)?;
            Ok(Value::Null)
        }
        "config/entity_registry/get_automatic_entity_ids" => {
            check_fields(object, &["type", "id", "entity_ids"])?;
            let mut result = Map::new();
            let mut reserved = HashSet::new();
            let entries = registries.entities();
            for entity_id in string_array(object, "entity_ids")? {
                let Some(entry) = entries.iter().find(|entry| entry.entity_id == entity_id) else {
                    result.insert(entity_id, Value::Null);
                    continue;
                };
                let fallback = format!("{}_{}", entry.platform, entry.unique_id);
                let base = entry
                    .name
                    .as_deref()
                    .or(entry.suggested_object_id.as_deref())
                    .or(entry.original_name.as_deref())
                    .unwrap_or(&fallback);
                let stem = format!("{}.{}", entry.domain, crate::slugify(base));
                let mut candidate = stem.clone();
                let mut suffix = 2;
                while entries.iter().any(|registered| {
                    registered.entity_id == candidate && registered.entity_id != entry.entity_id
                }) || reserved.contains(&candidate)
                {
                    candidate = format!("{stem}_{suffix}");
                    suffix += 1;
                }
                reserved.insert(candidate.clone());
                result.insert(entity_id, Value::String(candidate));
            }
            Ok(Value::Object(result))
        }
        "config/entity_registry/settings/get" => {
            check_fields(object, &["type", "id"])?;
            let data = registries.snapshot();
            Ok(json!({"entity_id_parts":data.entity_id_parts}))
        }
        "config/entity_registry/settings/update" => {
            check_fields(object, &["type", "id", "entity_id_parts"])?;
            let Some(raw) = object.get("entity_id_parts") else {
                let data = registries.snapshot();
                return Ok(json!({"entity_id_parts":data.entity_id_parts}));
            };
            let value = if raw.is_null() {
                None
            } else {
                let items = raw
                    .as_array()
                    .ok_or_else(|| invalid("entity_id_parts must be an array or null"))?;
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    let value = item
                        .as_str()
                        .ok_or_else(|| invalid("entity_id_parts must contain strings"))?;
                    if !["area", "parent_device", "device", "entity", "floor"].contains(&value) {
                        return Err(invalid("unsupported entity_id_parts value"));
                    }
                    parts.push(value.to_owned());
                }
                let unique: HashSet<_> = parts.iter().collect();
                if unique.len() != parts.len()
                    || !parts.iter().any(|part| part == "entity")
                    || !parts.iter().any(|part| part == "device")
                {
                    return Err(invalid(
                        "entity_id_parts must be unique and include entity and device",
                    ));
                }
                Some(parts)
            };
            registries.transact(|data| {
                data.entity_id_parts = value.clone();
                Ok(((), Vec::new()))
            })?;
            Ok(json!({"entity_id_parts":value}))
        }
        "config/device_registry/list" => {
            check_fields(object, &["type", "id"])?;
            Ok(Value::Array(
                registries
                    .devices()
                    .iter()
                    .map(|entry| entry.websocket_value())
                    .collect(),
            ))
        }
        "config/device_registry/list_composite_splits" => {
            check_fields(object, &["type", "id"])?;
            Ok(json!({}))
        }
        "config/device_registry/list_linked_devices" => {
            check_fields(object, &["type", "id", "device_id"])?;
            let device_id = required_string(object, "device_id")?;
            let devices = registries.devices();
            let device = devices
                .iter()
                .find(|entry| entry.id == device_id)
                .ok_or_else(|| Error::NotFound(format!("device {device_id}")))?;
            let linked = devices
                .iter()
                .filter(|entry| {
                    entry.id != device_id
                        && (entry
                            .identifiers
                            .iter()
                            .any(|identity| device.identifiers.contains(identity))
                            || entry
                                .connections
                                .iter()
                                .any(|identity| device.connections.contains(identity)))
                })
                .map(|entry| Value::String(entry.id.clone()))
                .collect::<Vec<_>>();
            Ok(json!({"linked_devices":linked}))
        }
        "config/device_registry/update" => device_update(registries, object),
        "config/device_registry/remove" => {
            check_fields(object, &["type", "id", "device_id"])?;
            registries.remove_device(required_string(object, "device_id")?)?;
            Ok(Value::Null)
        }
        "config/device_registry/remove_config_entry" => {
            check_fields(object, &["type", "id", "device_id", "config_entry_id"])?;
            registries.remove_config_entry_from_device(
                required_string(object, "device_id")?,
                required_string(object, "config_entry_id")?,
            )?;
            Ok(Value::Null)
        }
        "config/area_registry/list" => {
            check_fields(object, &["type", "id"])?;
            Ok(Value::Array(
                registries
                    .areas()
                    .iter()
                    .map(|entry| entry.websocket_value())
                    .collect(),
            ))
        }
        "config/area_registry/create" => {
            check_fields(
                object,
                &[
                    "type",
                    "id",
                    "name",
                    "aliases",
                    "floor_id",
                    "humidity_entity_id",
                    "icon",
                    "labels",
                    "picture",
                    "temperature_entity_id",
                ],
            )?;
            let mut input = AreaInput::new(required_string(object, "name")?);
            if let Some(value) = object.get("aliases") {
                input.aliases = string_array_value(value, "aliases")?.into_iter().collect();
            }
            if let Some(value) = object.get("floor_id") {
                input.floor_id = Some(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("floor_id must be a string"))?
                        .to_owned(),
                );
            }
            input.humidity_entity_id = optional_string(object, "humidity_entity_id")?;
            input.icon = optional_string(object, "icon")?;
            if let Some(value) = object.get("labels") {
                input.labels = string_array_value(value, "labels")?.into_iter().collect();
            }
            input.picture = optional_string(object, "picture")?;
            input.temperature_entity_id = optional_string(object, "temperature_entity_id")?;
            Ok(registries.create_area(input)?.websocket_value())
        }
        "config/area_registry/update" => area_update(registries, object),
        "config/area_registry/delete" => {
            check_fields(object, &["type", "id", "area_id"])?;
            registries.delete_area(required_string(object, "area_id")?)?;
            Ok(Value::String("success".into()))
        }
        "config/area_registry/reorder" => {
            check_fields(object, &["type", "id", "area_ids"])?;
            registries.reorder_areas(&string_array(object, "area_ids")?)?;
            Ok(Value::Null)
        }
        "config/floor_registry/list" => {
            check_fields(object, &["type", "id"])?;
            Ok(Value::Array(
                registries
                    .floors()
                    .iter()
                    .map(|entry| entry.websocket_value())
                    .collect(),
            ))
        }
        "config/floor_registry/create" => {
            check_fields(object, &["type", "id", "name", "aliases", "icon", "level"])?;
            let mut input = FloorInput::new(required_string(object, "name")?);
            if let Some(value) = object.get("aliases") {
                input.aliases = string_array_value(value, "aliases")?.into_iter().collect();
            }
            input.icon = optional_string(object, "icon")?;
            input.level = optional_i64(object, "level")?;
            Ok(registries.create_floor(input)?.websocket_value())
        }
        "config/floor_registry/update" => floor_update(registries, object),
        "config/floor_registry/delete" => {
            check_fields(object, &["type", "id", "floor_id"])?;
            registries.delete_floor(required_string(object, "floor_id")?)?;
            Ok(Value::Null)
        }
        "config/floor_registry/reorder" => {
            check_fields(object, &["type", "id", "floor_ids"])?;
            registries.reorder_floors(&string_array(object, "floor_ids")?)?;
            Ok(Value::Null)
        }
        "config/label_registry/list" => {
            check_fields(object, &["type", "id"])?;
            Ok(Value::Array(
                registries
                    .labels()
                    .iter()
                    .map(|entry| entry.websocket_value())
                    .collect(),
            ))
        }
        "config/label_registry/create" => {
            check_fields(
                object,
                &["type", "id", "name", "color", "description", "icon"],
            )?;
            let input = LabelInput {
                name: required_string(object, "name")?.to_owned(),
                color: optional_string(object, "color")?,
                description: optional_string(object, "description")?,
                icon: optional_string(object, "icon")?,
            };
            Ok(registries.create_label(input)?.websocket_value())
        }
        "config/label_registry/update" => label_update(registries, object),
        "config/label_registry/delete" => {
            check_fields(object, &["type", "id", "label_id"])?;
            registries.delete_label(required_string(object, "label_id")?)?;
            Ok(Value::Null)
        }
        other => Err(invalid(format!(
            "unsupported registry websocket command {other}"
        ))),
    }
}

fn entity_update(registries: &Registries, object: &Map<String, Value>) -> Result<Value> {
    check_fields(
        object,
        &[
            "type",
            "id",
            "entity_id",
            "aliases",
            "area_id",
            "categories",
            "device_class",
            "icon",
            "labels",
            "name",
            "new_entity_id",
            "disabled_by",
            "hidden_by",
            "options_domain",
            "options",
        ],
    )?;
    let entity_id = required_string(object, "entity_id")?;
    let mut update = EntityUpdate::default();
    if let Some(value) = object.get("aliases") {
        let aliases = value
            .as_array()
            .ok_or_else(|| invalid("aliases must be an array"))?
            .clone();
        if aliases
            .iter()
            .any(|alias| !alias.is_null() && !alias.is_string())
        {
            return Err(invalid("aliases must contain strings or null"));
        }
        update.aliases = Some(aliases);
    }
    update.area_id = optional_string_patch(object, "area_id")?;
    if let Some(value) = object.get("categories") {
        let categories = value
            .as_object()
            .ok_or_else(|| invalid("categories must be an object"))?;
        update.categories = Some(
            categories
                .iter()
                .map(|(key, value)| {
                    let value = if value.is_null() {
                        Ok(None)
                    } else {
                        value
                            .as_str()
                            .map(|value| Some(value.to_owned()))
                            .ok_or_else(|| invalid("category IDs must be strings or null"))
                    }?;
                    Ok((key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        );
    }
    update.device_class = optional_string_patch(object, "device_class")?;
    update.icon = optional_string_patch(object, "icon")?;
    if let Some(value) = object.get("labels") {
        update.labels = Some(string_array_value(value, "labels")?.into_iter().collect());
    }
    update.name = optional_string_patch(object, "name")?;
    if let Some(value) = object.get("new_entity_id") {
        update.new_entity_id = Some(
            value
                .as_str()
                .ok_or_else(|| invalid("new_entity_id must be a string"))?
                .to_owned(),
        );
    }
    if let Some(value) = object.get("disabled_by") {
        let disabled_by = if value.is_null() {
            None
        } else {
            let value = value
                .as_str()
                .ok_or_else(|| invalid("disabled_by must be user or null"))?;
            if value != "user" {
                return Err(invalid("disabled_by can only be user or null"));
            }
            Some(value.to_owned())
        };
        update.disabled_by = Some(disabled_by);
    }
    if let Some(value) = object.get("hidden_by") {
        let hidden_by = if value.is_null() {
            None
        } else {
            let value = value
                .as_str()
                .ok_or_else(|| invalid("hidden_by must be user or null"))?;
            if value != "user" {
                return Err(invalid("hidden_by can only be user or null"));
            }
            Some(value.to_owned())
        };
        update.hidden_by = Some(hidden_by);
    }
    if object.contains_key("options_domain") != object.contains_key("options") {
        return Err(invalid(
            "options_domain and options must be provided together",
        ));
    }
    if let Some(domain) = object.get("options_domain") {
        let domain = domain
            .as_str()
            .ok_or_else(|| invalid("options_domain must be a string"))?
            .to_owned();
        let options = object.get("options").expect("paired check");
        update.options = Some((domain, (!options.is_null()).then(|| options.clone())));
    }
    let was_disabled = registries
        .entity(entity_id)
        .ok_or_else(|| Error::NotFound(format!("entity {entity_id}")))?
        .disabled_by
        .is_some();
    let entry = registries
        .update_entity(entity_id, update)?
        .websocket_value();
    let mut result = Map::new();
    result.insert("entity_entry".into(), entry);
    let updated_entity_id = object
        .get("new_entity_id")
        .and_then(Value::as_str)
        .unwrap_or(entity_id);
    if was_disabled
        && registries
            .entity(updated_entity_id)
            .is_some_and(|entry| entry.disabled_by.is_none())
    {
        result.insert("require_restart".into(), Value::Bool(true));
    }
    Ok(Value::Object(result))
}

fn device_update(registries: &Registries, object: &Map<String, Value>) -> Result<Value> {
    check_fields(
        object,
        &[
            "type",
            "id",
            "area_id",
            "device_id",
            "disabled_by",
            "labels",
            "name_by_user",
        ],
    )?;
    let device_id = required_string(object, "device_id")?;
    let disabled_by = if let Some(value) = object.get("disabled_by") {
        let disabled = if value.is_null() {
            None
        } else {
            let disabled = value
                .as_str()
                .ok_or_else(|| invalid("disabled_by must be user or null"))?;
            if disabled != "user" {
                return Err(invalid("disabled_by can only be user or null"));
            }
            Some(disabled.to_owned())
        };
        Some(disabled)
    } else {
        None
    };
    let mut update = DeviceUpdate {
        area_id: optional_string_patch(object, "area_id")?,
        disabled_by,
        name_by_user: optional_string_patch(object, "name_by_user")?,
        ..DeviceUpdate::default()
    };
    if let Some(value) = object.get("labels") {
        update.labels = Some(string_array_value(value, "labels")?.into_iter().collect());
    }
    Ok(registries
        .update_device(device_id, update)?
        .websocket_value())
}

fn area_update(registries: &Registries, object: &Map<String, Value>) -> Result<Value> {
    check_fields(
        object,
        &[
            "type",
            "id",
            "area_id",
            "aliases",
            "floor_id",
            "humidity_entity_id",
            "icon",
            "labels",
            "name",
            "picture",
            "temperature_entity_id",
        ],
    )?;
    let area_id = required_string(object, "area_id")?;
    let mut update = AreaUpdate::default();
    if let Some(value) = object.get("aliases") {
        update.aliases = Some(string_array_value(value, "aliases")?.into_iter().collect());
    }
    update.floor_id = optional_string_patch(object, "floor_id")?;
    update.humidity_entity_id = optional_string_patch(object, "humidity_entity_id")?;
    update.icon = optional_string_patch(object, "icon")?;
    if let Some(value) = object.get("labels") {
        update.labels = Some(string_array_value(value, "labels")?.into_iter().collect());
    }
    if let Some(value) = object.get("name") {
        update.name = Some(
            value
                .as_str()
                .ok_or_else(|| invalid("name must be a string"))?
                .to_owned(),
        );
    }
    update.picture = optional_string_patch(object, "picture")?;
    update.temperature_entity_id = optional_string_patch(object, "temperature_entity_id")?;
    Ok(registries.update_area(area_id, update)?.websocket_value())
}

fn floor_update(registries: &Registries, object: &Map<String, Value>) -> Result<Value> {
    check_fields(
        object,
        &["type", "id", "floor_id", "aliases", "icon", "level", "name"],
    )?;
    let floor_id = required_string(object, "floor_id")?;
    let mut update = FloorUpdate::default();
    if let Some(value) = object.get("aliases") {
        update.aliases = Some(string_array_value(value, "aliases")?.into_iter().collect());
    }
    update.icon = optional_string_patch(object, "icon")?;
    update.level = optional_i64_patch(object, "level")?;
    if let Some(value) = object.get("name") {
        update.name = Some(
            value
                .as_str()
                .ok_or_else(|| invalid("name must be a string"))?
                .to_owned(),
        );
    }
    Ok(registries.update_floor(floor_id, update)?.websocket_value())
}

fn label_update(registries: &Registries, object: &Map<String, Value>) -> Result<Value> {
    check_fields(
        object,
        &[
            "type",
            "id",
            "label_id",
            "color",
            "description",
            "icon",
            "name",
        ],
    )?;
    let label_id = required_string(object, "label_id")?;
    let name = object
        .get("name")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("name must be a string"))
        })
        .transpose()?;
    let update = LabelUpdate {
        color: optional_string_patch(object, "color")?,
        description: optional_string_patch(object, "description")?,
        icon: optional_string_patch(object, "icon")?,
        name,
    };
    Ok(registries.update_label(label_id, update)?.websocket_value())
}

fn check_fields(object: &Map<String, Value>, fields: &[&str]) -> Result<()> {
    if let Some(unknown) = object.keys().find(|key| !fields.contains(&key.as_str())) {
        return Err(invalid(format!("unknown registry command field {unknown}")));
    }
    Ok(())
}
fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{key} is required and must be a string")))
}
fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(format!("{key} must be a string or null"))),
    }
}
fn optional_string_patch(object: &Map<String, Value>, key: &str) -> Result<Option<Option<String>>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(value.clone()))),
        Some(_) => Err(invalid(format!("{key} must be a string or null"))),
    }
}
fn optional_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be an integer or null"))),
    }
}
fn optional_i64_patch(object: &Map<String, Value>, key: &str) -> Result<Option<Option<i64>>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_i64()
            .map(|value| Some(Some(value)))
            .ok_or_else(|| invalid(format!("{key} must be an integer or null"))),
    }
}
fn string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let value = object
        .get(key)
        .ok_or_else(|| invalid(format!("{key} is required")))?;
    string_array_value(value, key)
}
fn string_array_value(value: &Value, key: &str) -> Result<Vec<String>> {
    let array = value
        .as_array()
        .ok_or_else(|| invalid(format!("{key} must be an array of strings")))?;
    array
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("{key} must contain only strings")))
        })
        .collect()
}
