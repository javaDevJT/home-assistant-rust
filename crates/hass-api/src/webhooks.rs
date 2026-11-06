//! Public HTTP webhook dispatch and admin-only WebSocket inventory.

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    extract::{Extension, Path, Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
};
use hass_core::{Error as CoreError, Result as CoreResult};
use hass_webhook::{WebhookManager, WebhookOrigin, WebhookRequest, WebhookResponse};
use serde_json::{Map, Value, json};

use crate::{AppState, extensions::Identity, network::ClientOrigin};

const MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;

/// Receive an inbound webhook. The parent router must place this route outside
/// JWT authentication and after the global trusted-origin middleware.
pub(crate) async fn handle(
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<String>,
    Extension(origin): Extension<ClientOrigin>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_WEBHOOK_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return empty_response(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let normalized = match normalize_request(
        parts.method.as_str(),
        parts.uri.query().unwrap_or_default(),
        &parts.headers,
        &body,
        WebhookOrigin {
            client_ip: origin.client_ip,
            is_local: origin.is_local,
        },
    ) {
        Ok(request) => request,
        Err(()) => return into_http_response(WebhookResponse::ok(), &parts.method),
    };
    let response = match state.features.webhooks.clone() {
        Some(manager) => manager.dispatch(&webhook_id, normalized).await,
        None => WebhookResponse::ok(),
    };
    into_http_response(response, &parts.method)
}

/// Handle the native webhook WebSocket commands.
///
/// The inventory command checks admin authority here as well as at the outer
/// WebSocket command gate, so direct calls to this adapter cannot disclose IDs.
pub(crate) async fn handle_ws(
    state: &AppState,
    identity: &Identity,
    origin: ClientOrigin,
    command: &Value,
) -> Option<CoreResult<Value>> {
    let object = command.as_object()?;
    match object.get("type").and_then(Value::as_str)? {
        "webhook/list" => Some(webhook_list(state, identity, object)),
        "webhook/handle" => Some(webhook_handle(state, origin, object).await),
        _ => None,
    }
}

fn webhook_list(
    state: &AppState,
    identity: &Identity,
    command: &Map<String, Value>,
) -> CoreResult<Value> {
    if !identity.is_admin {
        return Err(CoreError::InvalidInput("Unauthorized".into()));
    }
    validate_ws_fields(command, &["id", "type"])?;
    let inventory = state
        .features
        .webhooks
        .as_ref()
        .map_or_else(Vec::new, WebhookManager::admin_inventory);
    serde_json::to_value(inventory)
        .map_err(|_| CoreError::Service("Cannot serialize webhook inventory".into()))
}

async fn webhook_handle(
    state: &AppState,
    origin: ClientOrigin,
    command: &Map<String, Value>,
) -> CoreResult<Value> {
    validate_ws_fields(
        command,
        &[
            "id",
            "type",
            "webhook_id",
            "method",
            "body",
            "headers",
            "query",
        ],
    )?;
    let webhook_id = required_string(command, "webhook_id")?;
    let method = required_string(command, "method")?;
    if !hass_webhook::SUPPORTED_METHODS.contains(&method.as_str()) {
        return Err(CoreError::InvalidInput("Unsupported webhook method".into()));
    }
    let body = optional_string(command, "body")?.unwrap_or_default();
    if body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(CoreError::InvalidInput(
            "Webhook body exceeds the size limit".into(),
        ));
    }
    let query = optional_string(command, "query")?.unwrap_or_default();
    let headers = command
        .get("headers")
        .map(string_map)
        .transpose()?
        .unwrap_or_default();
    let headers = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect::<std::collections::BTreeMap<_, _>>();
    let origin = WebhookOrigin {
        client_ip: origin.client_ip,
        is_local: origin.is_local,
    };
    let normalized = normalize_request_parts(method, &query, headers, body.into_bytes(), origin);
    let response = match normalized {
        Ok(request) => match state.features.webhooks.clone() {
            Some(manager) => manager.dispatch(&webhook_id, request).await,
            None => WebhookResponse::ok(),
        },
        Err(()) => WebhookResponse::ok(),
    };
    let status = StatusCode::from_u16(response.status)
        .map(|status| status.as_u16())
        .unwrap_or(StatusCode::OK.as_u16());
    let body = if StatusCode::from_u16(response.status).is_ok() {
        String::from_utf8_lossy(&response.body).into_owned()
    } else {
        String::new()
    };
    let content_type = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map_or_else(|| "text/plain".to_owned(), |(_, value)| value.clone());
    Ok(json!({
        "status": status,
        "body": body,
        "headers": { "Content-Type": content_type },
    }))
}

fn normalize_request(
    method: &str,
    query: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    origin: WebhookOrigin,
) -> Result<WebhookRequest, ()> {
    let mut normalized_headers = std::collections::BTreeMap::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            normalized_headers
                .entry(name.as_str().to_ascii_lowercase())
                .or_insert_with(|| value.to_owned());
        }
    }
    normalize_request_parts(
        method.to_owned(),
        query,
        normalized_headers,
        body.to_vec(),
        origin,
    )
}

fn normalize_request_parts(
    method: String,
    query: &str,
    headers: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
    origin: WebhookOrigin,
) -> Result<WebhookRequest, ()> {
    let content_type = headers.get("content-type").cloned().unwrap_or_default();
    let mut request = WebhookRequest::new(method, origin);
    request.query = decode_form_pairs(query.as_bytes());
    request.headers = headers;
    request.body = body;
    if content_type.contains("json") {
        request.json = Some(if request.body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&request.body).map_err(|_| ())?
        });
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        request.form = decode_form_pairs(&request.body);
    }
    Ok(request)
}

fn decode_form_pairs(encoded: &[u8]) -> std::collections::BTreeMap<String, String> {
    let mut pairs = std::collections::BTreeMap::new();
    for (name, value) in url::form_urlencoded::parse(encoded) {
        pairs
            .entry(name.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    pairs
}

fn into_http_response(mut response: WebhookResponse, method: &Method) -> Response {
    let Ok(status) = StatusCode::from_u16(response.status) else {
        return empty_response(StatusCode::OK);
    };
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(std::mem::take(&mut response.body))
    };
    let mut result = Response::new(body);
    *result.status_mut() = status;
    for (name, value) in response.headers {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            continue;
        };
        result.headers_mut().insert(name, value);
    }
    result
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn required_string(command: &Map<String, Value>, field: &str) -> CoreResult<String> {
    command
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CoreError::InvalidInput(format!("{field} must be a string")))
}

fn optional_string(command: &Map<String, Value>, field: &str) -> CoreResult<Option<String>> {
    command
        .get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| CoreError::InvalidInput(format!("{field} must be a string")))
        })
        .transpose()
}

fn string_map(value: &Value) -> CoreResult<std::collections::BTreeMap<String, String>> {
    let object = value
        .as_object()
        .ok_or_else(|| CoreError::InvalidInput("headers must be an object".into()))?;
    object
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_owned()))
                .ok_or_else(|| CoreError::InvalidInput("header values must be strings".into()))
        })
        .collect()
}

fn validate_ws_fields(command: &Map<String, Value>, allowed: &[&str]) -> CoreResult<()> {
    if command.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(CoreError::InvalidInput(
            "Unexpected webhook command field".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiFeatures, ApiState};
    use hass_core::{MemoryStore, Runtime};
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[test]
    fn webhook_inventory_is_admin_only_even_when_called_directly() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let manager = WebhookManager::new();
        let _registration = manager
            .register(
                "admin-only-inventory-secret",
                hass_webhook::WebhookConfig::new("test", "private callback"),
                |_| async { Ok(None) },
            )
            .unwrap();
        let state = AppState {
            api: ApiState {
                runtime,
                access_token: String::new(),
                config: json!({}),
            },
            websocket_slots: Arc::new(Semaphore::new(1)),
            shutdown: None,
            features: ApiFeatures {
                webhooks: Some(manager),
                ..ApiFeatures::default()
            },
            login: Arc::new(crate::login::LoginFlows::default()),
            _service_target_guard: None,
        };
        let identity = Identity {
            user_id: Some("read-only-user".into()),
            session_id: None,
            is_admin: false,
            read_only: true,
            can_read_states: true,
            can_control: false,
            local_only: false,
        };

        let error = webhook_list(&state, &identity, &Map::new()).unwrap_err();
        assert!(error.to_string().contains("Unauthorized"));
        assert!(!error.to_string().contains("admin-only-inventory-secret"));

        let identity = Identity {
            is_admin: true,
            read_only: false,
            can_control: true,
            ..identity
        };
        assert_eq!(
            webhook_list(&state, &identity, &Map::new()).unwrap()[0]["webhook_id"],
            "admin-only-inventory-secret"
        );
    }
}
