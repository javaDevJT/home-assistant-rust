//! Transport adapter for the native editable zone collection.

use hass_core::{Error, Result};
use hass_zone::{ZoneConfig, ZoneManager, ZoneUpdate};
use serde_json::{Map, Value};

pub(crate) async fn handle(manager: ZoneManager, command: &Value) -> Result<Value> {
    let command = command.clone();
    tokio::task::spawn_blocking(move || handle_sync(&manager, &command))
        .await
        .map_err(|_| Error::Service("Zone task failed".into()))?
}

fn handle_sync(manager: &ZoneManager, command: &Value) -> Result<Value> {
    let object = command
        .as_object()
        .ok_or_else(|| Error::InvalidInput("Zone command must be an object".into()))?;
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    let mut fields: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    match kind {
        "zone/list" => {
            require_empty(&fields)?;
            encode(manager.list()?)
        }
        "zone/create" => {
            if fields.values().any(Value::is_null) {
                return Err(Error::InvalidInput("Zone fields cannot be null".into()));
            }
            let config: ZoneConfig = serde_json::from_value(Value::Object(fields))
                .map_err(|_| Error::InvalidInput("Invalid zone creation fields".into()))?;
            encode(manager.create(config)?)
        }
        "zone/update" => {
            let id = take_zone_id(&mut fields)?;
            if fields.values().any(Value::is_null) {
                return Err(Error::InvalidInput("Zone fields cannot be null".into()));
            }
            let update: ZoneUpdate = serde_json::from_value(Value::Object(fields))
                .map_err(|_| Error::InvalidInput("Invalid zone update fields".into()))?;
            encode(manager.update(&id, update)?)
        }
        "zone/delete" => {
            let id = take_zone_id(&mut fields)?;
            require_empty(&fields)?;
            manager.delete(&id)?;
            Ok(Value::Null)
        }
        _ => Err(Error::InvalidInput("Unsupported zone command".into())),
    }
}

fn take_zone_id(fields: &mut Map<String, Value>) -> Result<String> {
    fields
        .remove("zone_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::InvalidInput("zone_id must be a nonempty string".into()))
}

fn require_empty(fields: &Map<String, Value>) -> Result<()> {
    if fields.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidInput("Unexpected zone command fields".into()))
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| Error::Service("Cannot serialize zone".into()))
}
