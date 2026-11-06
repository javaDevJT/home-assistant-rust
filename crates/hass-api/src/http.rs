use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use hass_core::{Context, Error as CoreError, ServiceCall, State as HassState};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;

use crate::AppState;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Internal server error".to_owned(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::InvalidInput(message) => Self::invalid(message),
            CoreError::NotFound(message) => Self::not_found(message),
            CoreError::Storage(_) => Self::internal(),
            CoreError::Service(_) => Self::internal(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "message": self.message }))).into_response()
    }
}

pub(crate) async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if state.api.access_token.is_empty() {
        return ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "API access token is not configured".to_owned(),
        }
        .into_response();
    }

    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        });

    match token {
        Some(token) if token_matches(&state.api.access_token, token) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "message": "Unauthorized" })),
        )
            .into_response(),
    }
}

pub(crate) async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub(crate) async fn api_root() -> Json<Value> {
    Json(json!({ "message": "API running." }))
}

pub(crate) async fn get_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.api.config.clone())
}

pub(crate) async fn get_states(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<HassState>>, ApiError> {
    let runtime = state.api.runtime.clone();
    let states = run_blocking(move || Ok(runtime.states())).await?;
    Ok(Json(states))
}

pub(crate) async fn get_state(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
) -> Result<Json<HassState>, ApiError> {
    let runtime = state.api.runtime.clone();
    let entity_state = run_blocking(move || {
        runtime
            .get_state(&entity_id)
            .ok_or_else(|| CoreError::NotFound(format!("Entity {entity_id} not found")))
    })
    .await?;
    Ok(Json(entity_state))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateRequest {
    state: String,
    #[serde(default)]
    attributes: Map<String, Value>,
    #[serde(default)]
    force_update: bool,
}

pub(crate) async fn set_state(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: StateRequest = parse_json(&body)?;
    let location = format!("/api/states/{entity_id}");
    let runtime = state.api.runtime.clone();
    let result = run_blocking(move || {
        runtime.set_state(
            &entity_id,
            &request.state,
            request.attributes,
            Context::default(),
            request.force_update,
        )
    })
    .await?;

    let status = if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut response = (status, Json(result.state)).into_response();
    response.headers_mut().insert(
        header::LOCATION,
        location.parse().map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

pub(crate) async fn delete_state(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let runtime = state.api.runtime.clone();
    let removed =
        run_blocking(move || runtime.remove_state(&entity_id, Context::default())).await?;
    if !removed {
        return Err(ApiError::not_found("Entity not found."));
    }
    Ok(Json(json!({ "message": "Entity removed." })))
}

pub(crate) async fn get_services(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let runtime = state.api.runtime.clone();
    let services = run_blocking(move || Ok(runtime.services())).await?;
    let response = match services {
        Value::Object(domains) => Value::Array(
            domains
                .into_iter()
                .map(|(domain, services)| json!({ "domain": domain, "services": services }))
                .collect(),
        ),
        _ => return Err(ApiError::internal()),
    };
    Ok(Json(response))
}

pub(crate) async fn call_service(
    State(state): State<Arc<AppState>>,
    Path((domain, service)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    if query.keys().any(|key| key != "return_response") {
        return Err(ApiError::invalid("Unsupported query parameter"));
    }
    let return_response = query.contains_key("return_response");
    let body = parse_object(&body, true)?;
    let ServicePayload { data, target } = service_payload(body)?;
    let call = ServiceCall {
        domain,
        service,
        data,
        target,
        context: Context::default(),
    };
    let runtime = state.api.runtime.clone();
    let check_runtime = runtime.clone();
    let domain = call.domain.clone();
    let service = call.service.clone();
    let exists = run_blocking(move || {
        Ok(check_runtime
            .services()
            .get(&domain)
            .and_then(Value::as_object)
            .is_some_and(|services| services.contains_key(&service)))
    })
    .await?;
    if !exists {
        return Err(ApiError::not_found("Service not found."));
    }
    let result = run_blocking(move || runtime.call_service(call)).await?;

    if return_response {
        Ok(Json(json!({
            "changed_states": result.changed_states,
            "service_response": result.response,
        }))
        .into_response())
    } else {
        Ok(Json(result.changed_states).into_response())
    }
}

struct ServicePayload {
    data: Map<String, Value>,
    target: Map<String, Value>,
}

fn service_payload(mut body: Map<String, Value>) -> Result<ServicePayload, ApiError> {
    let target = body
        .remove("target")
        .map(value_object)
        .transpose()?
        .unwrap_or_default();
    let mut data = match body.remove("service_data") {
        Some(value) => value_object(value)?,
        None => Map::new(),
    };
    data.extend(body);
    Ok(ServicePayload { data, target })
}

fn value_object(value: Value) -> Result<Map<String, Value>, ApiError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::invalid("Expected a JSON object"))
}

pub(crate) async fn get_events() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "message": "Event listener inventory is not implemented by hass-core."
        })),
    )
        .into_response()
}

pub(crate) async fn fire_event(
    State(state): State<Arc<AppState>>,
    Path(event_type): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate_event_type(&event_type)?;
    let data = parse_object(&body, true)?;
    let response_event_type = event_type.clone();
    let runtime = state.api.runtime.clone();
    run_blocking(move || runtime.fire_event(&event_type, data, Context::default())).await?;
    Ok(Json(
        json!({ "message": format!("Event {response_event_type} fired.") }),
    ))
}

pub(crate) async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Vec<HassState>>>, ApiError> {
    history_response(state, None, query).await
}

pub(crate) async fn get_history_from(
    State(state): State<Arc<AppState>>,
    Path(timestamp): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Vec<HassState>>>, ApiError> {
    history_response(state, Some(timestamp), query).await
}

async fn history_response(
    state: Arc<AppState>,
    path_start: Option<String>,
    query: HashMap<String, String>,
) -> Result<Json<Vec<Vec<HassState>>>, ApiError> {
    if query
        .keys()
        .any(|key| key != "filter_entity_id" && key != "end_time")
    {
        return Err(ApiError::invalid("Unsupported history query parameter"));
    }

    let end = match query.get("end_time") {
        Some(value) => parse_timestamp(value)?,
        None => Utc::now(),
    };
    let start = match path_start {
        Some(value) => parse_timestamp(&value)?,
        None => end - Duration::days(1),
    };
    if start > end {
        return Err(ApiError::invalid(
            "History start time must not be after end_time",
        ));
    }

    let value = query
        .get("filter_entity_id")
        .ok_or_else(|| ApiError::invalid("filter_entity_id is required"))?;
    let entity_ids = value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if entity_ids.is_empty() {
        return Err(ApiError::invalid("filter_entity_id must not be empty"));
    }

    let runtime = state.api.runtime.clone();
    let history = run_blocking(move || {
        entity_ids
            .into_iter()
            .map(|entity_id| runtime.history(&entity_id, start, end))
            .collect::<hass_core::Result<Vec<_>>>()
    })
    .await?;
    Ok(Json(history))
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| ApiError::invalid("Timestamp must be RFC 3339"))
}

fn validate_event_type(event_type: &str) -> Result<(), ApiError> {
    if event_type.is_empty() || event_type.len() > 255 || event_type.chars().any(char::is_control) {
        return Err(ApiError::invalid("Invalid event type"));
    }
    Ok(())
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| ApiError::invalid("Invalid JSON body"))
}

fn parse_object(body: &Bytes, allow_empty: bool) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() && allow_empty {
        return Ok(Map::new());
    }
    let value: Value = parse_json(body)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::invalid("Request body must be a JSON object"))
}

async fn run_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> hass_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(ApiError::from)
}

fn token_matches(expected: &str, candidate: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    expected.as_bytes().ct_eq(candidate.as_bytes()).unwrap_u8() == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiState;
    use axum::{Router, body::Body, http::Request};
    use hass_core::{MemoryStore, Runtime};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_router() -> Router {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        crate::router(ApiState {
            runtime,
            access_token: "test-secret-token".to_owned(),
            config: json!({ "location_name": "test" }),
        })
    }

    fn request(method: &str, uri: &str, token: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    async fn response_json(response: axum::response::Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    #[tokio::test]
    async fn event_listener_inventory_is_explicitly_unimplemented() {
        let (status, body) = response_json(
            test_router()
                .oneshot(request("GET", "/api/events", Some("test-secret-token"), ""))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("listener inventory")
        );
    }

    #[tokio::test]
    async fn rest_auth_health_and_state_statuses() {
        let app = test_router();
        let (status, _) = response_json(
            app.clone()
                .oneshot(request("GET", "/api/config", None, ""))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, health) = response_json(
            app.clone()
                .oneshot(request("GET", "/health", None, ""))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(health, json!({ "status": "ok" }));

        let (status, _) = response_json(
            app.clone()
                .oneshot(request("GET", "/api/config", Some("wrong-token"), ""))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let create = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/states/light.test",
                Some("test-secret-token"),
                r#"{"state":"on","attributes":{"friendly_name":"Test"}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        assert_eq!(
            create.headers().get(header::LOCATION).unwrap(),
            "/api/states/light.test"
        );
        let (_, created_state) = response_json(create).await;
        assert_eq!(created_state["state"], "on");

        let (status, updated_state) = response_json(
            app.clone()
                .oneshot(request(
                    "POST",
                    "/api/states/light.test",
                    Some("test-secret-token"),
                    r#"{"state":"off"}"#,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated_state["state"], "off");

        let (status, deleted) = response_json(
            app.clone()
                .oneshot(request(
                    "DELETE",
                    "/api/states/light.test",
                    Some("test-secret-token"),
                    "",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deleted["message"], "Entity removed.");

        let (status, _) = response_json(
            app.clone()
                .oneshot(request(
                    "GET",
                    "/api/states/light.test",
                    Some("test-secret-token"),
                    "",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = response_json(
            app.oneshot(request(
                "DELETE",
                "/api/states/light.test",
                Some("test-secret-token"),
                "",
            ))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rest_invalid_state_body_and_history_filter() {
        let app = test_router();
        let (status, _) = response_json(
            app.clone()
                .oneshot(request(
                    "POST",
                    "/api/states/sensor.test",
                    Some("test-secret-token"),
                    "{",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, state) = response_json(
            app.clone()
                .oneshot(request(
                    "POST",
                    "/api/states/sensor.test",
                    Some("test-secret-token"),
                    r#"{"state":"22","attributes":{"unit_of_measurement":"°C"}}"#,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(state["state"], "22");

        let (status, history) = response_json(
            app.oneshot(request(
                "GET",
                "/api/history/period?filter_entity_id=sensor.test",
                Some("test-secret-token"),
                "",
            ))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(history.as_array().unwrap().len(), 1);
        assert_eq!(history[0][0]["entity_id"], "sensor.test");
    }

    #[tokio::test]
    async fn empty_configured_token_rejects_protected_requests() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let app = crate::router(ApiState {
            runtime,
            access_token: String::new(),
            config: Value::Null,
        });
        let (status, body) = response_json(
            app.oneshot(request("GET", "/api/config", Some(""), ""))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "API access token is not configured");
    }

    #[test]
    fn bearer_token_comparison_requires_exact_match() {
        assert!(token_matches("token", "token"));
        assert!(!token_matches("token", "token2"));
        assert!(!token_matches("token", "Token"));
        assert!(!token_matches("", ""));
    }

    #[test]
    fn rest_service_body_preserves_flat_data_and_extracts_target() {
        let mut body = Map::new();
        body.insert("entity_id".to_owned(), json!("light.kitchen"));
        body.insert("brightness".to_owned(), json!(120));
        body.insert("target".to_owned(), json!({ "area_id": "kitchen" }));
        let ServicePayload { data, target } = service_payload(body).unwrap();
        assert_eq!(data.get("brightness"), Some(&json!(120)));
        assert_eq!(data.get("entity_id"), Some(&json!("light.kitchen")));
        assert_eq!(target.get("area_id"), Some(&json!("kitchen")));
    }

    #[tokio::test]
    async fn runtime_test_fixture_constructs() {
        let _runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    }
}
