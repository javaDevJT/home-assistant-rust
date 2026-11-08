use std::fmt;

use async_trait::async_trait;
use hass_oauth2_client::{ExternalOAuthToken, OAuthError, OAuthSession};
use reqwest::{Request, RequestBuilder, header, header::HeaderValue};
use thiserror::Error;

/// Sanitized failures from a Spotify request authorization adapter.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AuthorizationError {
    #[error("OAuth reauthentication is required")]
    ReauthenticationRequired,
    #[error("OAuth token storage is unavailable")]
    Storage,
    #[error("OAuth authorization is temporarily unavailable")]
    Unavailable,
    #[error("OAuth token or request is invalid")]
    InvalidRequest,
}

/// Adds a currently valid bearer token to an API request.
///
/// Implementations must mark the authorization header sensitive and must not
/// include token material in errors or debug output.
#[async_trait]
pub trait RequestAuthorizer: Send + Sync {
    async fn authorize(&self, request: RequestBuilder) -> Result<Request, AuthorizationError>;
}

/// Request authorizer backed by a persisted, refreshable OAuth session.
#[derive(Clone)]
pub struct OAuthSessionAuthorizer {
    session: OAuthSession,
}

impl OAuthSessionAuthorizer {
    /// Create an adapter for the host's per-entry OAuth session.
    pub fn new(session: OAuthSession) -> Self {
        Self { session }
    }
}

impl fmt::Debug for OAuthSessionAuthorizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthSessionAuthorizer")
            .field("session", &"[redacted]")
            .finish()
    }
}

#[async_trait]
impl RequestAuthorizer for OAuthSessionAuthorizer {
    async fn authorize(&self, request: RequestBuilder) -> Result<Request, AuthorizationError> {
        self.session
            .authorized_request(request)
            .await
            .map_err(map_oauth_error)
    }
}

/// Per-call bearer adapter for the profile lookup before a token is persisted.
///
/// The OAuth token type intentionally does not expose an access-token accessor;
/// extract its validated provider map only for this request and never retain or
/// format the value.
pub(crate) struct InitialTokenAuthorizer<'a> {
    token: &'a ExternalOAuthToken,
}

impl<'a> InitialTokenAuthorizer<'a> {
    pub(crate) fn new(token: &'a ExternalOAuthToken) -> Self {
        Self { token }
    }
}

impl fmt::Debug for InitialTokenAuthorizer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitialTokenAuthorizer")
            .field("token", &"[redacted]")
            .finish()
    }
}

#[async_trait]
impl RequestAuthorizer for InitialTokenAuthorizer<'_> {
    async fn authorize(&self, request: RequestBuilder) -> Result<Request, AuthorizationError> {
        let access_token = {
            let mut value = self.token.to_value();
            value
                .as_object_mut()
                .and_then(|fields| fields.remove("access_token"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .filter(|token| !token.is_empty())
                .ok_or(AuthorizationError::InvalidRequest)?
        };
        let mut request = request
            .build()
            .map_err(|_| AuthorizationError::InvalidRequest)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| AuthorizationError::InvalidRequest)?;
        authorization.set_sensitive(true);
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization);
        Ok(request)
    }
}

fn map_oauth_error(error: OAuthError) -> AuthorizationError {
    match error {
        OAuthError::ReauthenticationRequired(_) => AuthorizationError::ReauthenticationRequired,
        OAuthError::Storage => AuthorizationError::Storage,
        OAuthError::Transient(_) | OAuthError::Timeout | OAuthError::Network => {
            AuthorizationError::Unavailable
        }
        OAuthError::InvalidConfiguration
        | OAuthError::InvalidTokenResponse
        | OAuthError::ResponseTooLarge => AuthorizationError::InvalidRequest,
        _ => AuthorizationError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use hass_oauth2_client::ExternalOAuthToken;
    use reqwest::header::AUTHORIZATION;
    use serde_json::json;

    use super::{InitialTokenAuthorizer, RequestAuthorizer};

    #[tokio::test]
    async fn one_shot_authorization_header_is_sensitive_and_not_debuggable() {
        let token = ExternalOAuthToken::from_stored_value(json!({
            "access_token": "one-shot-access-secret",
            "refresh_token": "one-shot-refresh-secret",
            "expires_at": i64::MAX
        }))
        .unwrap();
        let authorizer = InitialTokenAuthorizer::new(&token);
        let request = authorizer
            .authorize(reqwest::Client::new().get("http://127.0.0.1:8123/v1/me"))
            .await
            .unwrap();
        let header = &request.headers()[AUTHORIZATION];
        assert_eq!(header, "Bearer one-shot-access-secret");
        assert!(header.is_sensitive());
        assert!(!format!("{authorizer:?}").contains("one-shot-access-secret"));
        assert!(!format!("{authorizer:?}").contains("one-shot-refresh-secret"));
        assert!(!format!("{token:?}").contains("one-shot-access-secret"));
    }
}
