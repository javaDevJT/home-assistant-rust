//! Source logger commands backed by the actual dynamic tracing policy.
use hass_core::{Error, Result};
use hass_logger::{
    IntegrationCatalogProvider, LoggerLevel, LoggerManager, Persistence, TargetKind,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleUpdate {
    module: String,
    level: String,
    persistence: Persistence,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrationUpdate {
    integration: String,
    level: String,
    persistence: Persistence,
}

pub(crate) async fn handle(
    manager: LoggerManager,
    catalog: IntegrationCatalogProvider,
    command: &Value,
) -> Result<Value> {
    let command = command.clone();
    tokio::task::spawn_blocking(move || {
        let object = command
            .as_object()
            .ok_or_else(|| Error::InvalidInput("Logger command must be an object".into()))?;
        let fields: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        match object.get("type").and_then(Value::as_str) {
            Some("logger/log_info") => {
                if !fields.is_empty() {
                    return Err(Error::InvalidInput("Unexpected logger info fields".into()));
                }
                serde_json::to_value(manager.log_info(&catalog())?)
                    .map_err(|_| Error::Service("Cannot serialize logger info".into()))
            }
            Some("logger/log_level") => {
                let update: ModuleUpdate = serde_json::from_value(Value::Object(fields))
                    .map_err(|_| Error::InvalidInput("Invalid module logger fields".into()))?;
                manager.update_settings(
                    &update.module,
                    TargetKind::Module,
                    LoggerLevel::parse(&update.level)?,
                    update.persistence,
                )?;
                Ok(Value::Null)
            }
            Some("logger/integration_log_level") => {
                let update: IntegrationUpdate = serde_json::from_value(Value::Object(fields))
                    .map_err(|_| Error::InvalidInput("Invalid integration logger fields".into()))?;
                manager.update_settings(
                    &update.integration,
                    TargetKind::Integration,
                    LoggerLevel::parse(&update.level)?,
                    update.persistence,
                )?;
                Ok(Value::Null)
            }
            _ => Err(Error::InvalidInput("Unsupported logger command".into())),
        }
    })
    .await
    .map_err(|_| Error::Service("Logger command task failed".into()))?
}
