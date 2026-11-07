//! WebSocket adapter for Home Assistant's Network integration commands.
//!
//! This module intentionally stays separate from [`crate::network`], which
//! resolves HTTP client origin and trusted proxy policy.

use crate::{AppState, extensions::Identity};
use hass_core::{Error, Result};
use hass_network::{NetworkAdapter, NetworkManager};
use serde_json::{Map, Value, json};

/// Handle one of the three pinned Network WebSocket commands.
///
/// The WebSocket dispatcher applies the command-specific admin policy before
/// this function is called. All commands arrive through an authenticated
/// WebSocket connection; `network/url` is available to every authenticated
/// identity.
pub(crate) async fn handle(
    state: &AppState,
    command: &Value,
    _identity: &Identity,
) -> Result<Value> {
    let kind = command
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(invalid_command)?;

    match kind {
        "network" => {
            validate_envelope(command, kind, &["id", "type"])?;
            let manager = network_manager(state)?;
            let (adapters, configured_adapters) =
                run_blocking(move || Ok((manager.adapters()?, manager.configured_adapters()?)))
                    .await?;
            Ok(json!({
                "adapters": adapters.iter().map(adapter_to_json).collect::<Vec<_>>(),
                "configured_adapters": configured_adapters,
            }))
        }
        "network/configure" => {
            validate_envelope(command, kind, &["id", "type", "config"])?;
            let configured_adapters = parse_configured_adapters(command)?;
            let manager = network_manager(state)?;
            let configured_adapters =
                run_blocking(move || manager.configure(configured_adapters)).await?;
            Ok(json!({ "configured_adapters": configured_adapters }))
        }
        "network/url" => {
            validate_envelope(command, kind, &["id", "type"])?;
            let (internal, external) = match state.features.config_entries.clone() {
                Some(entries) => {
                    let config = run_blocking(move || Ok(entries.core_config())).await?;
                    (config.internal_url, config.external_url)
                }
                None => (None, None),
            };
            Ok(json!({
                "internal": internal,
                "external": external,
                // No cloud URL provider is present in this application.
                "cloud": Value::Null,
            }))
        }
        _ => Err(invalid_command()),
    }
}

fn network_manager(state: &AppState) -> Result<NetworkManager> {
    state
        .features
        .network_adapters
        .clone()
        .ok_or_else(|| Error::NotFound("Network integration is not configured".into()))
}

fn validate_envelope<'a>(
    command: &'a Value,
    kind: &str,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>> {
    let object = command.as_object().ok_or_else(invalid_command)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || object
            .get("id")
            .and_then(Value::as_u64)
            .is_none_or(|id| id == 0)
        || object.get("type").and_then(Value::as_str) != Some(kind)
    {
        return Err(invalid_command());
    }
    Ok(object)
}

fn parse_configured_adapters(command: &Value) -> Result<Vec<String>> {
    let Some(config) = command.get("config") else {
        return Ok(Vec::new());
    };
    let config = config.as_object().ok_or_else(invalid_command)?;
    if config.keys().any(|key| key != "configured_adapters") {
        return Err(invalid_command());
    }
    let Some(value) = config.get("configured_adapters") else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(invalid_command)?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(invalid_command)
        })
        .collect()
}

async fn run_blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Error::Service("Network adapter task failed".into()))?
        .map_err(|error| match error {
            Error::NotFound(_) => Error::NotFound("Network integration is unavailable".into()),
            _ => {
                tracing::error!(%error, "Network adapter operation failed");
                Error::Service("Network adapter operation failed".into())
            }
        })
}

fn adapter_to_json(adapter: &NetworkAdapter) -> Value {
    json!({
        "name": adapter.name,
        "index": adapter.index,
        "enabled": adapter.enabled,
        "auto": adapter.auto,
        "default": adapter.default,
        "ipv4": adapter.ipv4.iter().map(|record| json!({
            "address": record.address.to_string(),
            "network_prefix": record.network_prefix,
        })).collect::<Vec<_>>(),
        "ipv6": adapter.ipv6.iter().map(|record| json!({
            "address": record.address.to_string(),
            "flowinfo": record.flowinfo,
            "scope_id": record.scope_id,
            "network_prefix": record.network_prefix,
        })).collect::<Vec<_>>(),
    })
}

fn invalid_command() -> Error {
    Error::InvalidInput("Invalid network command".into())
}
