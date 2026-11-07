//! Authenticated HTTP adapters for native Repairs issue-fix flows.

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::{AppState, extensions::Identity, http::ApiError};

fn require_admin_edit(identity: &Identity) -> Result<(), ApiError> {
    // Native authentication has no standalone POLICY_EDIT permission. The
    // nearest available policy is a writable administrator identity.
    if identity.is_admin && !identity.read_only {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "Unauthorized".into(),
        })
    }
}

fn manager(state: &AppState) -> Result<hass_repairs::RepairsManager, ApiError> {
    state.features.repairs.clone().ok_or_else(|| ApiError {
        status: StatusCode::NOT_IMPLEMENTED,
        message: "Repairs support is not enabled.".into(),
    })
}

async fn run_manager<T, F>(
    manager: hass_repairs::RepairsManager,
    operation: F,
) -> Result<T, hass_repairs::RepairsError>
where
    T: Send + 'static,
    F: FnOnce(hass_repairs::RepairsManager) -> hass_repairs::RepairsResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || operation(manager))
        .await
        .map_err(|_| {
            hass_repairs::RepairsError::Core(hass_core::Error::Service(
                "Repairs operation failed".into(),
            ))
        })?
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

fn map_create_error(error: hass_repairs::RepairsError) -> ApiError {
    use hass_repairs::RepairsError;

    match error {
        RepairsError::Core(error) => ApiError::from(error),
        RepairsError::UnknownIssue { issue_id, .. } => {
            ApiError::invalid(format!("issue id {issue_id} is not found"))
        }
        RepairsError::NotFixable { issue_id, .. } => {
            ApiError::invalid(format!("issue id {issue_id} is not fixable"))
        }
        RepairsError::UnknownFlow(flow_id) => ApiError::not_found(if flow_id.is_empty() {
            "Unknown flow".into()
        } else {
            format!("Unknown flow: {flow_id}")
        }),
        RepairsError::UnknownStep(message) => ApiError::invalid(message),
        RepairsError::UnknownEntry(entry_id) => {
            ApiError::invalid(format!("Config entry {entry_id} not found in next_flow"))
        }
        RepairsError::Unsupported(message) => ApiError::invalid(message),
    }
}

fn map_resource_error(error: hass_repairs::RepairsError) -> ApiError {
    use hass_repairs::RepairsError;

    match error {
        RepairsError::Core(error) => ApiError::from(error),
        RepairsError::UnknownIssue { issue_id, .. } => {
            ApiError::invalid(format!("issue id {issue_id} is not found"))
        }
        RepairsError::NotFixable { issue_id, .. } => {
            ApiError::invalid(format!("issue id {issue_id} is not fixable"))
        }
        RepairsError::UnknownFlow(_) => ApiError::not_found("Invalid flow specified"),
        RepairsError::UnknownStep(message) => ApiError::invalid(message),
        RepairsError::UnknownEntry(entry_id) => {
            ApiError::invalid(format!("Config entry {entry_id} not found in next_flow"))
        }
        RepairsError::Unsupported(message) => ApiError::invalid(message),
    }
}

/// POST `/api/repairs/issues/fix`.
pub(crate) async fn create_fix_flow(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_admin_edit(&identity)?;
    let data = parse_object(&body, false)?;
    let handler = required_string(&data, "handler")?;
    let issue_id = required_string(&data, "issue_id")?;
    let manager = manager(&state)?;
    let flow = run_manager(manager, move |manager| {
        manager.start_fix_flow(&handler, &issue_id)
    })
    .await
    .map_err(map_create_error)?;
    Ok(Json(flow.to_value().map_err(map_create_error)?))
}

/// GET `/api/repairs/issues/fix/{flow_id}`.
pub(crate) async fn get_fix_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Value>, ApiError> {
    require_admin_edit(&identity)?;
    let manager = manager(&state)?;
    let flow = run_manager(manager, move |manager| manager.flow_result(&flow_id))
        .await
        .map_err(map_resource_error)?;
    Ok(Json(flow.to_value().map_err(map_resource_error)?))
}

/// POST `/api/repairs/issues/fix/{flow_id}`.
pub(crate) async fn submit_fix_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_admin_edit(&identity)?;
    let user_input = parse_object(&body, true)?;
    let manager = manager(&state)?;
    let flow = run_manager(manager, move |manager| {
        manager.submit_step(&flow_id, Value::Object(user_input))
    })
    .await
    .map_err(map_resource_error)?;
    Ok(Json(flow.to_value().map_err(map_resource_error)?))
}

/// DELETE `/api/repairs/issues/fix/{flow_id}`.
///
/// The pinned parent resource handler adds no Repairs-specific admin policy to
/// DELETE, so the router's ordinary authenticated-write policy applies.
pub(crate) async fn abort_fix_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let manager = manager(&state)?;
    run_manager(manager, move |manager| manager.abort_flow(&flow_id))
        .await
        .map_err(map_resource_error)?;
    Ok(Json(json!({"message":"Flow aborted"})))
}
