//! Adapters for the native authentication, registry and template subsystems.
use crate::{AppState, http::ApiError};
use axum::{Json, extract::State, response::IntoResponse};
use hass_core::{Context, Error, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use subtle::ConstantTimeEq;

#[derive(Clone, Debug)]
pub(crate) struct Identity {
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub is_admin: bool,
    pub read_only: bool,
    pub can_read_states: bool,
    pub can_control: bool,
    pub local_only: bool,
}

impl Identity {
    pub fn context(&self) -> Context {
        Context {
            user_id: self.user_id.clone(),
            ..Context::default()
        }
    }
}

pub(crate) async fn authenticate_token(state: &AppState, token: &str) -> Result<Identity> {
    if !state.api.access_token.is_empty()
        && state
            .api
            .access_token
            .as_bytes()
            .ct_eq(token.as_bytes())
            .unwrap_u8()
            == 1
    {
        return Ok(Identity {
            user_id: None,
            session_id: None,
            is_admin: true,
            read_only: false,
            can_read_states: true,
            can_control: true,
            local_only: false,
        });
    }
    let auth = state
        .features
        .auth
        .clone()
        .ok_or_else(|| Error::InvalidInput("Invalid access token".into()))?;
    let token = token.to_owned();
    let access = tokio::task::spawn_blocking(move || auth.validate_access_token_identity(&token))
        .await
        .map_err(|_| Error::Service("Authentication task failed".into()))??;
    Ok(identity_from_access(access))
}

fn identity_from_access(access: hass_auth::AccessIdentity) -> Identity {
    let user = access.user;
    let is_admin = user.is_admin || user.is_owner;
    let can_control = is_admin || user.group_ids.iter().any(|group| group == "system-users");
    let can_read_states = can_control
        || user
            .group_ids
            .iter()
            .any(|group| group == "system-read-only");
    Identity {
        user_id: Some(user.id),
        session_id: Some(access.session_id),
        is_admin,
        read_only: !can_control,
        can_control,
        can_read_states,
        local_only: user.local_only.unwrap_or(false),
    }
}

pub(crate) async fn authenticate_signed_request(
    state: &AppState,
    request: &mut axum::http::Request<axum::body::Body>,
) -> Result<Identity> {
    if !matches!(request.method().as_str(), "GET" | "HEAD") {
        return Err(Error::InvalidInput("Invalid access token".into()));
    }
    let (path, signature) = signed_uri(request.uri())?
        .ok_or_else(|| Error::InvalidInput("Invalid access token".into()))?;
    let auth = state
        .features
        .auth
        .clone()
        .ok_or_else(|| Error::InvalidInput("Invalid access token".into()))?;
    let validated_path = path.clone();
    let access = tokio::task::spawn_blocking(move || {
        auth.validate_signed_request(&validated_path, &signature)
    })
    .await
    .map_err(|_| Error::Service("Authentication task failed".into()))??;
    *request.uri_mut() = path
        .parse()
        .map_err(|_| Error::InvalidInput("Invalid signed path".into()))?;
    Ok(identity_from_access(access))
}

/// Preserve raw signed query ordering and remove only the capability parameter.
fn signed_uri(uri: &axum::http::Uri) -> Result<Option<(String, String)>> {
    let Some(query) = uri.query() else {
        return Ok(None);
    };
    let mut signature = None;
    let mut rest = Vec::new();
    for part in query.split('&') {
        let decoded = url::form_urlencoded::parse(part.as_bytes()).next();
        if let Some((key, value)) = decoded
            && key == "authSig"
        {
            if signature.replace(value.into_owned()).is_some() {
                return Err(Error::InvalidInput("Duplicate signed capability".into()));
            }
        } else {
            rest.push(part);
        }
    }
    Ok(signature.map(|signature| {
        let mut path = uri.path().to_owned();
        if !rest.is_empty() {
            path.push('?');
            path.push_str(&rest.join("&"));
        }
        (path, signature)
    }))
}

pub(crate) async fn resolve_origin(
    State(state): State<Arc<AppState>>,
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0);
    match state.features.network.resolve(peer, request.headers()) {
        Ok(origin) => {
            request.extensions_mut().insert(origin);
            next.run(request).await
        }
        Err(_) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"message":"Invalid proxy forwarding configuration or headers"}),
            ),
        )
            .into_response(),
    }
}

pub(crate) fn registry_command(kind: &str) -> bool {
    [
        "config/entity_registry/",
        "config/device_registry/",
        "config/area_registry/",
        "config/floor_registry/",
        "config/label_registry/",
    ]
    .iter()
    .any(|prefix| kind.starts_with(prefix))
}

pub(crate) fn command_requires_admin(kind: &str) -> bool {
    if registry_command(kind) {
        return !matches!(
            kind.rsplit('/').next(),
            Some(
                "list"
                    | "get"
                    | "get_entries"
                    | "list_for_display"
                    | "list_composite_splits"
                    | "list_linked_devices"
            )
        );
    }
    kind.starts_with("config/auth/")
        || kind.starts_with("config_entries/")
        || kind == "config/core/update"
        || matches!(kind, "zone/create" | "zone/update" | "zone/delete")
        || matches!(kind, "person/create" | "person/update" | "person/delete")
        || matches!(kind, "image/create" | "image/update" | "image/delete")
        || kind == "webhook/list"
        || matches!(kind, "tag/create" | "tag/update" | "tag/delete")
        || matches!(kind, "logger/log_level" | "logger/integration_log_level")
        || matches!(kind, "system_log/list" | "system_log/subscribe_raw")
        || kind.starts_with("config/auth_provider/")
        || matches!(
            kind,
            "fire_event"
                | "render_template"
                | "subscribe_trigger"
                | "test_condition"
                | "execute_script"
        )
}

pub(crate) fn websocket_allowed(
    identity: &Identity,
    kind: &str,
    message: &Map<String, Value>,
) -> bool {
    if command_requires_admin(kind) && !identity.is_admin {
        return false;
    }
    if kind == "call_service" && !identity.can_control {
        return false;
    }
    if kind == "call_service"
        && (matches!(
            message.get("domain").and_then(Value::as_str),
            Some("recorder" | "logger")
        ) || (matches!(
            message.get("domain").and_then(Value::as_str),
            Some("zone" | "person")
        ) && message.get("service").and_then(Value::as_str) == Some("reload")))
        && !identity.is_admin
    {
        return false;
    }
    if kind == "subscribe_events" && !identity.is_admin {
        let Some(event_type) = message.get("event_type").and_then(Value::as_str) else {
            return false;
        };
        return matches!(
            event_type,
            "area_registry_updated"
                | "component_loaded"
                | "core_config_updated"
                | "device_registry_updated"
                | "entity_registry_updated"
                | "repairs_issue_registry_updated"
                | "lovelace_updated"
                | "panels_updated"
                | "recorder_5min_statistics_generated"
                | "recorder_hourly_statistics_generated"
                | "service_registered"
                | "service_removed"
                | "shopping_list_updated"
                | "state_changed"
                | "themes_updated"
                | "label_registry_updated"
                | "labs_updated"
                | "category_registry_updated"
                | "floor_registry_updated"
        );
    }
    true
}

pub(crate) fn same_authority(left: &Identity, right: &Identity) -> bool {
    left.user_id == right.user_id
        && left.session_id == right.session_id
        && left.is_admin == right.is_admin
        && left.can_read_states == right.can_read_states
        && left.can_control == right.can_control
        && left.local_only == right.local_only
}

pub(crate) async fn handle_ws(
    state: &AppState,
    identity: &Identity,
    origin: crate::network::ClientOrigin,
    command: &Value,
) -> Option<Result<Value>> {
    let kind = command.get("type")?.as_str()?;
    if matches!(
        kind,
        "logger/log_info" | "logger/log_level" | "logger/integration_log_level"
    ) {
        let (Some(manager), Some(catalog)) = (
            state.features.logger.clone(),
            state.features.logger_catalog.clone(),
        ) else {
            return Some(Err(Error::NotFound("Logger is not configured".into())));
        };
        return Some(crate::logger::handle(manager, catalog, command).await);
    }
    if matches!(
        kind,
        "tag/list" | "tag/create" | "tag/update" | "tag/delete"
    ) {
        let Some(manager) = state.features.tags.clone() else {
            return Some(Err(Error::NotFound(
                "Tag collection is not configured".into(),
            )));
        };
        return Some(crate::tags::handle(manager, command).await);
    }
    if kind.starts_with("webhook/") {
        return crate::webhooks::handle_ws(state, identity, origin, command).await;
    }
    if kind.starts_with("image/") {
        let Some(manager) = state.features.images.clone() else {
            return Some(Err(Error::NotFound(
                "Image collection is not configured".into(),
            )));
        };
        return Some(crate::images::handle_ws(manager, command).await);
    }
    if kind.starts_with("person/") {
        return Some(crate::persons::handle(state, identity, command).await);
    }
    if kind.starts_with("zone/") {
        let Some(manager) = state.features.zones.clone() else {
            return Some(Err(Error::NotFound(
                "Zone integration is not configured".into(),
            )));
        };
        return Some(crate::zones::handle(manager, command).await);
    }
    if kind == "persistent_notification/get" {
        if command.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !matches!(key.as_str(), "id" | "type"))
        }) {
            return Some(Err(Error::InvalidInput(
                "Invalid notification get command".into(),
            )));
        }
        let Some(manager) = state.features.notifications.clone() else {
            return Some(Err(Error::NotFound(
                "Notification integration is not configured".into(),
            )));
        };
        return Some(
            tokio::task::spawn_blocking(move || {
                serde_json::to_value(manager.list()?)
                    .map_err(|_| Error::Service("Cannot serialize notifications".into()))
            })
            .await
            .map_err(|_| Error::Service("Notification task failed".into()))
            .and_then(|value| value),
        );
    }
    if kind == "auth/setup_mfa" {
        return Some(crate::login::setup_mfa(state, identity, command).await);
    }
    if kind == "history/history_during_period" {
        return Some(crate::history::history_during_period(state, identity, command).await);
    }
    if kind == "get_config" && state.features.config_entries.is_some() {
        return Some(configuration(state).await);
    }
    if let Some(result) = crate::config_subentries::handle_ws(state, command).await {
        return Some(result);
    }
    if kind.starts_with("config_entries/") || kind.starts_with("config/core/") {
        let Some(entries) = state.features.config_entries.clone() else {
            return Some(Err(Error::NotFound(
                "Config entry subsystem is not configured".into(),
            )));
        };
        let command = command.clone();
        return Some(
            tokio::task::spawn_blocking(move || entries.handle_ws(&command))
                .await
                .map_err(|_| Error::Service("Config entry task failed".into()))
                .and_then(|result| result),
        );
    }
    if registry_command(kind) {
        let Some(registries) = state.features.registries.clone() else {
            return Some(Err(Error::NotFound(
                "Registry subsystem is not configured".into(),
            )));
        };
        let command = command.clone();
        return Some(
            tokio::task::spawn_blocking(move || registries.handle_ws(&command))
                .await
                .map_err(|_| Error::Service("Registry task failed".into()))
                .and_then(|result| result),
        );
    }
    if kind.starts_with("auth/")
        || kind.starts_with("config/auth/")
        || kind.starts_with("config/auth_provider/")
    {
        let auth = state.features.auth.clone()?;
        let Some(user_id) = identity.user_id.clone() else {
            return Some(Err(Error::InvalidInput(
                "A user access token is required for account operations".into(),
            )));
        };
        let command = command.clone();
        let session_id = identity.session_id.clone();
        return Some(
            tokio::task::spawn_blocking(move || {
                auth.handle_ws_session(&command, &user_id, session_id.as_deref())
            })
            .await
            .map_err(|_| Error::Service("Authentication task failed".into()))
            .and_then(|result| result),
        );
    }
    None
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateRequest {
    template: String,
    #[serde(default)]
    variables: Map<String, Value>,
}

pub(crate) async fn configuration(state: &AppState) -> Result<Value> {
    let mut config = state.api.config.clone();
    if let Some(entries) = state.features.config_entries.clone() {
        let core = tokio::task::spawn_blocking(move || {
            entries.handle_ws(&serde_json::json!({"type":"config/core/get"}))
        })
        .await
        .map_err(|_| Error::Service("Config task failed".into()))??;
        if let (Some(config), Some(core)) = (config.as_object_mut(), core.as_object()) {
            config.extend(core.clone());
        }
    }
    Ok(config)
}

pub(crate) async fn render_template(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<Identity>,
    Json(request): Json<TemplateRequest>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    if !identity.is_admin {
        return Err(ApiError::forbidden());
    }
    let engine = state
        .features
        .templates
        .clone()
        .ok_or_else(|| ApiError::not_found("Template engine is not configured"))?;
    let rendered =
        tokio::task::spawn_blocking(move || engine.render(&request.template, &request.variables))
            .await
            .map_err(|_| ApiError::internal())??;
    Ok(rendered)
}
