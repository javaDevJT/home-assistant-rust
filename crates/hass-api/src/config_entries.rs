//! HTTP adapters for native config-entry and flow managers.

use crate::{AppState, extensions::Identity, http::ApiError};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hass_config::ConfigEntries;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(crate) struct EntryFilter {
    domain: Option<String>,
    #[serde(rename = "type")]
    integration_type: Option<String>,
}

#[derive(Default, Deserialize)]
pub(crate) struct FlowHandlerFilter {
    #[serde(rename = "type")]
    integration_type: Option<String>,
}

fn require_admin(identity: &Identity) -> Result<(), ApiError> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn manager_unavailable() -> ApiError {
    ApiError {
        status: StatusCode::NOT_IMPLEMENTED,
        message: "Config-entry support is not enabled.".into(),
    }
}

async fn run_manager<T, F>(state: &AppState, operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(ConfigEntries) -> hass_core::Result<T> + Send + 'static,
{
    let manager = state
        .features
        .config_entries
        .clone()
        .ok_or_else(manager_unavailable)?;
    tokio::task::spawn_blocking(move || operation(manager))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(ApiError::from)
}

fn parse_object(body: &Bytes, allow_empty: bool) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() && allow_empty {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::invalid("Request body must be valid JSON"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::invalid("Request body must be a JSON object"))
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, ApiError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::invalid(format!("Missing or invalid '{key}'")))
}

fn parse_flow_handlers(value: Option<&Value>) -> Result<Vec<String>, ApiError> {
    match value {
        Some(Value::String(domain)) => Ok(vec![domain.clone()]),
        Some(Value::Array(handlers)) if !handlers.is_empty() => handlers
            .iter()
            .map(|handler| {
                handler
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ApiError::invalid("'handler' must contain integration domains"))
            })
            .collect(),
        Some(Value::Array(_)) => Err(ApiError::invalid("'handler' must not be empty")),
        _ => Err(ApiError::invalid("Missing or invalid 'handler'")),
    }
}

/// GET `/api/config/config_entries/entry`
pub(crate) async fn get_entries(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<EntryFilter>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let domain = filter.domain.filter(|domain| !domain.is_empty());
    let integration_type = filter.integration_type;
    let entries = run_manager(&state, move |manager| {
        let entries = manager.list_entries(domain.as_deref());
        let fragments = entries
            .into_iter()
            .filter(|entry| {
                let Some(integration_type) = integration_type.as_deref() else {
                    return true;
                };
                match manager.driver(&entry.domain) {
                    Some(driver) => driver.integration_type() == integration_type,
                    // Home Assistant keeps unknown integrations in non-helper
                    // lists for compatibility, while excluding them from helpers.
                    None => integration_type != "helper",
                }
            })
            .map(|entry| entry.public_json())
            .collect();
        Ok(fragments)
    })
    .await?;
    Ok(Json(entries))
}

/// DELETE `/api/config/config_entries/entry/{entry_id}`
pub(crate) async fn delete_entry(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    run_manager(&state, move |manager| {
        manager.remove_entry(&entry_id)?;
        Ok(json!(true))
    })
    .await
    .map(Json)
}

/// POST `/api/config/config_entries/entry/{entry_id}/reload`
pub(crate) async fn reload_entry(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    run_manager(&state, move |manager| {
        manager.reload_entry(&entry_id)?;
        // A successful native reload has already run the registered unload and
        // setup callbacks, so no host restart is required.
        Ok(json!({"require_restart": false}))
    })
    .await
    .map(Json)
}

/// POST `/api/config/config_entries/flow`
pub(crate) async fn init_flow(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    let data = parse_object(&body, false)?;
    let domains = parse_flow_handlers(data.get("handler"))?;
    let entry_id = data
        .get("entry_id")
        .map(|_| required_string(&data, "entry_id"))
        .transpose()?;
    let source = if entry_id.is_some() {
        "reconfigure"
    } else {
        "user"
    }
    .to_owned();
    let mut context = Map::new();
    if let Some(entry_id) = entry_id {
        context.insert("entry_id".into(), json!(entry_id));
    }

    let flow = run_manager(&state, move |manager| {
        let Some(domain) = domains
            .iter()
            .find(|domain| manager.driver(domain).is_some())
        else {
            return Err(hass_core::Error::InvalidInput(format!(
                "Failed dependencies {}",
                domains.join(", ")
            )));
        };
        manager.init_config_flow_with_input(domain, &source, context, None)
    })
    .await?;
    Ok(Json(flow))
}

/// GET `/api/config/config_entries/flow/{flow_id}` and options-flow equivalent.
pub(crate) async fn get_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    run_manager(&state, move |manager| manager.flow_result(&flow_id))
        .await
        .map(Json)
}

/// POST `/api/config/config_entries/flow/{flow_id}` and options-flow equivalent.
pub(crate) async fn submit_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    let user_input = parse_object(&body, true)?;
    run_manager(&state, move |manager| {
        manager.submit_flow(&flow_id, user_input)
    })
    .await
    .map(Json)
}

/// DELETE `/api/config/config_entries/flow/{flow_id}` and options-flow equivalent.
pub(crate) async fn abort_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    run_manager(&state, move |manager| {
        manager.abort_flow(&flow_id)?;
        Ok(json!({"message": "Flow aborted"}))
    })
    .await
    .map(Json)
}

/// GET `/api/config/config_entries/flow_handlers`
pub(crate) async fn flow_handlers(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<FlowHandlerFilter>,
) -> Result<Json<Vec<String>>, ApiError> {
    let integration_type = filter.integration_type;
    let handlers = run_manager(&state, move |manager| {
        let handlers = manager.flow_handlers();
        Ok(handlers
            .into_iter()
            .filter(|domain| {
                integration_type.as_deref().is_none_or(|integration_type| {
                    manager
                        .driver(domain)
                        .is_some_and(|driver| driver.integration_type() == integration_type)
                })
            })
            .collect())
    })
    .await?;
    Ok(Json(handlers))
}

/// POST `/api/config/config_entries/options/flow`
pub(crate) async fn init_options_flow(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_admin(&identity)?;
    let data = parse_object(&body, false)?;
    let entry_id = match data.get("handler") {
        Some(Value::String(entry_id)) => entry_id.clone(),
        _ => required_string(&data, "entry_id")?,
    };
    run_manager(&state, move |manager| manager.init_options_flow(&entry_id))
        .await
        .map(Json)
}
