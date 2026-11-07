//! External OAuth 2.0 client and flow primitives for Home Assistant integrations.
//!
//! This crate is intentionally separate from `hass-auth`: its tokens are
//! provider tokens stored with an integration config entry, not native Home
//! Assistant login sessions. The host supplies a trusted callback URI and an
//! [`OAuthTokenStore`] implementation. No provider catalog or frontend bridge
//! is included here.

use base64::Engine as _;
mod expiry;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{FutureExt, future::BoxFuture};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client as HttpClient, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};
use url::{Host, Url};
use uuid::Uuid;

/// OAuth's expiry safety margin used before a token is considered usable.
pub const EXPIRY_SKEW_SECONDS: i64 = 20;
/// Default per-request timeout for token endpoint calls.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default maximum token response size.
pub const DEFAULT_MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
/// Maximum token response size accepted by a client configuration.
pub const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
/// Default lifetime for a pending authorization flow.
pub const DEFAULT_FLOW_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_FLOWS: usize = 1024;
const MAX_COMPLETED_FLOWS: usize = 2048;

/// Sanitized failures from the client and flow engine.
///
/// Variants intentionally do not retain provider response bodies, endpoint
/// URLs, reqwest errors, callback values, or token material.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OAuthError {
    #[error("invalid OAuth client configuration")]
    InvalidConfiguration,
    #[error("OAuth authorization server endpoint is invalid")]
    InvalidEndpoint,
    #[error("OAuth URLs must use HTTPS except for literal loopback HTTP")]
    InsecureEndpoint,
    #[error("OAuth authorization endpoint contains a reserved parameter")]
    ReservedAuthorizationParameter,
    #[error("OAuth callback is malformed")]
    InvalidCallback,
    #[error("OAuth callback state is invalid")]
    InvalidState,
    #[error("OAuth authorization flow has expired")]
    ExpiredFlow,
    #[error("OAuth authorization flow is unknown")]
    UnknownFlow,
    #[error("OAuth callback has already been handled")]
    ReplayedCallback,
    #[error("OAuth authorization was denied")]
    AccessDenied,
    #[error("OAuth token response is invalid")]
    InvalidTokenResponse,
    #[error("OAuth token response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("OAuth authorization server timed out")]
    Timeout,
    #[error("OAuth authorization server could not be reached")]
    Network,
    #[error("OAuth authorization must be repeated (HTTP {0})")]
    ReauthenticationRequired(u16),
    #[error("OAuth authorization server is temporarily unavailable (HTTP {0})")]
    Transient(u16),
    #[error("OAuth authorization server rejected the request")]
    ProviderError,
    #[error("OAuth token storage failed")]
    Storage,
    #[error("OAuth credentials changed after this authorization started")]
    CredentialConflict,
    #[error("OAuth client factory is shutting down")]
    ShuttingDown,
    #[error("OAuth background task failed")]
    BackgroundTask,
    #[error("OAuth refresh token is unavailable")]
    MissingRefreshToken,
    #[error("too many OAuth authorization flows are pending")]
    TooManyFlows,
}

/// Errors reported by an [`OAuthTokenStore`] implementation.
///
/// Keep adapter-specific errors out of OAuth diagnostics because they may
/// contain config-entry data or serialized tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenStoreError {
    Load,
    Save,
}

/// Asynchronous private storage for an integration's provider token JSON.
///
/// `store` must atomically replace the complete token value for one account.
/// The engine invokes it only after a refresh response has been parsed and
/// validated. Implementations should not log the supplied value. This trait
/// does not provide cross-process compare-and-swap; use one shared factory per
/// storage domain or coordinate other writers at the storage owner.
pub trait OAuthTokenStore: Send + Sync + 'static {
    fn load<'a>(
        &'a self,
        account_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, TokenStoreError>>;

    fn store<'a>(
        &'a self,
        account_id: &'a str,
        provider_token: Value,
    ) -> BoxFuture<'a, Result<(), TokenStoreError>>;
}

/// A clock seam for expiry checks and deterministic tests.
pub trait Clock: Send + Sync + 'static {
    fn now_unix_seconds(&self) -> i64;
}

/// System wall clock implementation used by [`OAuthClientFactory::new`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_seconds(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or(0)
    }
}

/// OAuth client authentication method for token endpoint requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientAuthenticationMethod {
    /// Send `client_id` and `client_secret` in the form body.
    #[default]
    ClientSecretPost,
    /// Send form-encoded client credentials in the HTTP Basic header.
    ClientSecretBasic,
}

/// Client credentials and OAuth server endpoints for one implementation.
///
/// `Debug` deliberately omits all endpoints, client IDs, and extra parameter
/// values. Authorization endpoints commonly contain private provider query
/// configuration; token endpoints may also have query parameters.
#[derive(Clone)]
pub struct OAuthClientConfig {
    client_id: String,
    client_secret: Option<SecretString>,
    authorization_endpoint: Url,
    token_endpoint: Url,
    authentication_method: ClientAuthenticationMethod,
    authorization_parameters: Vec<(String, String)>,
    token_parameters: Vec<(String, String)>,
    request_timeout: Duration,
    max_token_response_bytes: usize,
    flow_ttl: Duration,
    pkce_verifier_length: usize,
}

impl fmt::Debug for OAuthClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientConfig")
            .field("authentication_method", &self.authentication_method)
            .field("client_secret_configured", &self.client_secret.is_some())
            .field(
                "authorization_parameters_count",
                &self.authorization_parameters.len(),
            )
            .field("token_parameters_count", &self.token_parameters.len())
            .field("request_timeout", &self.request_timeout)
            .field("max_token_response_bytes", &self.max_token_response_bytes)
            .field("flow_ttl", &self.flow_ttl)
            .field("pkce_verifier_length", &self.pkce_verifier_length)
            .finish_non_exhaustive()
    }
}

impl OAuthClientConfig {
    /// Create a client configuration from typed OAuth server URLs.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: Option<String>,
        authorization_endpoint: Url,
        token_endpoint: Url,
    ) -> Result<Self, OAuthError> {
        let client_id = client_id.into();
        if client_id.trim().is_empty() || client_id.chars().any(char::is_control) {
            return Err(OAuthError::InvalidConfiguration);
        }
        validate_endpoint(&authorization_endpoint)?;
        validate_endpoint(&token_endpoint)?;
        if authorization_endpoint
            .query_pairs()
            .any(|(name, _)| is_reserved_authorization_parameter(&name))
        {
            return Err(OAuthError::ReservedAuthorizationParameter);
        }

        let config = Self {
            client_id,
            client_secret: client_secret.map(SecretString),
            authorization_endpoint,
            token_endpoint,
            authentication_method: ClientAuthenticationMethod::ClientSecretPost,
            authorization_parameters: Vec::new(),
            token_parameters: Vec::new(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_token_response_bytes: DEFAULT_MAX_TOKEN_RESPONSE_BYTES,
            flow_ttl: DEFAULT_FLOW_TTL,
            pkce_verifier_length: 128,
        };
        config.validate()?;
        Ok(config)
    }

    /// Select client authentication for token endpoint requests.
    pub fn with_authentication_method(mut self, method: ClientAuthenticationMethod) -> Self {
        self.authentication_method = method;
        self
    }

    /// Append fixed provider-specific authorization query parameters.
    ///
    /// Repeated extension keys are retained. Standard OAuth and PKCE fields
    /// are reserved and rejected so extensions cannot replace flow secrets.
    pub fn with_authorization_parameters(
        mut self,
        parameters: Vec<(String, String)>,
    ) -> Result<Self, OAuthError> {
        validate_extension_parameters(&parameters, true)?;
        self.authorization_parameters = parameters;
        Ok(self)
    }

    /// Append fixed provider-specific token form parameters.
    ///
    /// Repeated extension keys are retained. Client-authentication, code,
    /// verifier, grant, redirect, and refresh fields are reserved.
    pub fn with_token_parameters(
        mut self,
        parameters: Vec<(String, String)>,
    ) -> Result<Self, OAuthError> {
        validate_extension_parameters(&parameters, false)?;
        self.token_parameters = parameters;
        Ok(self)
    }

    /// Set the total timeout applied to each token endpoint request.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self, OAuthError> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(OAuthError::InvalidConfiguration);
        }
        self.request_timeout = timeout;
        Ok(self)
    }

    /// Set a bounded token response body size from 1 byte through 1 MiB.
    pub fn with_max_token_response_bytes(mut self, max: usize) -> Result<Self, OAuthError> {
        if max == 0 || max > MAX_TOKEN_RESPONSE_BYTES {
            return Err(OAuthError::InvalidConfiguration);
        }
        self.max_token_response_bytes = max;
        Ok(self)
    }

    /// Set the lifetime of a pending flow, up to one hour.
    pub fn with_flow_ttl(mut self, ttl: Duration) -> Result<Self, OAuthError> {
        if ttl.is_zero() || ttl > Duration::from_secs(60 * 60) {
            return Err(OAuthError::InvalidConfiguration);
        }
        self.flow_ttl = ttl;
        Ok(self)
    }

    /// Set the PKCE verifier length. Valid values are 43 through 128.
    pub fn with_pkce_verifier_length(mut self, length: usize) -> Result<Self, OAuthError> {
        if !(43..=128).contains(&length) {
            return Err(OAuthError::InvalidConfiguration);
        }
        self.pkce_verifier_length = length;
        Ok(self)
    }

    fn validate(&self) -> Result<(), OAuthError> {
        validate_extension_parameters(&self.authorization_parameters, true)?;
        validate_extension_parameters(&self.token_parameters, false)?;
        if self.client_id.trim().is_empty() || self.client_id.chars().any(char::is_control) {
            return Err(OAuthError::InvalidConfiguration);
        }
        validate_endpoint(&self.authorization_endpoint)?;
        validate_endpoint(&self.token_endpoint)?;
        if self
            .authorization_endpoint
            .query_pairs()
            .any(|(name, _)| is_reserved_authorization_parameter(&name))
        {
            return Err(OAuthError::ReservedAuthorizationParameter);
        }
        Ok(())
    }
}

/// Request parameters for one authorization flow.
#[derive(Clone)]
pub struct AuthorizationRequest {
    redirect_uri: Url,
    scopes: Vec<String>,
}

impl fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("scope_count", &self.scopes.len())
            .field("redirect_uri", &"[redacted]")
            .finish()
    }
}

impl AuthorizationRequest {
    /// Create a request with the callback URI supplied by a trusted host
    /// adapter. The adapter owns any public-origin/`HA-Frontend-Base` policy.
    pub fn new(redirect_uri: Url) -> Self {
        Self {
            redirect_uri,
            scopes: Vec::new(),
        }
    }

    /// Set the OAuth scopes in their submitted order.
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }
}

/// Opaque identifier used by a host flow manager to associate a callback.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct OAuthFlowId(Uuid);

impl OAuthFlowId {
    /// Stable opaque identifier for the host's own flow map.
    pub fn as_uuid_string(&self) -> String {
        self.0.to_string()
    }
}

impl fmt::Debug for OAuthFlowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OAuthFlowId([redacted])")
    }
}

/// Browser authorization step. The URL contains the signed state and must
/// only be handed to the host's configured authorization UI.
pub struct AuthorizationStep {
    flow_id: OAuthFlowId,
    authorization_url: Url,
}

impl AuthorizationStep {
    /// Flow identity for the host's private flow registry.
    pub fn flow_id(&self) -> &OAuthFlowId {
        &self.flow_id
    }

    /// Authorization URL to pass to the frontend's external-step protocol.
    /// Do not log or include it in diagnostics; its query contains signed state.
    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }
}

impl fmt::Debug for AuthorizationStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationStep")
            .field("flow_id", &self.flow_id)
            .field("authorization_url", &"[redacted]")
            .finish()
    }
}

/// Untrusted callback query values. `Debug` omits all supplied fields.
pub struct OAuthCallback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

impl OAuthCallback {
    /// Build a callback payload from the callback query.
    pub fn new(state: impl Into<String>, code: Option<String>, error: Option<String>) -> Self {
        Self {
            state: state.into(),
            code,
            error,
        }
    }
}

impl fmt::Debug for OAuthCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCallback")
            .field("state", &"[redacted]")
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("error", &self.error.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Callback values after a host has validated the signed `state`.
///
/// The values remain untrusted. A code is used only for the token exchange
/// and is never retained in a returned result.
pub struct OAuthCallbackPayload {
    code: Option<String>,
    error: Option<String>,
}

impl OAuthCallbackPayload {
    /// Create callback values from the `code` and `error` query parameters.
    pub fn new(code: Option<String>, error: Option<String>) -> Self {
        Self { code, error }
    }
}

impl fmt::Debug for OAuthCallbackPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCallbackPayload")
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("error", &self.error.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Opaque proof that signed callback state belongs to a live private flow.
///
/// A host can inspect the flow ID, bind it to its own flow manager, and emit
/// the framework external-step result before resolving the authorization code.
pub struct VerifiedOAuthCallback {
    flow_id: OAuthFlowId,
    redirect_uri: Url,
    expires_at: i64,
    nonce: String,
}

impl VerifiedOAuthCallback {
    /// Flow identity authenticated by signed state and the private flow store.
    pub fn flow_id(&self) -> &OAuthFlowId {
        &self.flow_id
    }
}

impl fmt::Debug for VerifiedOAuthCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedOAuthCallback")
            .field("flow_id", &self.flow_id)
            .field("context", &"[redacted]")
            .finish()
    }
}

/// Validated result for a one-time OAuth callback.
#[derive(PartialEq)]
pub enum OAuthCallbackResult {
    /// The authorization code was exchanged and is ready for entry creation.
    Authorized {
        flow_id: OAuthFlowId,
        token: ExternalOAuthToken,
    },
    /// The authorization server returned an OAuth denial.
    Denied { flow_id: OAuthFlowId },
}

impl fmt::Debug for OAuthCallbackResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorized { flow_id, .. } => f
                .debug_struct("Authorized")
                .field("flow_id", flow_id)
                .field("token", &"[redacted]")
                .finish(),
            Self::Denied { flow_id } => f.debug_tuple("Denied").field(flow_id).finish(),
        }
    }
}

/// Provider-owned OAuth token object, including unrecognized provider fields.
///
/// Its contents are private and its `Debug` output is fully redacted. Use
/// [`to_value`](Self::to_value) only when the host is committing it to its
/// private config-entry token storage.
#[derive(Clone)]
pub struct ExternalOAuthToken {
    fields: Map<String, Value>,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
    write_permit: Option<TokenWritePermit>,
}

#[derive(Clone, PartialEq, Eq)]
struct TokenWritePermit {
    account_id: String,
    revision: u64,
    factory_id: Uuid,
    client_id: Uuid,
}

impl PartialEq for ExternalOAuthToken {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
            && self.access_token == other.access_token
            && self.refresh_token == other.refresh_token
            && self.expires_at == other.expires_at
    }
}

impl fmt::Debug for ExternalOAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalOAuthToken")
            .field("fields", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl ExternalOAuthToken {
    /// Absolute Unix expiry for a validated provider token.
    pub fn expires_at_unix(&self) -> i64 {
        self.expires_at
    }

    /// Serialize the complete provider token map for private entry storage.
    pub fn to_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    /// Validate a token map loaded from host storage.
    pub fn from_stored_value(value: Value) -> Result<Self, OAuthError> {
        let Value::Object(fields) = value else {
            return Err(OAuthError::InvalidTokenResponse);
        };
        let access_token = parse_access_token(&fields)?;
        let expires_at = fields
            .get("expires_at")
            .and_then(Value::as_i64)
            .ok_or(OAuthError::InvalidTokenResponse)?;
        let refresh_token = usable_refresh_token(&fields);
        Ok(Self {
            fields,
            access_token,
            refresh_token,
            expires_at,
            write_permit: None,
        })
    }

    fn from_provider_map(mut fields: Map<String, Value>, now: i64) -> Result<Self, OAuthError> {
        let access_token = parse_access_token(&fields)?;
        let expires_in = fields
            .get("expires_in")
            .and_then(expiry::seconds)
            .ok_or(OAuthError::InvalidTokenResponse)?;
        let expires_at = now
            .checked_add(expires_in)
            .ok_or(OAuthError::InvalidTokenResponse)?;
        fields.insert(
            "expires_in".to_owned(),
            Value::Number(Number::from(expires_in)),
        );
        fields.insert(
            "expires_at".to_owned(),
            Value::Number(Number::from(expires_at)),
        );
        let refresh_token = usable_refresh_token(&fields);
        Ok(Self {
            fields,
            access_token,
            refresh_token,
            expires_at,
            write_permit: None,
        })
    }

    fn is_usable_at(&self, now: i64) -> bool {
        self.expires_at > now.saturating_add(EXPIRY_SKEW_SECONDS)
    }
}

/// Shared OAuth clients, per-account token revisions, and owned background jobs.
///
/// Create one factory per host/runtime storage domain and use it for all
/// clients that can refresh entries in that domain. Independently created
/// sessions from the factory then serialize refresh and storage by account ID.
/// Revisions coordinate only writers through this factory; external writers
/// must coordinate with the storage owner. Retain a factory owner while using
/// its client/session handles and await `shutdown` before dropping it. Dropping
/// the last factory owner closes admission and aborts retained work as a cleanup
/// fallback; only explicit shutdown preserves the admitted commit barrier.
/// need their own compare-and-swap or single-owner policy.
#[derive(Clone)]
pub struct OAuthClientFactory {
    shared: Arc<FactoryShared>,
    _owner: Arc<FactoryOwner>,
}

struct FactoryOwner {
    shared: Weak<FactoryShared>,
}

impl Drop for FactoryOwner {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            let mut jobs = shared
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            jobs.accepting = false;
            for task in &jobs.abort_handles {
                task.abort();
            }
            if let Some(coordinator) = &jobs.shutdown {
                coordinator.abort.abort();
            }
        }
    }
}

impl fmt::Debug for OAuthClientFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientFactory")
            .field("token_store", &"[redacted]")
            .finish_non_exhaustive()
    }
}

struct FactoryShared {
    id: Uuid,
    token_store: Arc<dyn OAuthTokenStore>,
    clock: Arc<dyn Clock>,
    http: HttpClient,
    account_locks: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
    account_revisions: StdMutex<HashMap<String, u64>>,
    jobs: StdMutex<OwnedJobs>,
}

struct OwnedJobs {
    accepting: bool,
    tasks: Vec<JoinHandle<()>>,
    abort_handles: Vec<AbortHandle>,
    failed: bool,
    shutdown: Option<Arc<ShutdownCoordinator>>,
}

struct ShutdownCoordinator {
    abort: AbortHandle,
    completed: watch::Receiver<Option<bool>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl OAuthClientFactory {
    /// Create a factory with the system wall clock.
    pub fn new(token_store: Arc<dyn OAuthTokenStore>) -> Result<Self, OAuthError> {
        Self::with_clock(token_store, Arc::new(SystemClock))
    }

    /// Create a factory with a host-supplied clock.
    pub fn with_clock(
        token_store: Arc<dyn OAuthTokenStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, OAuthError> {
        Self::build(token_store, clock, false)
    }

    /// Create a factory with an injected clock and no environment proxy.
    ///
    /// This keeps owned loopback fixtures on their listener and also supports
    /// hosts whose transport policy explicitly excludes system proxy settings.
    pub fn with_clock_without_proxy(
        token_store: Arc<dyn OAuthTokenStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, OAuthError> {
        Self::build(token_store, clock, true)
    }

    fn build(
        token_store: Arc<dyn OAuthTokenStore>,
        clock: Arc<dyn Clock>,
        no_proxy: bool,
    ) -> Result<Self, OAuthError> {
        let builder = HttpClient::builder()
            .redirect(Policy::none())
            .timeout(DEFAULT_REQUEST_TIMEOUT);
        let builder = if no_proxy {
            builder.no_proxy()
        } else {
            builder
        };
        let http = builder.build().map_err(|_| OAuthError::Network)?;
        let shared = Arc::new(FactoryShared {
            id: Uuid::new_v4(),
            token_store,
            clock,
            http,
            account_locks: StdMutex::new(HashMap::new()),
            account_revisions: StdMutex::new(HashMap::new()),
            jobs: StdMutex::new(OwnedJobs {
                accepting: true,
                tasks: Vec::new(),
                abort_handles: Vec::new(),
                failed: false,
                shutdown: None,
            }),
        });
        Ok(Self {
            _owner: Arc::new(FactoryOwner {
                shared: Arc::downgrade(&shared),
            }),
            shared,
        })
    }

    /// Construct a client for one typed application-credentials descriptor.
    pub fn client(&self, config: OAuthClientConfig) -> Result<OAuthClient, OAuthError> {
        config.validate()?;
        let mut signing_key = Vec::with_capacity(32);
        signing_key.extend_from_slice(Uuid::new_v4().as_bytes());
        signing_key.extend_from_slice(Uuid::new_v4().as_bytes());
        Ok(OAuthClient {
            inner: Arc::new(ClientInner {
                id: Uuid::new_v4(),
                shared: self.shared.clone(),
                config: Arc::new(config),
                signing_key,
                flows: StdMutex::new(FlowRegistry::default()),
            }),
        })
    }

    /// Stop admitting new work and join all admitted token refresh/commit jobs.
    ///
    /// Call this before shutting down the Tokio runtime. All factory clones
    /// share the same shutdown barrier; later operations return
    /// [`OAuthError::ShuttingDown`].
    pub async fn shutdown(&self) -> Result<(), OAuthError> {
        self.shared.shutdown().await
    }
}

impl FactoryShared {
    async fn ensure_accepting(&self) -> Result<(), OAuthError> {
        if self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting
        {
            Ok(())
        } else {
            Err(OAuthError::ShuttingDown)
        }
    }

    async fn spawn_owned_job<T, F>(
        &self,
        future: F,
    ) -> Result<oneshot::Receiver<Result<T, OAuthError>>, OAuthError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, OAuthError>> + Send + 'static,
    {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !jobs.accepting {
            return Err(OAuthError::ShuttingDown);
        }
        let mut failed = false;
        jobs.tasks.retain_mut(|task| match task.now_or_never() {
            Some(result) => {
                failed |= result.is_err();
                false
            }
            None => true,
        });
        jobs.failed |= failed;
        jobs.abort_handles.retain(|task| !task.is_finished());
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = sender.send(future.await);
        });
        jobs.abort_handles.push(task.abort_handle());
        jobs.tasks.push(task);
        Ok(receiver)
    }

    async fn shutdown(&self) -> Result<(), OAuthError> {
        let coordinator = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(coordinator) = &jobs.shutdown {
                Arc::clone(coordinator)
            } else {
                jobs.accepting = false;
                let admitted = std::mem::take(&mut jobs.tasks);
                let previously_failed = jobs.failed;
                let (sender, completed) = watch::channel(None);
                let handle = tokio::spawn(async move {
                    let mut failed = previously_failed;
                    for task in admitted {
                        if task.await.is_err() {
                            failed = true;
                        }
                    }
                    let _ = sender.send(Some(failed));
                });
                let coordinator = Arc::new(ShutdownCoordinator {
                    abort: handle.abort_handle(),
                    completed,
                    handle: Mutex::new(Some(handle)),
                });
                jobs.shutdown = Some(Arc::clone(&coordinator));
                coordinator
            }
        };

        let mut completed = coordinator.completed.clone();
        let failed = loop {
            if let Some(failed) = *completed.borrow() {
                break failed;
            }
            if completed.changed().await.is_err() {
                break true;
            }
        };

        let mut handle = coordinator.handle.lock().await;
        if let Some(task) = handle.as_mut()
            && task.await.is_err()
        {
            return Err(OAuthError::BackgroundTask);
        }
        handle.take();
        if failed {
            Err(OAuthError::BackgroundTask)
        } else {
            Ok(())
        }
    }

    fn account_revision(&self, account_id: &str) -> u64 {
        *self
            .account_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account_id)
            .unwrap_or(&0)
    }

    fn next_account_revision(&self, account_id: &str) -> Result<u64, OAuthError> {
        self.account_revision(account_id)
            .checked_add(1)
            .ok_or(OAuthError::CredentialConflict)
    }

    fn set_account_revision(&self, account_id: &str, revision: u64) {
        self.account_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(account_id.to_owned(), revision);
    }
}

/// OAuth authorization and token operations for one implementation.
#[derive(Clone)]
pub struct OAuthClient {
    inner: Arc<ClientInner>,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("config", &self.inner.config)
            .field("flows", &"[redacted]")
            .finish_non_exhaustive()
    }
}

struct ClientInner {
    id: Uuid,
    shared: Arc<FactoryShared>,
    config: Arc<OAuthClientConfig>,
    signing_key: Vec<u8>,
    flows: StdMutex<FlowRegistry>,
}

#[derive(Default)]
struct FlowRegistry {
    pending: HashMap<Uuid, PendingFlow>,
    completed: HashMap<Uuid, i64>,
}

struct PendingFlow {
    redirect_uri: Url,
    verifier: SecretString,
    expires_at: i64,
    nonce: String,
    write_permit: Option<TokenWritePermit>,
}

#[derive(Serialize, Deserialize)]
struct SignedState {
    flow_id: String,
    redirect_uri: String,
    #[serde(rename = "exp")]
    expires_at: u64,
    nonce: String,
}

impl OAuthClient {
    /// Start an authorization-code flow and produce its browser URL.
    pub async fn start_authorization(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationStep, OAuthError> {
        self.start_authorization_with_permit(request, None).await
    }

    async fn start_authorization_for_account(
        &self,
        account_id: &str,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationStep, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        let permit = TokenWritePermit {
            account_id: account_id.to_owned(),
            revision: self.inner.shared.account_revision(account_id),
            factory_id: self.inner.shared.id,
            client_id: self.inner.id,
        };
        self.start_authorization_with_permit(request, Some(permit))
            .await
    }

    async fn start_authorization_with_permit(
        &self,
        request: AuthorizationRequest,
        write_permit: Option<TokenWritePermit>,
    ) -> Result<AuthorizationStep, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        if !valid_redirect_uri(&request.redirect_uri)
            || request.scopes.len() > 128
            || request.scopes.iter().any(|scope| {
                scope.trim().is_empty() || scope.len() > 256 || scope.chars().any(char::is_control)
            })
        {
            return Err(OAuthError::InvalidConfiguration);
        }
        let now = self.inner.shared.clock.now_unix_seconds();
        if now < 0 {
            return Err(OAuthError::InvalidConfiguration);
        }
        let ttl = i64::try_from(self.inner.config.flow_ttl.as_secs())
            .map_err(|_| OAuthError::InvalidConfiguration)?;
        let expires_at = now
            .checked_add(ttl)
            .ok_or(OAuthError::InvalidConfiguration)?;
        let flow_id = Uuid::new_v4();
        let verifier = generate_verifier(self.inner.config.pkce_verifier_length);
        let nonce = random_url_safe(32);
        let claims = SignedState {
            flow_id: flow_id.to_string(),
            redirect_uri: request.redirect_uri.as_str().to_owned(),
            expires_at: u64::try_from(expires_at).map_err(|_| OAuthError::InvalidConfiguration)?,
            nonce: nonce.clone(),
        };
        let state = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&self.inner.signing_key),
        )
        .map_err(|_| OAuthError::InvalidState)?;

        let challenge = pkce_challenge(&verifier.0);
        let mut authorization_url = self.inner.config.authorization_endpoint.clone();
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.inner.config.client_id)
                .append_pair("redirect_uri", request.redirect_uri.as_str())
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
            if !request.scopes.is_empty() {
                query.append_pair("scope", &request.scopes.join(" "));
            }
            query.extend_pairs(self.inner.config.authorization_parameters.iter());
        }

        let mut flows = self
            .inner
            .flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        flows.pending.retain(|_, flow| flow.expires_at > now);
        flows.completed.retain(|_, expires_at| *expires_at > now);
        if flows.pending.len() >= MAX_PENDING_FLOWS {
            return Err(OAuthError::TooManyFlows);
        }
        flows.pending.insert(
            flow_id,
            PendingFlow {
                redirect_uri: request.redirect_uri,
                verifier,
                expires_at,
                nonce,
                write_permit,
            },
        );

        Ok(AuthorizationStep {
            flow_id: OAuthFlowId(flow_id),
            authorization_url,
        })
    }

    /// Verify callback state and resolve its values in one operation.
    ///
    /// Hosts that need Home Assistant's intermediate external-done step can
    /// call [`verify_callback_state`](Self::verify_callback_state), return that
    /// step, and later call [`complete_verified_callback`](Self::complete_verified_callback).
    pub async fn complete_callback(
        &self,
        callback: OAuthCallback,
    ) -> Result<OAuthCallbackResult, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        let verified = self.verify_callback_state(&callback.state).await?;
        self.complete_verified_callback(
            verified,
            OAuthCallbackPayload::new(callback.code, callback.error),
        )
        .await
    }

    /// Verify callback state, requiring the expected host flow ID before any
    /// token request. A mismatch leaves the genuine flow pending.
    pub async fn complete_callback_for(
        &self,
        expected_flow_id: &OAuthFlowId,
        callback: OAuthCallback,
    ) -> Result<OAuthCallbackResult, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        let verified = self.verify_callback_state(&callback.state).await?;
        if &verified.flow_id != expected_flow_id {
            return Err(OAuthError::InvalidState);
        }
        self.complete_verified_callback(
            verified,
            OAuthCallbackPayload::new(callback.code, callback.error),
        )
        .await
    }

    /// Verify signed state and bind it to an existing, unexpired flow.
    ///
    /// Verification does not consume the flow. The opaque proof can be used
    /// after the host emits its external-step completion result; actual
    /// resolution remains one-time under the private flow lock.
    pub async fn verify_callback_state(
        &self,
        state: &str,
    ) -> Result<VerifiedOAuthCallback, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        if state.is_empty() || state.len() > 8192 {
            return Err(OAuthError::InvalidState);
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = false;
        validation.leeway = 0;
        let claims = decode::<SignedState>(
            state,
            &DecodingKey::from_secret(&self.inner.signing_key),
            &validation,
        )
        .map_err(|_| OAuthError::InvalidState)?
        .claims;
        let now = self.inner.shared.clock.now_unix_seconds();
        let expires_at = i64::try_from(claims.expires_at).map_err(|_| OAuthError::InvalidState)?;
        if expires_at <= now {
            return Err(OAuthError::ExpiredFlow);
        }
        let flow_uuid = Uuid::parse_str(&claims.flow_id).map_err(|_| OAuthError::InvalidState)?;
        let redirect_uri =
            Url::parse(&claims.redirect_uri).map_err(|_| OAuthError::InvalidState)?;
        {
            let flows = self
                .inner
                .flows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if flows.completed.contains_key(&flow_uuid) {
                return Err(OAuthError::ReplayedCallback);
            }
            let Some(flow) = flows.pending.get(&flow_uuid) else {
                return Err(OAuthError::UnknownFlow);
            };
            if flow.expires_at != expires_at
                || flow.redirect_uri != redirect_uri
                || flow.nonce != claims.nonce
            {
                return Err(OAuthError::InvalidState);
            }
        }
        Ok(VerifiedOAuthCallback {
            flow_id: OAuthFlowId(flow_uuid),
            redirect_uri,
            expires_at,
            nonce: claims.nonce,
        })
    }

    /// Resolve callback values after signed state has been verified.
    ///
    /// When both `code` and `error` are present, code takes precedence, as in
    /// the pinned Home Assistant flow. The private flow is consumed before
    /// exchange so callback replay cannot submit a code twice.
    pub async fn complete_verified_callback(
        &self,
        verified: VerifiedOAuthCallback,
        callback: OAuthCallbackPayload,
    ) -> Result<OAuthCallbackResult, OAuthError> {
        self.inner.shared.ensure_accepting().await?;
        let code = callback
            .code
            .as_deref()
            .filter(|code| !code.trim().is_empty());
        let denial = callback
            .error
            .as_deref()
            .filter(|error| !error.trim().is_empty());
        if code.is_none() && denial.is_none() {
            return Err(OAuthError::InvalidCallback);
        }
        if let Some(code) = code
            && (code.len() > 8192 || code.chars().any(char::is_control))
        {
            return Err(OAuthError::InvalidCallback);
        }
        let now = self.inner.shared.clock.now_unix_seconds();
        if verified.expires_at <= now {
            return Err(OAuthError::ExpiredFlow);
        }
        let flow_uuid = verified.flow_id.0;
        let pending = {
            let mut flows = self
                .inner
                .flows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if flows.completed.contains_key(&flow_uuid) {
                return Err(OAuthError::ReplayedCallback);
            }
            let Some(flow) = flows.pending.get(&flow_uuid) else {
                return Err(OAuthError::UnknownFlow);
            };
            if flow.expires_at != verified.expires_at
                || flow.redirect_uri != verified.redirect_uri
                || flow.nonce != verified.nonce
            {
                return Err(OAuthError::InvalidState);
            }
            let pending = flows
                .pending
                .remove(&flow_uuid)
                .ok_or(OAuthError::UnknownFlow)?;
            flows.completed.retain(|_, expiry| *expiry > now);
            if flows.completed.len() >= MAX_COMPLETED_FLOWS
                && let Some(oldest) = flows
                    .completed
                    .iter()
                    .min_by_key(|(_, expiry)| *expiry)
                    .map(|(id, _)| *id)
            {
                flows.completed.remove(&oldest);
            }
            flows.completed.insert(flow_uuid, verified.expires_at);
            pending
        };
        let flow_id = verified.flow_id;
        let Some(code) = code else {
            return Ok(OAuthCallbackResult::Denied { flow_id });
        };
        let mut token = self.exchange_code(&pending, code).await?;
        token.write_permit = pending.write_permit;
        Ok(OAuthCallbackResult::Authorized { flow_id, token })
    }

    /// Remove an unconsumed authorization flow and release its private verifier.
    ///
    /// Cleanup is allowed after factory admission closes. An exchange that has
    /// already consumed the flow must be canceled by its owning resolver; this
    /// method returns false for such flows and for unknown IDs.
    pub fn cancel_authorization(&self, flow_id: &OAuthFlowId) -> bool {
        self.inner
            .flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&flow_id.0)
            .is_some()
    }

    /// Create a token session for one config entry/account key.
    pub fn session(&self, account_id: impl Into<String>) -> Result<OAuthSession, OAuthError> {
        let account_id = account_id.into();
        if account_id.trim().is_empty()
            || account_id.len() > 256
            || account_id.chars().any(char::is_control)
        {
            return Err(OAuthError::InvalidConfiguration);
        }
        let account_lock = {
            let mut locks = self
                .inner
                .shared
                .account_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.retain(|_, weak| weak.strong_count() > 0);
            if let Some(existing) = locks.get(&account_id).and_then(Weak::upgrade) {
                existing
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(account_id.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        Ok(OAuthSession {
            client: self.clone(),
            account_id,
            account_lock,
        })
    }

    async fn exchange_code(
        &self,
        flow: &PendingFlow,
        code: &str,
    ) -> Result<ExternalOAuthToken, OAuthError> {
        if code.len() > 8192 || code.chars().any(char::is_control) {
            return Err(OAuthError::InvalidCallback);
        }
        let params = vec![
            ("grant_type".to_owned(), "authorization_code".to_owned()),
            ("code".to_owned(), code.to_owned()),
            (
                "redirect_uri".to_owned(),
                flow.redirect_uri.as_str().to_owned(),
            ),
            ("code_verifier".to_owned(), flow.verifier.0.clone()),
        ];
        let map = self.send_token_request(params).await?;
        ExternalOAuthToken::from_provider_map(map, self.inner.shared.clock.now_unix_seconds())
    }

    async fn refresh_token(
        &self,
        token: &ExternalOAuthToken,
    ) -> Result<ExternalOAuthToken, OAuthError> {
        let refresh_token = token
            .refresh_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(OAuthError::MissingRefreshToken)?;
        let params = vec![
            ("grant_type".to_owned(), "refresh_token".to_owned()),
            ("refresh_token".to_owned(), refresh_token.to_owned()),
        ];
        let response = self.send_token_request(params).await?;
        let now = self.inner.shared.clock.now_unix_seconds();
        let replacement = ExternalOAuthToken::from_provider_map(response, now)?;
        let mut merged = token.fields.clone();
        for (key, value) in replacement.fields {
            if key == "refresh_token"
                && value
                    .as_str()
                    .is_none_or(|replacement| replacement.trim().is_empty())
            {
                continue;
            }
            merged.insert(key, value);
        }
        ExternalOAuthToken::from_provider_map(merged, now)
    }

    async fn send_token_request(
        &self,
        mut parameters: Vec<(String, String)>,
    ) -> Result<Map<String, Value>, OAuthError> {
        let has_secret = self
            .inner
            .config
            .client_secret
            .as_ref()
            .is_some_and(|secret| !secret.0.is_empty());
        let mut basic_auth = None;
        match (self.inner.config.authentication_method, has_secret) {
            (ClientAuthenticationMethod::ClientSecretBasic, true) => {
                let secret = &self
                    .inner
                    .config
                    .client_secret
                    .as_ref()
                    .ok_or(OAuthError::InvalidConfiguration)?
                    .0;
                let basic = format!(
                    "{}:{}",
                    form_component(&self.inner.config.client_id),
                    form_component(secret)
                );
                basic_auth = Some(format!("Basic {}", STANDARD.encode(basic.as_bytes())));
            }
            _ => {
                parameters.push(("client_id".to_owned(), self.inner.config.client_id.clone()));
                if let Some(secret) = &self.inner.config.client_secret
                    && !secret.0.is_empty()
                {
                    parameters.push(("client_secret".to_owned(), secret.0.clone()));
                }
            }
        }
        parameters.extend(self.inner.config.token_parameters.iter().cloned());
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(parameters.iter())
            .finish();
        let mut request = self
            .inner
            .shared
            .http
            .post(self.inner.config.token_endpoint.clone())
            .timeout(self.inner.config.request_timeout)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        if let Some(authorization) = basic_auth {
            let mut authorization = HeaderValue::from_str(&authorization)
                .map_err(|_| OAuthError::InvalidConfiguration)?;
            authorization.set_sensitive(true);
            request = request.header(AUTHORIZATION, authorization);
        }
        let mut response = request.send().await.map_err(map_transport_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(map_status(status));
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.inner.config.max_token_response_bytes as u64)
        {
            return Err(OAuthError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
            if body.len().saturating_add(chunk.len()) > self.inner.config.max_token_response_bytes {
                return Err(OAuthError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| OAuthError::InvalidTokenResponse)?;
        let Value::Object(map) = value else {
            return Err(OAuthError::InvalidTokenResponse);
        };
        Ok(map)
    }
}

/// A per-config-entry bearer session with serialized refresh and persistence.
#[derive(Clone)]
pub struct OAuthSession {
    client: OAuthClient,
    account_id: String,
    account_lock: Arc<Mutex<()>>,
}

impl fmt::Debug for OAuthSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthSession")
            .field("account_id", &"[redacted]")
            .field("client", &self.client)
            .finish()
    }
}

impl OAuthSession {
    /// Start authorization bound to this account's current factory revision.
    ///
    /// Call this before exposing the authorization URL and keep this session
    /// for [`save_token`](Self::save_token). A later credential commit through
    /// the same factory makes the returned token conflict instead of replacing
    /// newer credentials.
    pub async fn start_authorization(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationStep, OAuthError> {
        self.client
            .start_authorization_for_account(&self.account_id, request)
            .await
    }

    /// Persist a validated token if its authorization revision is still current.
    ///
    /// Session-bound authorization results use an account-specific factory
    /// revision. Direct client flows have no account binding and can only create
    /// a token when the factory has never committed credentials for this
    /// account and storage is empty. Coordination covers writers using this
    /// factory; other processes or stores must provide their own CAS/owner
    /// policy because [`OAuthTokenStore`] has no cross-process compare-and-swap.
    pub async fn save_token(&self, token: &ExternalOAuthToken) -> Result<(), OAuthError> {
        let account_id = self.account_id.clone();
        let store = Arc::clone(&self.client.inner.shared.token_store);
        self.commit_authorization(token, move |provider_token| async move {
            store
                .store(&account_id, provider_token)
                .await
                .map_err(|_| OAuthError::Storage)
        })
        .await
    }

    /// Commit authorization through the host's private atomic transaction.
    ///
    /// The callback runs inside the account's validated revision barrier, as
    /// owned work that survives cancellation of its waiting caller. It may
    /// update a config entry and complete its exact reauth flow in one write;
    /// the revision advances only after that write succeeds. Defer integration
    /// setup/reload and other callbacks until this method returns. The callback
    /// must not await this account's token operations or factory shutdown.
    pub async fn commit_authorization<T, F, Fut>(
        &self,
        token: &ExternalOAuthToken,
        persist: F,
    ) -> Result<T, OAuthError>
    where
        T: Send + 'static,
        F: FnOnce(Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, OAuthError>> + Send + 'static,
    {
        if token.write_permit.as_ref().is_some_and(|permit| {
            permit.account_id != self.account_id
                || permit.factory_id != self.client.inner.shared.id
                || permit.client_id != self.client.inner.id
        }) {
            return Err(OAuthError::CredentialConflict);
        }

        let account_id = self.account_id.clone();
        let account_lock = Arc::clone(&self.account_lock);
        let store = Arc::clone(&self.client.inner.shared.token_store);
        let shared = Arc::clone(&self.client.inner.shared);
        let job_shared = Arc::clone(&shared);
        let provider_token = token.to_value();
        let write_permit = token.write_permit.clone();
        let receiver = shared
            .spawn_owned_job(async move {
                let _guard = account_lock.lock().await;
                let current_revision = job_shared.account_revision(&account_id);
                match write_permit {
                    Some(permit) if permit.revision == current_revision => {}
                    Some(_) => return Err(OAuthError::CredentialConflict),
                    None if current_revision == 0 => {
                        if store
                            .load(&account_id)
                            .await
                            .map_err(|_| OAuthError::Storage)?
                            .is_some()
                        {
                            return Err(OAuthError::CredentialConflict);
                        }
                    }
                    None => return Err(OAuthError::CredentialConflict),
                }
                let next_revision = current_revision
                    .checked_add(1)
                    .ok_or(OAuthError::CredentialConflict)?;
                let result = persist(provider_token).await?;
                job_shared.set_account_revision(&account_id, next_revision);
                Ok(result)
            })
            .await?;
        receiver.await.map_err(|_| OAuthError::BackgroundTask)?
    }

    /// Attach a refreshed, current bearer token to an HTTPS or literal-loopback
    /// HTTP reqwest request.
    ///
    /// Independently created sessions from the same factory and account key
    /// share the refresh lock. The token is reloaded after taking that lock,
    /// and refresh is committed before the request receives its bearer value.
    pub async fn authorized_request(
        &self,
        request: RequestBuilder,
    ) -> Result<reqwest::Request, OAuthError> {
        self.client.inner.shared.ensure_accepting().await?;
        let mut request = request
            .build()
            .map_err(|_| OAuthError::InvalidConfiguration)?;
        validate_endpoint(request.url())?;
        let token = self.current_token().await?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", token.access_token))
            .map_err(|_| OAuthError::InvalidTokenResponse)?;
        authorization.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, authorization);
        Ok(request)
    }

    async fn current_token(&self) -> Result<ExternalOAuthToken, OAuthError> {
        self.client.inner.shared.ensure_accepting().await?;
        let account_id = self.account_id.clone();
        let account_lock = Arc::clone(&self.account_lock);
        let client = self.client.clone();
        let shared = Arc::clone(&self.client.inner.shared);
        let job_shared = Arc::clone(&shared);
        let receiver = shared
            .spawn_owned_job(async move {
                let _guard = account_lock.lock().await;
                let stored = job_shared
                    .token_store
                    .load(&account_id)
                    .await
                    .map_err(|_| OAuthError::Storage)?
                    .ok_or(OAuthError::ReauthenticationRequired(401))?;
                let token = ExternalOAuthToken::from_stored_value(stored)?;
                if token.is_usable_at(job_shared.clock.now_unix_seconds()) {
                    return Ok(token);
                }

                let refreshed = client.refresh_token(&token).await?;
                let next_revision = job_shared.next_account_revision(&account_id)?;
                job_shared
                    .token_store
                    .store(&account_id, refreshed.to_value())
                    .await
                    .map_err(|_| OAuthError::Storage)?;
                job_shared.set_account_revision(&account_id, next_revision);
                Ok(refreshed)
            })
            .await?;
        receiver.await.map_err(|_| OAuthError::BackgroundTask)?
    }
}

#[derive(Clone)]
struct SecretString(String);

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

fn parse_access_token(fields: &Map<String, Value>) -> Result<String, OAuthError> {
    let access_token = fields
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(OAuthError::InvalidTokenResponse)?;
    if access_token
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(OAuthError::InvalidTokenResponse);
    }
    Ok(access_token.to_owned())
}

fn usable_refresh_token(fields: &Map<String, Value>) -> Option<String> {
    fields
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn validate_endpoint(url: &Url) -> Result<(), OAuthError> {
    if url.host().is_none() || has_user_information(url) || url.fragment().is_some() {
        return Err(OAuthError::InvalidEndpoint);
    }
    if url.scheme() == "https" || is_literal_loopback_http(url) {
        Ok(())
    } else {
        Err(OAuthError::InsecureEndpoint)
    }
}

fn has_user_information(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    url.as_str()
        .split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn valid_redirect_uri(url: &Url) -> bool {
    validate_endpoint(url).is_ok() && url.query().is_none()
}

fn is_literal_loopback_http(url: &Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    }
}

fn is_reserved_authorization_parameter(name: &str) -> bool {
    matches!(
        name,
        "state"
            | "client_id"
            | "redirect_uri"
            | "response_type"
            | "scope"
            | "code_challenge"
            | "code_challenge_method"
    )
}

fn validate_extension_parameters(
    parameters: &[(String, String)],
    authorization: bool,
) -> Result<(), OAuthError> {
    if parameters.len() > 128 {
        return Err(OAuthError::InvalidConfiguration);
    }
    for (name, value) in parameters {
        if name.is_empty()
            || name.len() > 128
            || value.len() > 8192
            || name.chars().any(char::is_control)
            || value.chars().any(char::is_control)
        {
            return Err(OAuthError::InvalidConfiguration);
        }
        let reserved = if authorization {
            matches!(
                name.as_str(),
                "response_type"
                    | "client_id"
                    | "redirect_uri"
                    | "state"
                    | "scope"
                    | "code_challenge"
                    | "code_challenge_method"
            )
        } else {
            matches!(
                name.as_str(),
                "grant_type"
                    | "code"
                    | "redirect_uri"
                    | "client_id"
                    | "client_secret"
                    | "code_verifier"
                    | "refresh_token"
            )
        };
        if reserved {
            return Err(OAuthError::InvalidConfiguration);
        }
    }
    Ok(())
}

fn random_url_safe(size: usize) -> String {
    let target_bytes = size.saturating_add(2).saturating_mul(3) / 4 + 1;
    let mut bytes = Vec::with_capacity(target_bytes);
    while bytes.len() < target_bytes {
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(bytes).chars().take(size).collect()
}

fn generate_verifier(length: usize) -> SecretString {
    SecretString(random_url_safe(length))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn form_component(value: &str) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("x", value)
        .finish();
    encoded.strip_prefix("x=").unwrap_or(&encoded).to_owned()
}

fn map_transport_error(error: reqwest::Error) -> OAuthError {
    if error.is_timeout() {
        OAuthError::Timeout
    } else {
        OAuthError::Network
    }
}

fn map_status(status: StatusCode) -> OAuthError {
    let value = status.as_u16();
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        OAuthError::Transient(value)
    } else if status.is_client_error() {
        OAuthError::ReauthenticationRequired(value)
    } else {
        OAuthError::ProviderError
    }
}

#[cfg(test)]
mod tests;
