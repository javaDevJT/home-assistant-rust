//! Native tag collection WebSocket transport.

use hass_core::{Error, Result};
use hass_tag::{TagCreate, TagManager, TagUpdate};
use serde_json::{Map, Value};

/// Handle the source-compatible tag collection CRUD commands.
///
/// Subscription setup is owned by `NativeSubscriptions`, which uses
/// `TagManager::snapshot_and_subscribe` to avoid a gap between the initial list
/// and private collection deltas. The tag integration intentionally exposes no
/// scan WebSocket command or HTTP/QR endpoint.
pub(crate) async fn handle(manager: TagManager, command: &Value) -> Result<Value> {
    let command = command.clone();
    tokio::task::spawn_blocking(move || handle_sync(&manager, &command))
        .await
        .map_err(|_| Error::Service("Tag task failed".into()))?
}

fn handle_sync(manager: &TagManager, command: &Value) -> Result<Value> {
    let object = command
        .as_object()
        .ok_or_else(|| Error::InvalidInput("Tag command must be an object".into()))?;
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    let mut fields: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    match kind {
        "tag/list" => {
            require_empty(&fields)?;
            encode(manager.list()?)
        }
        "tag/create" => {
            reject_nulls(&fields)?;
            coerce_strings(&mut fields, &["tag_id", "description", "device_id"])?;
            let create: TagCreate = serde_json::from_value(Value::Object(fields))
                .map_err(|_| Error::InvalidInput("Invalid tag creation fields".into()))?;
            encode(manager.create(create)?)
        }
        "tag/update" => {
            let id = take_tag_id(&mut fields)?;
            reject_nulls(&fields)?;
            coerce_strings(&mut fields, &["description", "device_id"])?;
            let update: TagUpdate = serde_json::from_value(Value::Object(fields))
                .map_err(|_| Error::InvalidInput("Invalid tag update fields".into()))?;
            encode(manager.update(&id, update)?)
        }
        "tag/delete" => {
            let id = take_tag_id(&mut fields)?;
            require_empty(&fields)?;
            manager.delete(&id)?;
            Ok(Value::Null)
        }
        _ => Err(Error::InvalidInput("Unsupported tag command".into())),
    }
}

fn coerce_strings(fields: &mut Map<String, Value>, names: &[&str]) -> Result<()> {
    // These fields use cv.string in the pinned collection schema. Names and
    // update/delete IDs use strict strings instead.
    for name in names {
        if let Some(value) = fields.get_mut(*name) {
            let string = match value {
                Value::String(value) => value.clone(),
                Value::Bool(value) => if *value { "True" } else { "False" }.into(),
                Value::Number(value) if value.is_f64() => {
                    let number = value
                        .as_f64()
                        .ok_or_else(|| Error::InvalidInput(format!("Invalid {name} number")))?;
                    let formatted = format!("{number:?}");
                    if let Some((base, exponent)) = formatted.split_once('e') {
                        let exponent = exponent
                            .parse::<i32>()
                            .map_err(|_| Error::InvalidInput(format!("Invalid {name} number")))?;
                        format!(
                            "{base}e{}{abs:02}",
                            if exponent < 0 { "-" } else { "+" },
                            abs = exponent.unsigned_abs()
                        )
                    } else {
                        formatted
                    }
                }
                Value::Number(value) => value.to_string(),
                _ => {
                    return Err(Error::InvalidInput(format!(
                        "{name} must be a scalar string value"
                    )));
                }
            };
            *value = Value::String(string);
        }
    }
    Ok(())
}

fn take_tag_id(fields: &mut Map<String, Value>) -> Result<String> {
    // Home Assistant's collection schema accepts any string, including an
    // empty one. Empty update/delete IDs therefore resolve as not found.
    fields
        .remove("tag_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| Error::InvalidInput("tag_id must be a string".into()))
}

fn require_empty(fields: &Map<String, Value>) -> Result<()> {
    if fields.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidInput("Unexpected tag command fields".into()))
    }
}

fn reject_nulls(fields: &Map<String, Value>) -> Result<()> {
    if fields.values().any(Value::is_null) {
        Err(Error::InvalidInput("Tag fields cannot be null".into()))
    } else {
        Ok(())
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| Error::Service("Cannot serialize tag".into()))
}
