//! Public OAuth callback adapter and shared async flow error handling.

use std::future::Future;
use url::form_urlencoded;

use axum::{
    extract::{RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use hass_application_credentials::ApplicationCredentialsError;
use hass_oauth2_client::OAuthError;
use hass_oauth2_flow::{OAuthCallbackParams, OAuthFlowError};
use std::sync::Arc;

use crate::{AppState, http::ApiError};

const MAX_CALLBACK_QUERY_BYTES: usize = 16 * 1024;
const MAX_CALLBACK_QUERY_PAIRS: usize = 64;
const MAX_CALLBACK_NAME_BYTES: usize = 128;
const MAX_CALLBACK_VALUE_BYTES: usize = 8 * 1024;
const MAX_CALLBACK_EXTRA_VALUE_BYTES: usize = 2 * 1024;

const CALLBACK_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorization complete</title></head><body><p>You can close this window.</p><script>window.close();</script></body></html>";

/// Handle the public external OAuth callback.
///
/// The callback only admits signed callback data into the host's private flow
/// state. Token exchange is deferred until the authenticated flow GET refreshes
/// the flow. The response is constant and contains no callback values.
pub(crate) async fn external_callback(
    State(state): State<Arc<AppState>>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let params = match parse_callback(raw_query.as_deref()) {
        Ok(params) => params,
        Err(error) => return error.into_response(),
    };
    let Some(manager) = state.features.oauth2_flows.clone() else {
        return ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            message: "OAuth flows are not enabled.".to_owned(),
        }
        .into_response();
    };

    match run_operation(&state, async {
        manager.callback(params).await.map_err(|error| match error {
            OAuthFlowError::UnknownFlow
            | OAuthFlowError::ExpiredFlow
            | OAuthFlowError::ReplayedCallback
            | OAuthFlowError::FlowAborted => OAuthFlowError::InvalidCallback,
            OAuthFlowError::OAuth(
                OAuthError::InvalidState
                | OAuthError::ExpiredFlow
                | OAuthError::UnknownFlow
                | OAuthError::ReplayedCallback,
            ) => OAuthFlowError::InvalidCallback,
            OAuthFlowError::Core(hass_core::Error::NotFound(_)) => OAuthFlowError::InvalidCallback,
            other => other,
        })
    })
    .await
    {
        Ok(_) => Html(CALLBACK_HTML).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Await an OAuth flow operation until it finishes or application shutdown
/// begins. This owns no task: canceling the HTTP waiter only drops its manager
/// future, while the OAuth host retains ownership of admitted resolver work.
pub(crate) async fn run_operation<T, F>(state: &AppState, operation: F) -> Result<T, ApiError>
where
    F: Future<Output = Result<T, OAuthFlowError>>,
{
    let Some(mut shutdown) = state.shutdown.clone() else {
        return operation.await.map_err(map_flow_error);
    };

    tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => Err(map_flow_error(OAuthFlowError::ShuttingDown)),
        result = operation => result.map_err(map_flow_error),
    }
}

pub(crate) async fn wait_for_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Convert OAuth host failures into status codes and fixed safe messages.
/// Wrapped errors are never formatted into an HTTP response.
pub(crate) fn map_flow_error(error: OAuthFlowError) -> ApiError {
    let (status, message) = match error {
        OAuthFlowError::InvalidConfiguration => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth flow configuration is invalid.",
        ),
        OAuthFlowError::InvalidCallback => (StatusCode::BAD_REQUEST, "Invalid OAuth callback."),
        OAuthFlowError::ExpiredFlow => (StatusCode::GONE, "OAuth flow has expired."),
        OAuthFlowError::UnknownFlow | OAuthFlowError::EntryUnavailable => {
            (StatusCode::NOT_FOUND, "OAuth flow was not found.")
        }
        OAuthFlowError::ReplayedCallback => (
            StatusCode::CONFLICT,
            "OAuth callback has already been handled.",
        ),
        OAuthFlowError::FlowAborted => (StatusCode::GONE, "OAuth flow has been aborted."),
        OAuthFlowError::MissingCredentials => {
            (StatusCode::CONFLICT, "Application credentials are missing.")
        }
        OAuthFlowError::OAuthImplementationUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth implementation is unavailable.",
        ),
        OAuthFlowError::UnsupportedCustomImplementation => (
            StatusCode::NOT_IMPLEMENTED,
            "This OAuth implementation is not supported.",
        ),
        OAuthFlowError::TooManyFlows => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many OAuth flows are active.",
        ),
        OAuthFlowError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth flow service is shutting down.",
        ),
        OAuthFlowError::ProviderAbort(_) => (
            StatusCode::BAD_REQUEST,
            "OAuth provider declined authorization.",
        ),
        OAuthFlowError::ProviderFailure => {
            (StatusCode::BAD_GATEWAY, "OAuth provider operation failed.")
        }
        OAuthFlowError::Core(error) => map_core_error(error),
        OAuthFlowError::ApplicationCredentials(error) => map_credentials_error(error),
        OAuthFlowError::OAuth(error) => map_oauth_error(error),
    };
    ApiError {
        status,
        message: message.to_owned(),
    }
}

fn map_core_error(error: hass_core::Error) -> (StatusCode, &'static str) {
    match error {
        hass_core::Error::InvalidInput(_)
        | hass_core::Error::ServiceValidation(_)
        | hass_core::Error::InvalidData { .. } => {
            (StatusCode::BAD_REQUEST, "Invalid OAuth flow request.")
        }
        hass_core::Error::NotFound(_) => (StatusCode::NOT_FOUND, "OAuth flow was not found."),
        hass_core::Error::NotAllowed(_) => (
            StatusCode::FORBIDDEN,
            "OAuth flow operation is not allowed.",
        ),
        hass_core::Error::Storage(_)
        | hass_core::Error::Service(_)
        | hass_core::Error::HomeAssistant(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth flow operation failed.",
        ),
    }
}

fn map_credentials_error(error: ApplicationCredentialsError) -> (StatusCode, &'static str) {
    match error {
        ApplicationCredentialsError::ItemNotFound(_)
        | ApplicationCredentialsError::ConfigEntryNotFound(_) => (
            StatusCode::NOT_FOUND,
            "Application credentials were not found.",
        ),
        ApplicationCredentialsError::CredentialInUse { .. }
        | ApplicationCredentialsError::DuplicatePlatform(_) => (
            StatusCode::CONFLICT,
            "Application credentials are in use or already registered.",
        ),
        ApplicationCredentialsError::UpdatesNotSupported => (
            StatusCode::NOT_IMPLEMENTED,
            "Application credential updates are not supported.",
        ),
        ApplicationCredentialsError::IntegrationStopped
        | ApplicationCredentialsError::StorageUnavailable
        | ApplicationCredentialsError::NoPlatform(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Application credentials are unavailable.",
        ),
        ApplicationCredentialsError::InvalidPlatform(_)
        | ApplicationCredentialsError::MalformedStore
        | ApplicationCredentialsError::UnsupportedStoreVersion(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Application credential configuration is invalid.",
        ),
        ApplicationCredentialsError::PlatformCallback { .. } => (
            StatusCode::BAD_GATEWAY,
            "Application credential provider operation failed.",
        ),
    }
}

fn map_oauth_error(error: OAuthError) -> (StatusCode, &'static str) {
    match error {
        OAuthError::InvalidConfiguration
        | OAuthError::InvalidEndpoint
        | OAuthError::InsecureEndpoint
        | OAuthError::ReservedAuthorizationParameter => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth client configuration is invalid.",
        ),
        OAuthError::InvalidCallback | OAuthError::InvalidState => {
            (StatusCode::BAD_REQUEST, "Invalid OAuth callback.")
        }
        OAuthError::ExpiredFlow => (StatusCode::GONE, "OAuth flow has expired."),
        OAuthError::UnknownFlow => (StatusCode::NOT_FOUND, "OAuth flow was not found."),
        OAuthError::ReplayedCallback => (
            StatusCode::CONFLICT,
            "OAuth callback has already been handled.",
        ),
        OAuthError::AccessDenied => (StatusCode::FORBIDDEN, "OAuth authorization was denied."),
        OAuthError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "OAuth provider request timed out.",
        ),
        OAuthError::TooManyFlows => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many OAuth flows are active.",
        ),
        OAuthError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth flow service is shutting down.",
        ),
        OAuthError::CredentialConflict => (
            StatusCode::CONFLICT,
            "OAuth credentials changed during authorization.",
        ),
        OAuthError::MissingRefreshToken => (
            StatusCode::CONFLICT,
            "OAuth authorization must be repeated.",
        ),
        OAuthError::InvalidTokenResponse
        | OAuthError::ResponseTooLarge
        | OAuthError::Network
        | OAuthError::ReauthenticationRequired(_)
        | OAuthError::Transient(_)
        | OAuthError::ProviderError => (StatusCode::BAD_GATEWAY, "OAuth provider request failed."),
        OAuthError::Storage | OAuthError::BackgroundTask => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth flow operation failed.",
        ),
    }
}

fn parse_callback(raw_query: Option<&str>) -> Result<OAuthCallbackParams, ApiError> {
    let Some(raw_query) = raw_query else {
        return Err(invalid_callback());
    };
    if raw_query.len() > MAX_CALLBACK_QUERY_BYTES {
        return Err(invalid_callback());
    }

    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut pairs = 0usize;
    for (name, value) in form_urlencoded::parse(raw_query.as_bytes()) {
        pairs += 1;
        if pairs > MAX_CALLBACK_QUERY_PAIRS || name.len() > MAX_CALLBACK_NAME_BYTES {
            return Err(invalid_callback());
        }
        let value_limit = match name.as_ref() {
            "state" | "code" => MAX_CALLBACK_VALUE_BYTES,
            "error" => MAX_CALLBACK_EXTRA_VALUE_BYTES,
            _ => MAX_CALLBACK_EXTRA_VALUE_BYTES,
        };
        if value.len() > value_limit {
            return Err(invalid_callback());
        }

        let slot = match name.as_ref() {
            "state" => Some(&mut state),
            "code" => Some(&mut code),
            "error" => Some(&mut error),
            _ => None,
        };
        if let Some(slot) = slot
            && slot.replace(value.into_owned()).is_some()
        {
            return Err(invalid_callback());
        }
    }

    let Some(state) = state.filter(|value: &String| !value.is_empty()) else {
        return Err(invalid_callback());
    };
    if code.as_ref().is_some_and(String::is_empty)
        || error.as_ref().is_some_and(String::is_empty)
        || (code.is_none() && error.is_none())
    {
        return Err(invalid_callback());
    }

    // Core's callback semantics give a valid code precedence if a provider
    // includes both `code` and `error`.
    if code.is_some() {
        error = None;
    }
    Ok(OAuthCallbackParams::new(state, code, error))
}

fn invalid_callback() -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "Invalid OAuth callback.".to_owned(),
    }
}
