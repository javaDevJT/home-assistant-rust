//! Native password-provider login flows and one-time OAuth code exchange.
use crate::{AppState, http::ApiError};
use axum::{
    Extension, Form, Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use hass_auth::AuthManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const LOGIN_TTL: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 128;

struct Flow {
    client_id: String,
    redirect_uri: String,
    expires: Instant,
    stage: FlowStage,
}
enum FlowStage {
    Password,
    Mfa {
        user_id: String,
        username: String,
        challenge_id: String,
    },
}
struct AuthorizationCode {
    user_id: String,
    client_id: String,
    expires: Instant,
    mfa_grant: Option<hass_auth::MfaLoginGrant>,
}
#[derive(Clone)]
struct MfaSetup {
    user_id: String,
    secret: String,
    uri: String,
    qr_code: String,
    expires: Instant,
}
#[derive(Default)]
struct Pending {
    flows: HashMap<String, Flow>,
    codes: HashMap<String, AuthorizationCode>,
    failures: HashMap<String, (u8, Instant)>,
    mfa_setup: HashMap<String, MfaSetup>,
}

impl Pending {
    fn prune(&mut self) {
        let now = Instant::now();
        self.flows.retain(|_, flow| flow.expires > now);
        self.codes.retain(|_, code| code.expires > now);
        self.failures.retain(|_, (_, until)| *until > now);
        self.mfa_setup.retain(|_, flow| flow.expires > now);
    }
}

pub(crate) struct LoginFlows {
    pending: Mutex<Pending>,
    password_slots: Arc<Semaphore>,
}
impl Default for LoginFlows {
    fn default() -> Self {
        Self {
            pending: Mutex::new(Pending::default()),
            password_slots: Arc::new(Semaphore::new(4)),
        }
    }
}

fn auth(state: &AppState) -> Result<AuthManager, ApiError> {
    state
        .features
        .auth
        .clone()
        .ok_or_else(|| ApiError::not_found("Password authentication is not configured"))
}

/// MFA setup belongs to the current native user and never to the static token.
pub(crate) async fn setup_mfa(
    state: &AppState,
    identity: &crate::extensions::Identity,
    command: &Value,
) -> hass_core::Result<Value> {
    use hass_core::Error;
    let invalid = || Error::InvalidInput("Invalid MFA setup request".into());
    let user_id = identity.user_id.clone().ok_or_else(invalid)?;
    let session_id = identity.session_id.clone().ok_or_else(invalid)?;
    let fields = command.as_object().ok_or_else(invalid)?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "id" | "type" | "mfa_module_id" | "flow_id" | "user_input"
        )
    }) || fields.contains_key("mfa_module_id") == fields.contains_key("flow_id")
        || fields
            .get("user_input")
            .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(invalid());
    }
    let auth = state
        .features
        .auth
        .clone()
        .ok_or_else(|| Error::NotFound("Authentication is not configured".into()))?;
    // Recheck session ownership before revealing enrollment material, including
    // on subsequent requests for an existing flow.
    let current_auth = auth.clone();
    let current_user = user_id.clone();
    tokio::task::spawn_blocking(move || {
        current_auth.handle_ws_session(
            &json!({"type":"auth/current_user"}),
            &current_user,
            Some(&session_id),
        )
    })
    .await
    .map_err(|_| Error::Service("MFA task failed".into()))??;
    if let Some(module) = fields.get("mfa_module_id") {
        if module.as_str() != Some("totp") {
            return Err(Error::NotFound("MFA module is not found".into()));
        }
        let flow_id = Uuid::new_v4().simple().to_string();
        {
            let mut pending = state
                .login
                .pending
                .lock()
                .map_err(|_| Error::Storage("Login flow lock failed".into()))?;
            pending.prune();
            if let Some((id, flow)) = pending
                .mfa_setup
                .iter()
                .find(|(_, flow)| flow.user_id == user_id)
            {
                if flow.secret.is_empty() {
                    return Err(Error::InvalidInput("MFA setup is already starting".into()));
                }
                return Ok(mfa_setup_form(id, flow, false));
            }
            if pending.mfa_setup.len() >= MAX_PENDING {
                return Err(Error::InvalidInput("Too many MFA setup flows".into()));
            }
            // Reserve the user's slot across the asynchronous enrollment task.
            pending.mfa_setup.insert(
                flow_id.clone(),
                MfaSetup {
                    user_id: user_id.clone(),
                    secret: String::new(),
                    uri: String::new(),
                    qr_code: String::new(),
                    expires: Instant::now() + LOGIN_TTL,
                },
            );
        }
        let owner = user_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let enrollment = auth.begin_mfa_enrollment(&owner)?;
            let qr = qrcode::QrCode::new(enrollment.otpauth_uri.as_bytes())
                .map_err(|_| Error::Service("Cannot encode MFA QR code".into()))?;
            let svg = qr
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(240, 240)
                .build();
            Ok::<_, Error>(MfaSetup {
                user_id: owner,
                secret: enrollment.secret,
                uri: enrollment.otpauth_uri,
                qr_code: svg,
                expires: Instant::now() + LOGIN_TTL,
            })
        })
        .await
        .map_err(|_| Error::Service("MFA task failed".into()))
        .and_then(|result| result);
        let mut pending = state
            .login
            .pending
            .lock()
            .map_err(|_| Error::Storage("Login flow lock failed".into()))?;
        pending.mfa_setup.remove(&flow_id);
        let flow = result?;
        let response = mfa_setup_form(&flow_id, &flow, false);
        pending.mfa_setup.insert(flow_id, flow);
        return Ok(response);
    }
    let flow_id = fields
        .get("flow_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let flow = {
        let mut pending = state
            .login
            .pending
            .lock()
            .map_err(|_| Error::Storage("Login flow lock failed".into()))?;
        pending.prune();
        pending
            .mfa_setup
            .get(flow_id)
            .filter(|flow| flow.user_id == user_id && !flow.secret.is_empty())
            .cloned()
            .ok_or_else(|| Error::NotFound("MFA setup flow is not found".into()))?
    };
    let Some(input) = fields.get("user_input").filter(|value| !value.is_null()) else {
        return Ok(mfa_setup_form(flow_id, &flow, false));
    };
    let input = input.as_object().ok_or_else(invalid)?;
    if input.keys().any(|key| key != "code") {
        return Err(invalid());
    }
    let code = input
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let result = tokio::task::spawn_blocking(move || auth.confirm_mfa_enrollment(&user_id, &code))
        .await
        .map_err(|_| Error::Service("MFA task failed".into()))?;
    match result {
        Ok(_) => {
            state
                .login
                .pending
                .lock()
                .map_err(|_| Error::Storage("Login flow lock failed".into()))?
                .mfa_setup
                .remove(flow_id);
            Ok(
                json!({"type":"create_entry","flow_id":flow_id,"handler":"totp","title":"","data":{}}),
            )
        }
        Err(Error::InvalidInput(_)) => Ok(mfa_setup_form(flow_id, &flow, true)),
        Err(error) => Err(error),
    }
}

fn mfa_setup_form(flow_id: &str, flow: &MfaSetup, invalid: bool) -> Value {
    json!({"type":"form","flow_id":flow_id,"handler":"totp","step_id":"init",
        "data_schema":[{"name":"code","type":"string","required":true}],
        "description_placeholders":{"code":flow.secret,"url":flow.uri,"qr_code":flow.qr_code,
            "google_authenticator_url":"https://support.google.com/accounts/answer/1066447","authy_url":"https://authy.com/"},
        "errors":if invalid {json!({"base":"invalid_code"})} else {json!({})}})
}
fn busy() -> ApiError {
    ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        message: "Too many authentication attempts".into(),
    }
}
fn invalid_grant() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid_grant"})),
    )
        .into_response()
}

fn validate_client(client_id: &str, redirect_uri: &str) -> Result<(), ApiError> {
    let client = url::Url::parse(client_id).map_err(|_| ApiError::invalid("Invalid client_id"))?;
    let redirect =
        url::Url::parse(redirect_uri).map_err(|_| ApiError::invalid("Invalid redirect_uri"))?;
    if !matches!(client.scheme(), "http" | "https")
        || client.host_str().is_none()
        || !client.username().is_empty()
        || client.password().is_some()
        || client.origin() != redirect.origin()
        || redirect.fragment().is_some()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || client_id.len() > 2048
        || redirect_uri.len() > 2048
    {
        return Err(ApiError::invalid(
            "Client and redirect must use the same HTTP origin",
        ));
    }
    Ok(())
}

fn form(flow_id: &str, invalid: bool) -> Value {
    json!({"type":"form", "flow_id":flow_id, "handler":["homeassistant",null], "step_id":"init",
        "data_schema":[{"name":"username","required":true,"type":"string"},{"name":"password","required":true,"type":"string"}],
        "errors":if invalid {json!({"base":"invalid_auth"})} else {json!({})}, "description_placeholders":{}, "last_step":true})
}

fn issue_code(
    state: &AppState,
    user_id: String,
    client_id: String,
    mfa_grant: Option<hass_auth::MfaLoginGrant>,
) -> Result<String, ApiError> {
    let mut pending = state
        .login
        .pending
        .lock()
        .map_err(|_| ApiError::internal())?;
    pending.prune();
    if pending.codes.len() >= MAX_PENDING {
        return Err(busy());
    }
    let code = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    pending.codes.insert(
        code.clone(),
        AuthorizationCode {
            user_id,
            client_id,
            expires: Instant::now() + LOGIN_TTL,
            mfa_grant,
        },
    );
    Ok(code)
}

pub(crate) async fn providers(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    auth(&state)?;
    Ok(Json(
        json!([{"name":"Home Assistant", "id":null,"type":"homeassistant"}]),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlowStart {
    client_id: String,
    handler: Value,
    redirect_uri: Option<String>,
}

pub(crate) async fn create_flow(
    State(state): State<Arc<AppState>>,
    Json(request): Json<FlowStart>,
) -> Result<Json<Value>, ApiError> {
    auth(&state)?;
    if request.handler != json!(["homeassistant", null]) {
        return Err(ApiError::invalid("Unknown authentication provider"));
    }
    let redirect = request
        .redirect_uri
        .unwrap_or_else(|| request.client_id.clone());
    validate_client(&request.client_id, &redirect)?;
    let flow_id = Uuid::new_v4().simple().to_string();
    let mut pending = state
        .login
        .pending
        .lock()
        .map_err(|_| ApiError::internal())?;
    pending.prune();
    if pending.flows.len() >= MAX_PENDING {
        return Err(busy());
    }
    pending.flows.insert(
        flow_id.clone(),
        Flow {
            client_id: request.client_id,
            redirect_uri: redirect,
            expires: Instant::now() + LOGIN_TTL,
            stage: FlowStage::Password,
        },
    );
    Ok(Json(form(&flow_id, false)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    username: Option<String>,
    password: Option<String>,
    client_id: Option<String>,
    code: Option<String>,
    mfa_module_id: Option<String>,
}

fn mfa_form(flow_id: &str, invalid: bool) -> Value {
    json!({"type":"form","flow_id":flow_id,"handler":["homeassistant",null],"step_id":"mfa",
        "data_schema":[{"name":"code","required":true,"type":"string"}],
        "errors":if invalid {json!({"base":"invalid_code"})} else {json!({})},
        "description_placeholders":{"mfa_module_name":"Authenticator app"},"last_step":true})
}

pub(crate) async fn submit_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    Extension(origin): Extension<crate::network::ClientOrigin>,
    Json(request): Json<Credentials>,
) -> Result<Json<Value>, ApiError> {
    let auth = auth(&state)?;
    let permit = state
        .login
        .password_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let mut flow = {
        let mut pending = state
            .login
            .pending
            .lock()
            .map_err(|_| ApiError::internal())?;
        pending.prune();
        let flow = pending
            .flows
            .remove(&flow_id)
            .ok_or_else(|| ApiError::not_found("Unknown or expired login flow"))?;
        if request
            .client_id
            .as_ref()
            .is_some_and(|id| id != &flow.client_id)
        {
            return Err(ApiError::invalid("Client mismatch"));
        }
        flow
    };
    let rate_key = match &flow.stage {
        FlowStage::Password => request
            .username
            .clone()
            .ok_or_else(|| ApiError::invalid("Username required"))?,
        FlowStage::Mfa { username, .. } => username.clone(),
    };
    {
        let pending = state
            .login
            .pending
            .lock()
            .map_err(|_| ApiError::internal())?;
        if pending
            .failures
            .get(&rate_key)
            .is_some_and(|(count, _)| *count >= 5)
        {
            return Err(busy());
        }
    }
    enum Completed {
        Authenticated(String, Option<hass_auth::MfaLoginGrant>),
        Challenge(String, String),
    }
    let verified: hass_core::Result<Completed> = match &flow.stage {
        FlowStage::Password => {
            if request.code.is_some() || request.mfa_module_id.is_some() {
                return Err(ApiError::invalid(
                    "Password step requires username and password",
                ));
            }
            let username = request
                .username
                .ok_or_else(|| ApiError::invalid("Username required"))?;
            let password = request
                .password
                .ok_or_else(|| ApiError::invalid("Password required"))?;
            if username.len() > 256 || password.len() > 4096 {
                return Err(ApiError::invalid("Invalid credentials"));
            }
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                match auth.begin_mfa_login(&username, &password)? {
                    hass_auth::LoginAuthResult::Authenticated { user } => {
                        if user.local_only.unwrap_or(false) && !origin.is_local {
                            return Err(hass_core::Error::InvalidInput(
                                "Invalid credentials".into(),
                            ));
                        }
                        Ok(Completed::Authenticated(user.id, None))
                    }
                    hass_auth::LoginAuthResult::MfaRequired { user, challenge_id } => {
                        if user.local_only.unwrap_or(false) && !origin.is_local {
                            return Err(hass_core::Error::InvalidInput(
                                "Invalid credentials".into(),
                            ));
                        }
                        Ok(Completed::Challenge(user.id, challenge_id))
                    }
                }
            })
            .await
            .map_err(|_| ApiError::internal())?
        }
        FlowStage::Mfa {
            user_id,
            challenge_id,
            ..
        } => {
            if request.username.is_some()
                || request.password.is_some()
                || request
                    .mfa_module_id
                    .as_deref()
                    .is_some_and(|module| module != "totp")
            {
                return Err(ApiError::invalid("Invalid MFA step"));
            }
            let code = request
                .code
                .ok_or_else(|| ApiError::invalid("Code required"))?;
            let user_id = user_id.clone();
            let challenge_id = challenge_id.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let user = auth.get_user(&user_id)?;
                if user.local_only.unwrap_or(false) && !origin.is_local {
                    return Err(hass_core::Error::InvalidInput("Invalid credentials".into()));
                }
                auth.complete_mfa_login(&challenge_id, &code)
                    .map(|grant| Completed::Authenticated(user_id, Some(grant)))
            })
            .await
            .map_err(|_| ApiError::internal())?
        }
    };
    match verified {
        Ok(Completed::Authenticated(user_id, grant)) => {
            let code = issue_code(&state, user_id, flow.client_id, grant)?;
            state
                .login
                .pending
                .lock()
                .map_err(|_| ApiError::internal())?
                .failures
                .remove(&rate_key);
            Ok(Json(
                json!({"type":"create_entry","flow_id":flow_id,"handler":["homeassistant",null],"title":"Home Assistant","result":code,"redirect_uri":flow.redirect_uri}),
            ))
        }
        Ok(Completed::Challenge(user_id, challenge_id)) => {
            flow.stage = FlowStage::Mfa {
                user_id,
                username: rate_key.clone(),
                challenge_id,
            };
            let mut pending = state
                .login
                .pending
                .lock()
                .map_err(|_| ApiError::internal())?;
            pending.failures.remove(&rate_key);
            pending.flows.insert(flow_id.clone(), flow);
            Ok(Json(mfa_form(&flow_id, false)))
        }
        Err(hass_core::Error::Storage(_)) => Err(ApiError::internal()),
        Err(_) => {
            let mut pending = state
                .login
                .pending
                .lock()
                .map_err(|_| ApiError::internal())?;
            if pending.failures.len() >= 1024 && !pending.failures.contains_key(&rate_key) {
                return Err(busy());
            }
            let failure = pending
                .failures
                .entry(rate_key)
                .or_insert((0, Instant::now() + Duration::from_secs(60)));
            failure.0 = failure.0.saturating_add(1);
            let response = if matches!(flow.stage, FlowStage::Mfa { .. }) {
                mfa_form(&flow_id, true)
            } else {
                form(&flow_id, true)
            };
            pending.flows.insert(flow_id, flow);
            Ok(Json(response))
        }
    }
}

pub(crate) async fn delete_flow(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .login
        .pending
        .lock()
        .map_err(|_| ApiError::internal())?
        .flows
        .remove(&flow_id);
    Ok(Json(json!({"message":"Flow deleted"})))
}

#[derive(Deserialize)]
pub(crate) struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
    action: Option<String>,
}

pub(crate) async fn token(
    State(state): State<Arc<AppState>>,
    Form(request): Form<TokenRequest>,
) -> Response {
    let Ok(auth) = auth(&state) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unsupported_grant_type"})),
        )
            .into_response();
    };
    if request.action.as_deref() == Some("revoke") {
        let Some(refresh) = request.refresh_token else {
            return invalid_grant();
        };
        return match tokio::task::spawn_blocking(move || auth.revoke_refresh_token(&refresh)).await
        {
            Ok(Ok(_)) => StatusCode::OK.into_response(),
            _ => ApiError::internal().into_response(),
        };
    }
    let result = match request.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(client_id)) = (request.code, request.client_id) else {
                return invalid_grant();
            };
            let code = {
                let Ok(mut pending) = state.login.pending.lock() else {
                    return ApiError::internal().into_response();
                };
                pending.prune();
                pending.codes.remove(&code)
            };
            let Some(code) = code.filter(|code| code.client_id == client_id) else {
                return invalid_grant();
            };
            tokio::task::spawn_blocking(move || match code.mfa_grant {
                Some(grant) => auth.issue_tokens_after_mfa(grant, &client_id, None),
                None => auth.issue_tokens(&code.user_id, &client_id, None),
            })
            .await
        }
        Some("refresh_token") => {
            let (Some(refresh), Some(client_id)) = (request.refresh_token, request.client_id)
            else {
                return invalid_grant();
            };
            tokio::task::spawn_blocking(move || {
                auth.refresh_tokens_for_client(&refresh, &client_id)
            })
            .await
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"unsupported_grant_type"})),
            )
                .into_response();
        }
    };
    match result {
        Ok(Ok(tokens)) => (
            [("cache-control", "no-store"), ("pragma", "no-cache")],
            Json(tokens),
        )
            .into_response(),
        Ok(Err(hass_core::Error::Storage(_))) | Err(_) => ApiError::internal().into_response(),
        Ok(Err(_)) => invalid_grant(),
    }
}

pub(crate) async fn onboarding(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let auth = auth(&state)?;
    let done = tokio::task::spawn_blocking(move || auth.has_users())
        .await
        .map_err(|_| ApiError::internal())??;
    Ok(Json(
        json!([{"step":"user","done":done},{"step":"core_config","done":false},{"step":"integration","done":false},{"step":"analytics","done":false}]),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OnboardUser {
    name: String,
    username: String,
    password: String,
    client_id: String,
    language: Option<String>,
}

pub(crate) async fn onboard_user(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OnboardUser>,
) -> Result<Json<Value>, ApiError> {
    let auth = auth(&state)?;
    validate_client(&request.client_id, &request.client_id)?;
    let _language = request.language;
    let permit = state
        .login
        .password_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let user = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth.bootstrap_owner_with_username(&request.username, &request.name, &request.password)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let code = issue_code(&state, user.id, request.client_id, None)?;
    Ok(Json(json!({"auth_code":code})))
}
