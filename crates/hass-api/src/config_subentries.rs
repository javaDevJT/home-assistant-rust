//! HTTP and WebSocket adapters for native config-subentry flows.

use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, State},
    response::{IntoResponse, Response},
};
use hass_config::{ConfigEntries, ConfigFlowType, ConfigSubentryUpdate};
use hass_core::{Error, Result};
use serde_json::{Map, Value, json};

use crate::{AppState, extensions::Identity, http::ApiError};

pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;

pub(crate) type FlowApiResult<T> = std::result::Result<T, FlowApiError>;

pub(crate) enum FlowApiError {
    Api(ApiError),
    InvalidData(Map<String, Value>),
}

impl From<ApiError> for FlowApiError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl IntoResponse for FlowApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(error) => error.into_response(),
            Self::InvalidData(schema_errors) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "errors": schema_errors })),
            )
                .into_response(),
        }
    }
}

fn require_admin(identity: &Identity) -> ApiResult<()> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(ApiError {
            status: axum::http::StatusCode::UNAUTHORIZED,
            message: "Unauthorized".into(),
        })
    }
}

fn manager_unavailable() -> ApiError {
    ApiError {
        status: axum::http::StatusCode::NOT_IMPLEMENTED,
        message: "Config-entry support is not enabled.".into(),
    }
}

async fn run_manager<T, F>(state: &AppState, operation: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(ConfigEntries) -> Result<T> + Send + 'static,
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

async fn run_flow_manager<T, F>(state: &AppState, operation: F) -> FlowApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(ConfigEntries) -> Result<T> + Send + 'static,
{
    let manager = state
        .features
        .config_entries
        .clone()
        .ok_or_else(|| FlowApiError::Api(manager_unavailable()))?;
    let result = tokio::task::spawn_blocking(move || operation(manager))
        .await
        .map_err(|_| FlowApiError::Api(ApiError::internal()))?;
    result.map_err(|error| match error {
        Error::InvalidData { schema_errors } => FlowApiError::InvalidData(schema_errors),
        error => FlowApiError::Api(ApiError::from(error)),
    })
}

fn parse_object(body: &Bytes, allow_empty: bool) -> ApiResult<Map<String, Value>> {
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

fn required_handler_pair(object: &Map<String, Value>) -> ApiResult<(String, String)> {
    let Some(Value::Array(handler)) = object.get("handler") else {
        return Err(ApiError::invalid(
            "'handler' must be a [entry_id, subentry_type] pair",
        ));
    };
    let [entry_id, subentry_type] = handler.as_slice() else {
        return Err(ApiError::invalid(
            "'handler' must be a [entry_id, subentry_type] pair",
        ));
    };
    let (Some(entry_id), Some(subentry_type)) = (entry_id.as_str(), subentry_type.as_str()) else {
        return Err(ApiError::invalid(
            "'handler' must contain string entry_id and subentry_type values",
        ));
    };
    Ok((entry_id.to_owned(), subentry_type.to_owned()))
}

fn is_falsey(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number == 0.0),
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
    }
}

fn source_flow_result(mut result: Value) -> Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object.remove("context");
    if object.get("type").and_then(Value::as_str) == Some("abort") {
        object.insert("translation_domain".into(), json!("homeassistant"));
        object.insert("description_placeholders".into(), Value::Null);
    }
    result
}

fn invalid_flow(error: Error) -> Error {
    match error {
        Error::NotFound(_) => Error::NotFound("Invalid flow specified".into()),
        error => error,
    }
}

fn invalid_handler(error: Error) -> Error {
    match error {
        Error::NotFound(message)
            if message.starts_with("unknown config entry ")
                || message.starts_with("no native integration driver registered for ")
                || message.starts_with("config entry ") =>
        {
            Error::NotFound("Invalid handler specified".into())
        }
        error => error,
    }
}

/// POST `/api/config/config_entries/subentries/flow`.
pub(crate) async fn post_flow(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_admin(&identity)?;
    let data = parse_object(&body, false)?;
    let (entry_id, subentry_type) = required_handler_pair(&data)?;
    let subentry_id = match data.get("subentry_id") {
        Some(Value::String(subentry_id)) if !subentry_id.is_empty() => Some(subentry_id.clone()),
        Some(value) if is_falsey(value) => None,
        Some(_) => return Err(ApiError::invalid("'subentry_id' must be a string")),
        None => None,
    };
    let source = if subentry_id.is_some() {
        "reconfigure"
    } else {
        "user"
    };
    let result = run_manager(&state, move |manager| {
        manager
            .init_subentry_flow(
                &entry_id,
                &subentry_type,
                source,
                subentry_id.as_deref(),
                Map::new(),
                None,
            )
            .map_err(invalid_handler)
    })
    .await?;
    Ok(Json(source_flow_result(result)))
}

/// GET `/api/config/config_entries/subentries/flow/{flow_id}`.
pub(crate) async fn get_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    require_admin(&identity)?;
    let result = run_manager(&state, move |manager| {
        manager
            .active_flow(ConfigFlowType::ConfigSubentries, &flow_id)
            .map(|flow| flow.result)
            .map_err(invalid_flow)
    })
    .await?;
    Ok(Json(source_flow_result(result)))
}

/// POST `/api/config/config_entries/subentries/flow/{flow_id}`.
pub(crate) async fn submit_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> FlowApiResult<Json<Value>> {
    require_admin(&identity)?;
    let user_input = parse_object(&body, true)?;
    let result = run_flow_manager(&state, move |manager| {
        manager
            .active_flow(ConfigFlowType::ConfigSubentries, &flow_id)
            .map_err(invalid_flow)?;
        manager.submit_flow(&flow_id, user_input)
    })
    .await?;
    Ok(Json(source_flow_result(result)))
}

/// DELETE `/api/config/config_entries/subentries/flow/{flow_id}`.
///
/// The upstream resource inherits an authenticated-only DELETE handler; its
/// route authorization is applied by the HTTP middleware.
pub(crate) async fn delete_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> ApiResult<Json<Value>> {
    run_manager(&state, move |manager| {
        manager
            .active_flow(ConfigFlowType::ConfigSubentries, &flow_id)
            .map_err(invalid_flow)?;
        manager.abort_flow(&flow_id)
    })
    .await?;
    Ok(Json(json!({ "message": "Flow aborted" })))
}

fn valid_ws_fields(object: &Map<String, Value>, allowed: &[&str]) -> bool {
    object.keys().all(|key| allowed.contains(&key.as_str()))
}

fn required_ws_string(object: &Map<String, Value>, key: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput(format!("Missing or invalid '{key}'")))
}

fn handle_ws_sync(manager: ConfigEntries, command: &Value) -> Result<Value> {
    let object = command
        .as_object()
        .ok_or_else(|| Error::InvalidInput("Config subentry command must be an object".into()))?;
    let command_type = object.get("type").and_then(Value::as_str).ok_or_else(|| {
        Error::InvalidInput("Config subentry command type must be a string".into())
    })?;

    match command_type {
        "config_entries/subentries/list" => {
            if !valid_ws_fields(object, &["id", "type", "entry_id"]) {
                return Err(Error::InvalidInput(
                    "Invalid config subentry list command".into(),
                ));
            }
            let entry_id = required_ws_string(object, "entry_id")?;
            let entry = manager
                .get_entry(&entry_id)
                .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
            Ok(Value::Array(
                entry
                    .subentries
                    .into_iter()
                    .map(|subentry| {
                        json!({
                            "subentry_id": subentry.subentry_id,
                            "subentry_type": subentry.subentry_type,
                            "title": subentry.title,
                            "unique_id": subentry.unique_id,
                        })
                    })
                    .collect(),
            ))
        }
        "config_entries/subentries/update" => {
            if !valid_ws_fields(object, &["id", "type", "entry_id", "subentry_id", "title"]) {
                return Err(Error::InvalidInput(
                    "Invalid config subentry update command".into(),
                ));
            }
            let entry_id = required_ws_string(object, "entry_id")?;
            let subentry_id = required_ws_string(object, "subentry_id")?;
            if object.get("title").is_some_and(|value| !value.is_string()) {
                return Err(Error::InvalidInput("Missing or invalid 'title'".into()));
            }
            let entry = manager
                .get_entry(&entry_id)
                .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
            if !entry
                .subentries
                .iter()
                .any(|subentry| subentry.subentry_id == subentry_id)
            {
                return Err(Error::NotFound("Config subentry not found".into()));
            }
            let title = object
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);
            manager.update_subentry(
                &entry_id,
                &subentry_id,
                ConfigSubentryUpdate {
                    title,
                    unique_id: None,
                    data: None,
                },
            )?;
            Ok(Value::Null)
        }
        "config_entries/subentries/delete" => {
            if !valid_ws_fields(object, &["id", "type", "entry_id", "subentry_id"]) {
                return Err(Error::InvalidInput(
                    "Invalid config subentry delete command".into(),
                ));
            }
            let entry_id = required_ws_string(object, "entry_id")?;
            let subentry_id = required_ws_string(object, "subentry_id")?;
            let entry = manager
                .get_entry(&entry_id)
                .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
            if !entry
                .subentries
                .iter()
                .any(|subentry| subentry.subentry_id == subentry_id)
            {
                return Err(Error::NotFound("Config subentry not found".into()));
            }
            manager.remove_subentry(&entry_id, &subentry_id)?;
            Ok(Value::Null)
        }
        _ => unreachable!("only config subentry commands are dispatched here"),
    }
}

/// Handle one of the three pinned config-subentry WebSocket commands.
///
/// The WebSocket dispatcher owns command-level administrator authorization.
pub(crate) async fn handle_ws(state: &AppState, command: &Value) -> Option<Result<Value>> {
    let command_type = command.get("type")?.as_str()?;
    if !matches!(
        command_type,
        "config_entries/subentries/list"
            | "config_entries/subentries/update"
            | "config_entries/subentries/delete"
    ) {
        return None;
    }
    let Some(manager) = state.features.config_entries.clone() else {
        return Some(Err(Error::NotFound(
            "Config entry subsystem is not configured".into(),
        )));
    };
    let command = command.clone();
    Some(
        tokio::task::spawn_blocking(move || handle_ws_sync(manager, &command))
            .await
            .map_err(|_| Error::Service("Config subentry task failed".into()))
            .and_then(|result| result),
    )
}
