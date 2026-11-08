//! Native config-flow orchestration for the local OAuth 2.0 client.
//!
//! The host keeps OAuth proof, callback values, and provider tokens in private
//! flow state. Only `ExternalStep`, `ExternalDone`, and ordinary Core flow
//! results cross the config-flow boundary.

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use hass_application_credentials::{
    ApplicationCredentialsError, ApplicationCredentialsManager, ApplicationCredentialsPlatform,
    ApplicationCredentialsPlatformRegistry, LocalOAuth2Implementation, PlatformRegistrationGuard,
    ResolvedOAuthImplementation,
};
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowContinuation, ConfigFlowRequest,
    ConfigFlowResult, ConfigSubentryFlowRequest, ConfigSubentryFlowResult, ConfigSubentryTypeInfo,
    DeferredExternalFlowCompletion, ExternalFlowLease, SetupOutcome,
};
use hass_core::{Error as CoreError, Runtime};
use hass_oauth2_client::{
    AuthorizationRequest, ExternalOAuthToken, OAuthCallbackPayload, OAuthCallbackResult,
    OAuthClient, OAuthClientConfig, OAuthClientFactory, OAuthError, OAuthFlowId, OAuthSession,
    OAuthTokenStore, TokenStoreError, VerifiedOAuthCallback,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinHandle};
use url::Url;
use uuid::Uuid;

const AUTH_STEP_ID: &str = "auth";
const IMPLEMENTATION_STEP_ID: &str = "auth_implementation";
const CREATION_STEP_ID: &str = "creation";
const REJECTED_STEP_ID: &str = "authorize_rejected";
const AUTH_IMPLEMENTATION_FIELD: &str = "auth_implementation";
const TOKEN_FIELD: &str = "token";
const DEFAULT_MAX_FLOWS: usize = 128;
const MAX_CALLBACK_VALUE_BYTES: usize = 8192;

/// Failures from the native OAuth host bridge.
#[derive(Debug, Error)]
pub enum OAuthFlowError {
    #[error("invalid OAuth flow host configuration")]
    InvalidConfiguration,
    #[error("OAuth callback is malformed")]
    InvalidCallback,
    #[error("OAuth authorization flow has expired")]
    ExpiredFlow,
    #[error("OAuth flow is unknown")]
    UnknownFlow,
    #[error("OAuth callback has already been handled")]
    ReplayedCallback,
    #[error("OAuth flow has been aborted")]
    FlowAborted,
    #[error("application credentials are missing")]
    MissingCredentials,
    #[error("OAuth implementation is unavailable")]
    OAuthImplementationUnavailable,
    #[error("opaque custom OAuth implementations are not supported by the local engine")]
    UnsupportedCustomImplementation,
    #[error("too many OAuth flows are active")]
    TooManyFlows,
    #[error("OAuth flow host is shutting down")]
    ShuttingDown,
    #[error("OAuth provider callback failed")]
    ProviderFailure,
    /// A trusted provider's static, translated config-flow abort reason.
    #[error("the OAuth provider aborted authorization")]
    ProviderAbort(&'static str),
    #[error("configuration entry is not available")]
    EntryUnavailable,
    #[error("configuration flow operation failed")]
    Core(#[source] CoreError),
    #[error("application credentials operation failed")]
    ApplicationCredentials(#[source] ApplicationCredentialsError),
    #[error("OAuth client operation failed")]
    OAuth(#[source] OAuthError),
}

impl From<CoreError> for OAuthFlowError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

impl From<ApplicationCredentialsError> for OAuthFlowError {
    fn from(error: ApplicationCredentialsError) -> Self {
        Self::ApplicationCredentials(error)
    }
}

impl From<OAuthError> for OAuthFlowError {
    fn from(error: OAuthError) -> Self {
        match error {
            OAuthError::InvalidState | OAuthError::InvalidCallback => Self::InvalidCallback,
            OAuthError::ExpiredFlow => Self::ExpiredFlow,
            OAuthError::UnknownFlow => Self::UnknownFlow,
            OAuthError::ReplayedCallback => Self::ReplayedCallback,
            OAuthError::TooManyFlows => Self::TooManyFlows,
            OAuthError::ShuttingDown => Self::ShuttingDown,
            other => Self::OAuth(other),
        }
    }
}

/// Fixed, trusted callback URL and private flow bounds for one host.
///
/// The callback URL must be validated by the application from trusted
/// configuration. It is never derived from an incoming request header.
#[derive(Clone)]
pub struct OAuthFlowHostOptions {
    callback_url: Url,
    max_active_flows: usize,
    resolver_timeout: Duration,
    flow_ttl: Duration,
}

impl fmt::Debug for OAuthFlowHostOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlowHostOptions")
            .field("callback_url", &"[redacted]")
            .field("max_active_flows", &self.max_active_flows)
            .field("resolver_timeout", &self.resolver_timeout)
            .field("flow_ttl", &self.flow_ttl)
            .finish()
    }
}

impl OAuthFlowHostOptions {
    /// Create host options around a callback URL validated by the application.
    pub fn new(callback_url: Url) -> Self {
        Self {
            callback_url,
            max_active_flows: DEFAULT_MAX_FLOWS,
            resolver_timeout: Duration::from_secs(60),
            flow_ttl: Duration::from_secs(10 * 60),
        }
    }

    /// Bound the number of active or resolver-owned host flows.
    pub fn with_max_active_flows(mut self, maximum: usize) -> Self {
        self.max_active_flows = maximum;
        self
    }

    /// Bound token exchange and provider post-authorization work.
    pub fn with_resolver_timeout(mut self, timeout: Duration) -> Self {
        self.resolver_timeout = timeout;
        self
    }

    /// Bound how long an OAuth flow can remain in private host state.
    pub fn with_flow_ttl(mut self, ttl: Duration) -> Self {
        self.flow_ttl = ttl;
        self
    }
}

/// Sanitized query values passed by the public callback adapter.
pub struct OAuthCallbackParams {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

impl OAuthCallbackParams {
    /// Construct callback inputs from the parsed query string.
    pub fn new(state: impl Into<String>, code: Option<String>, error: Option<String>) -> Self {
        Self {
            state: state.into(),
            code,
            error,
        }
    }
}

impl fmt::Debug for OAuthCallbackParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCallbackParams")
            .field("state", &"[redacted]")
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("error", &self.error.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Successful callback-admission receipt with no callback or token values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OAuthCallbackResponse;

/// Trusted provider input after a code exchange. The raw authorization code
/// and signed callback state are deliberately absent.
pub struct OAuthPostAuthorizationContext {
    pub request: ConfigFlowRequest,
    pub existing_entry: Option<ConfigEntry>,
    pub implementation: LocalOAuth2Implementation,
    pub token: ExternalOAuthToken,
}

impl fmt::Debug for OAuthPostAuthorizationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthPostAuthorizationContext")
            .field("flow_id", &self.request.flow_id)
            .field("domain", &self.request.domain)
            .field("source", &self.request.source)
            .field("entry_id", &self.request.entry_id)
            .field("implementation", &self.implementation)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Entry metadata returned by a provider after it validates the authorized
/// identity. The host appends `auth_implementation` and the provider token.
pub struct OAuthEntryDetails {
    pub title: String,
    pub data: Map<String, Value>,
    pub options: Map<String, Value>,
    pub unique_id: Option<String>,
    pub version: u32,
    pub minor_version: u32,
}

impl OAuthEntryDetails {
    /// Construct entry metadata with the integration driver's current version.
    pub fn new(
        title: impl Into<String>,
        data: Map<String, Value>,
        options: Map<String, Value>,
        unique_id: Option<String>,
        version: (u32, u32),
    ) -> Self {
        Self {
            title: title.into(),
            data,
            options,
            unique_id,
            version: version.0,
            minor_version: version.1,
        }
    }
}

impl fmt::Debug for OAuthEntryDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthEntryDetails")
            .field("title", &self.title)
            .field("data", &"[redacted]")
            .field("options", &"[redacted]")
            .field("unique_id", &self.unique_id)
            .field("version", &self.version)
            .field("minor_version", &self.minor_version)
            .finish()
    }
}

/// A native integration that uses locally resolved application credentials.
///
/// Implement `ConfigEntryDriver` for setup, unload, options, and lifecycle
/// behavior. The host wrapper owns the OAuth config-flow steps and invokes
/// `post_authorize` after exchanging a verified code.
pub trait OAuthFlowProvider: ConfigEntryDriver {
    /// Register the actual, loaded application-credentials platform.
    fn application_credentials_platform(&self) -> ApplicationCredentialsPlatform;

    /// Return scopes for this flow; the request is private to the integration.
    fn oauth_scopes(&self, _request: &ConfigFlowRequest) -> Vec<String> {
        Vec::new()
    }

    /// Optionally customize the SDK client for one typed local descriptor.
    fn oauth_client_config(
        &self,
        implementation: &LocalOAuth2Implementation,
    ) -> Result<OAuthClientConfig, OAuthFlowError> {
        let authorize = Url::parse(&implementation.authorization_server.authorize_url)
            .map_err(|_| OAuthFlowError::InvalidConfiguration)?;
        let token = Url::parse(&implementation.authorization_server.token_url)
            .map_err(|_| OAuthFlowError::InvalidConfiguration)?;
        OAuthClientConfig::new(
            implementation.client_id.clone(),
            Some(implementation.client_secret().to_owned()),
            authorize,
            token,
        )
        .map_err(OAuthFlowError::from)
    }

    /// Resolve provider identity and construct private entry metadata.
    fn post_authorize<'a>(
        &'a self,
        context: OAuthPostAuthorizationContext,
    ) -> BoxFuture<'a, Result<OAuthEntryDetails, OAuthFlowError>>;
}

/// Private config-entry backed implementation of the OAuth client token store.
pub struct ConfigEntryTokenStore {
    entries: ConfigEntries,
}

impl ConfigEntryTokenStore {
    /// Create an adapter for the same private config-entry manager used by the
    /// host and its token-refresh sessions.
    pub fn new(entries: ConfigEntries) -> Self {
        Self { entries }
    }
}

impl fmt::Debug for ConfigEntryTokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigEntryTokenStore")
            .field("entries", &"[private]")
            .finish()
    }
}

impl OAuthTokenStore for ConfigEntryTokenStore {
    fn load<'a>(
        &'a self,
        account_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, TokenStoreError>> {
        Box::pin(async move {
            Ok(self
                .entries
                .get_entry(account_id)
                .and_then(|entry| entry.data.get(TOKEN_FIELD).cloned()))
        })
    }

    fn store<'a>(
        &'a self,
        account_id: &'a str,
        provider_token: Value,
    ) -> BoxFuture<'a, Result<(), TokenStoreError>> {
        Box::pin(async move {
            let Some(entry) = self.entries.get_entry(account_id) else {
                return Err(TokenStoreError::Save);
            };
            let mut patch = Map::new();
            patch.insert(TOKEN_FIELD.to_owned(), provider_token);
            self.entries
                .patch_entry_data(account_id, &entry.domain, patch)
                .map_err(|_| TokenStoreError::Save)?;
            Ok(())
        })
    }
}

/// Strong lifecycle owner for native OAuth config flows.
pub struct OAuthFlowHost {
    inner: Arc<HostInner>,
}

impl fmt::Debug for OAuthFlowHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlowHost")
            .field("active_flows", &self.active_flow_count())
            .finish_non_exhaustive()
    }
}

impl OAuthFlowHost {
    /// Build a host bridge around the same config-entry, application-credentials,
    /// and OAuth factory instances used by the application.
    pub fn new(
        entries: ConfigEntries,
        app_credentials: ApplicationCredentialsManager,
        platform_registry: ApplicationCredentialsPlatformRegistry,
        factory: OAuthClientFactory,
        options: OAuthFlowHostOptions,
    ) -> Result<Self, OAuthFlowError> {
        validate_options(&options)?;
        let max_active_flows = options.max_active_flows;
        Ok(Self {
            inner: Arc::new(HostInner {
                entries,
                app_credentials,
                platform_registry,
                factory,
                options,
                state: Mutex::new(HostState {
                    accepting: true,
                    providers: HashMap::new(),
                    flows: HashMap::new(),
                    state_index: HashMap::new(),
                    resolver_jobs: HashMap::new(),
                    flow_cleanup_jobs: HashMap::new(),
                    provider_cleanup_jobs: HashMap::new(),
                    pending_provider_cleanup: HashMap::new(),
                    shutdown_cleanup_job: None,
                }),
                slots: Arc::new(tokio::sync::Semaphore::new(max_active_flows)),
                client_cache: Arc::new(Mutex::new(HashMap::new())),
            }),
        })
    }

    /// Return a weak handle suitable for API features and provider callbacks.
    pub fn manager(&self) -> OAuthFlowManager {
        OAuthFlowManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Register one native OAuth provider and its loaded application-credentials
    /// platform. Dropping the guard removes only this registration.
    pub fn register_provider(
        &self,
        provider: Arc<dyn OAuthFlowProvider>,
    ) -> Result<OAuthFlowRegistrationGuard, OAuthFlowError> {
        self.inner.register_provider(provider)
    }

    /// Explicitly cancel and join host-owned resolvers, invalidate Core flows,
    /// and unregister provider/platform guards. The OAuth factory itself stays
    /// owned by the application lifecycle.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// Current number of private OAuth/config-flow records held by this host.
    pub fn active_flow_count(&self) -> usize {
        lock(&self.inner.state).flows.len()
    }
}

/// Weak public flow manager. API transport may retain this value without
/// extending the application owner's lifetime.
#[derive(Clone)]
pub struct OAuthFlowManager {
    inner: Weak<HostInner>,
}

impl fmt::Debug for OAuthFlowManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlowManager")
            .field("host", &"[weak]")
            .finish()
    }
}

/// Exact provider and application-credentials registration handle.
pub struct OAuthFlowRegistrationGuard {
    inner: Weak<HostInner>,
    domain: String,
    registration_id: Uuid,
    active: bool,
}

impl fmt::Debug for OAuthFlowRegistrationGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthFlowRegistrationGuard")
            .field("domain", &self.domain)
            .field("active", &self.active)
            .finish()
    }
}

impl OAuthFlowRegistrationGuard {
    /// Unregister this exact provider after canceling and joining its owned
    /// resolver jobs. The guard does not keep the host alive.
    pub async fn unregister(mut self) {
        let cleanup = self
            .inner
            .upgrade()
            .map(|inner| inner.begin_provider_cleanup(&self.domain, self.registration_id));
        self.active = false;
        if let Some(cleanup) = cleanup {
            cleanup.wait().await;
        }
    }
}

impl Drop for OAuthFlowRegistrationGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            inner.unregister_provider_fallback(&self.domain, self.registration_id);
        }
        self.active = false;
    }
}

struct HostInner {
    entries: ConfigEntries,
    app_credentials: ApplicationCredentialsManager,
    platform_registry: ApplicationCredentialsPlatformRegistry,
    factory: OAuthClientFactory,
    options: OAuthFlowHostOptions,
    state: Mutex<HostState>,
    slots: Arc<tokio::sync::Semaphore>,
    client_cache: Arc<Mutex<HashMap<ClientCacheKey, OAuthClient>>>,
}

struct HostState {
    accepting: bool,
    providers: HashMap<String, ProviderRegistration>,
    flows: HashMap<String, Arc<FlowRecord>>,
    state_index: HashMap<String, String>,
    resolver_jobs: HashMap<String, ResolverJob>,
    flow_cleanup_jobs: HashMap<String, FlowCleanupRegistration>,
    provider_cleanup_jobs: HashMap<Uuid, Arc<LifecycleCleanup>>,
    pending_provider_cleanup: HashMap<Uuid, String>,
    shutdown_cleanup_job: Option<Arc<LifecycleCleanup>>,
}

struct FlowCleanupRegistration {
    registration_id: Uuid,
    reason: FlowCleanupReason,
    job: Arc<LifecycleCleanup>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FlowCleanupReason {
    Expired,
    Unregistered,
    Shutdown,
}

struct LifecycleCleanup {
    id: Uuid,
    completed: watch::Receiver<bool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl LifecycleCleanup {
    fn pending() -> (Arc<Self>, watch::Sender<bool>) {
        let (sender, completed) = watch::channel(false);
        (
            Arc::new(Self {
                id: Uuid::new_v4(),
                completed,
                join: Mutex::new(None),
            }),
            sender,
        )
    }

    fn completed() -> Arc<Self> {
        let (job, sender) = Self::pending();
        sender.send_replace(true);
        job
    }

    fn retain(&self, join: JoinHandle<()>) {
        *lock(&self.join) = Some(join);
    }

    fn is_complete(&self) -> bool {
        *self.completed.borrow()
    }

    async fn wait(&self) {
        let mut receiver = self.completed.clone();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

struct ProviderRegistration {
    id: Uuid,
    provider: Arc<dyn OAuthFlowProvider>,
    driver: Arc<dyn ConfigEntryDriver>,
    _platform_guard: PlatformRegistrationGuard,
}

struct ResolverJob {
    abort: AbortHandle,
    join: JoinHandle<()>,
}

struct FlowCleanupWork {
    flow_id: String,
    registration_id: Uuid,
    flow: Arc<FlowRecord>,
    phase: FlowPhase,
    resolver_job: Option<ResolverJob>,
}

struct FlowCleanupStart {
    job: Arc<LifecycleCleanup>,
    completion: watch::Sender<bool>,
    work: FlowCleanupWork,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct ClientCacheKey {
    registration_id: Uuid,
    domain: String,
    auth_domain: String,
    client_id: String,
}

struct FlowRecord {
    flow_id: String,
    request: Mutex<ConfigFlowRequest>,
    registration_id: Uuid,
    provider: Arc<dyn OAuthFlowProvider>,
    data: Mutex<FlowPrivateData>,
    _slot: Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
    expires_at: Instant,
    completion: watch::Sender<Option<ResolverOutcome>>,
}

struct FlowPrivateData {
    phase: FlowPhase,
    implementation: Option<LocalOAuth2Implementation>,
    auth_implementation: Option<String>,
    client: Option<OAuthClient>,
    session: Option<OAuthSession>,
    sdk_flow_id: Option<OAuthFlowId>,
    state_digest: Option<String>,
    callback: Option<StagedCallback>,
    token: Option<ExternalOAuthToken>,
}

#[derive(Clone)]
enum ImplementationChoice {
    Local {
        implementation: LocalOAuth2Implementation,
        label: String,
    },
    Custom {
        auth_domain: String,
        label: String,
    },
}

impl ImplementationChoice {
    fn auth_domain(&self) -> &str {
        match self {
            Self::Local { implementation, .. } => &implementation.auth_domain,
            Self::Custom { auth_domain, .. } => auth_domain,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Local { label, .. } | Self::Custom { label, .. } => label,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowPhase {
    SelectingImplementation,
    AwaitingCallback,
    CallbackStaged,
    Resolving,
    Committing,
    AuthorizedReady,
    Finished,
}

struct StagedCallback {
    proof: VerifiedOAuthCallback,
    payload: OAuthCallbackPayload,
    next_step: &'static str,
}

#[derive(Clone)]
enum ResolverOutcome {
    Complete(Value),
    RetryProvider,
    Failed(ResolverFailure),
}

#[derive(Clone, Copy)]
enum ResolverFailure {
    Core,
    Aborted,
}

impl FlowRecord {
    fn new(
        request: ConfigFlowRequest,
        registration_id: Uuid,
        provider: Arc<dyn OAuthFlowProvider>,
        slot: tokio::sync::OwnedSemaphorePermit,
        flow_ttl: Duration,
    ) -> Arc<Self> {
        let (completion, _) = watch::channel(None);
        Arc::new(Self {
            flow_id: request.flow_id.clone(),
            request: Mutex::new(request),
            registration_id,
            provider,
            data: Mutex::new(FlowPrivateData {
                phase: FlowPhase::SelectingImplementation,
                implementation: None,
                auth_implementation: None,
                client: None,
                session: None,
                sdk_flow_id: None,
                state_digest: None,
                callback: None,
                token: None,
            }),
            _slot: Mutex::new(Some(slot)),
            expires_at: Instant::now() + flow_ttl,
            completion,
        })
    }

    fn phase(&self) -> FlowPhase {
        lock(&self.data).phase
    }

    fn complete(&self, outcome: ResolverOutcome) {
        let mut data = lock(&self.data);
        data.phase = match &outcome {
            ResolverOutcome::Complete(_) | ResolverOutcome::Failed(_) => FlowPhase::Finished,
            ResolverOutcome::RetryProvider => FlowPhase::AuthorizedReady,
        };
        self.completion.send_replace(Some(outcome));
    }

    fn request(&self) -> ConfigFlowRequest {
        lock(&self.request).clone()
    }
}

impl fmt::Debug for FlowRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlowRecord")
            .field("flow_id", &self.flow_id)
            .field("domain", &self.request().domain)
            .field("source", &self.request().source)
            .field("entry_id", &self.request().entry_id)
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cancel_sdk_authorization(flow: &FlowRecord) {
    let pending = {
        let data = lock(&flow.data);
        data.client.clone().zip(data.sdk_flow_id.clone())
    };
    if let Some((client, flow_id)) = pending {
        client.cancel_authorization(&flow_id);
    }
}

async fn wait_for_flow_completion(flow: &FlowRecord) {
    let mut receiver = flow.completion.subscribe();
    loop {
        if receiver.borrow_and_update().is_some() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn run_flow_cleanup(entries: &ConfigEntries, mut work: FlowCleanupWork) {
    if work.phase == FlowPhase::Committing {
        wait_for_flow_completion(&work.flow).await;
    } else if work.phase != FlowPhase::Finished {
        cancel_sdk_authorization(&work.flow);
        if let Some(job) = &work.resolver_job {
            job.abort.abort();
        }
        let _ = entries.abort_flow_async(&work.flow_id).await;
    }
    if let Some(job) = work.resolver_job.take() {
        let _ = job.join.await;
    }
    clear_private_flow(&work.flow);
}

fn clear_private_flow(flow: &FlowRecord) {
    let private = {
        let mut data = lock(&flow.data);
        (
            data.callback.take(),
            data.token.take(),
            data.client.take(),
            data.session.take(),
            data.implementation.take(),
            data.auth_implementation.take(),
            data.sdk_flow_id.take(),
            data.state_digest.take(),
        )
    };
    let slot = lock(&flow._slot).take();
    drop((private, slot));
}

fn evict_provider_clients_from(
    client_cache: &Arc<Mutex<HashMap<ClientCacheKey, OAuthClient>>>,
    registration_id: Uuid,
) {
    let removed = {
        let mut clients = lock(client_cache);
        let keys = clients
            .keys()
            .filter(|key| key.registration_id == registration_id)
            .cloned()
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| clients.remove(&key))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

fn clear_client_cache(client_cache: &Arc<Mutex<HashMap<ClientCacheKey, OAuthClient>>>) {
    let removed = std::mem::take(&mut *lock(client_cache));
    drop(removed);
}

fn remove_completed_flow_cleanup(state: &mut HostState, flow_id: &str, cleanup_id: Uuid) {
    let completed = state
        .flow_cleanup_jobs
        .get(flow_id)
        .is_some_and(|cleanup| cleanup.job.id == cleanup_id && cleanup.job.is_complete());
    if completed {
        state.flow_cleanup_jobs.remove(flow_id);
    }
}

fn take_flow_cleanup_locked(
    state: &mut HostState,
    flow_id: &str,
    reason: FlowCleanupReason,
    expire_only: bool,
) -> Option<FlowCleanupStart> {
    let flow = state.flows.get(flow_id)?.clone();
    let phase = flow.phase();
    if expire_only && (flow.expires_at > Instant::now() || phase == FlowPhase::Committing) {
        return None;
    }
    let flow = state.flows.remove(flow_id)?;
    state
        .state_index
        .retain(|_, indexed_flow| indexed_flow != flow_id);
    if phase != FlowPhase::Committing && phase != FlowPhase::Finished {
        flow.complete(ResolverOutcome::Failed(ResolverFailure::Aborted));
    }
    let resolver_job = state.resolver_jobs.remove(flow_id);
    let registration_id = flow.registration_id;
    let (job, completion) = LifecycleCleanup::pending();
    state.flow_cleanup_jobs.insert(
        flow_id.to_owned(),
        FlowCleanupRegistration {
            registration_id,
            reason,
            job: Arc::clone(&job),
        },
    );
    Some(FlowCleanupStart {
        job,
        completion,
        work: FlowCleanupWork {
            flow_id: flow_id.to_owned(),
            registration_id,
            flow,
            phase,
            resolver_job,
        },
    })
}

fn launch_flow_cleanup(
    entries: ConfigEntries,
    runtime: &tokio::runtime::Handle,
    host: Weak<HostInner>,
    start: FlowCleanupStart,
) {
    let job = Arc::clone(&start.job);
    let cleanup_id = job.id;
    let completion = start.completion;
    let join = runtime.spawn(async move {
        let registration_id = start.work.registration_id;
        let flow_id = start.work.flow_id.clone();
        run_flow_cleanup(&entries, start.work).await;
        completion.send_replace(true);
        if let Some(host) = host.upgrade() {
            {
                let mut state = lock(&host.state);
                remove_completed_flow_cleanup(&mut state, &flow_id, cleanup_id);
            }
            host.start_pending_provider_cleanup(registration_id);
        }
    });
    job.retain(join);
}

impl HostInner {
    fn provider_is_loaded(&self, domain: &str) -> bool {
        lock(&self.state).providers.contains_key(domain)
    }

    async fn expire_flow_if_due(self: &Arc<Self>, flow_id: &str) -> bool {
        let (job, start) = {
            let mut state = lock(&self.state);
            if let Some(existing) = state.flow_cleanup_jobs.get(flow_id) {
                if existing.reason == FlowCleanupReason::Expired {
                    (Some(Arc::clone(&existing.job)), None)
                } else {
                    (None, None)
                }
            } else if let Some(start) =
                take_flow_cleanup_locked(&mut state, flow_id, FlowCleanupReason::Expired, true)
            {
                (Some(Arc::clone(&start.job)), Some(start))
            } else {
                (None, None)
            }
        };
        if let Some(start) = start {
            launch_flow_cleanup(
                self.entries.clone(),
                &tokio::runtime::Handle::current(),
                Arc::downgrade(self),
                start,
            );
        }
        if let Some(job) = job {
            job.wait().await;
            true
        } else {
            false
        }
    }

    async fn prune_expired(self: &Arc<Self>) {
        let expired = {
            let state = lock(&self.state);
            state
                .flows
                .iter()
                .filter(|(_, flow)| {
                    flow.expires_at <= Instant::now() && flow.phase() != FlowPhase::Committing
                })
                .map(|(flow_id, _)| flow_id.clone())
                .collect::<Vec<_>>()
        };
        for flow_id in expired {
            self.expire_flow_if_due(&flow_id).await;
        }
        self.prune_finished_jobs().await;
    }

    fn expire_flow_for_driver(&self, flow_id: &str) -> bool {
        let (flow, phase, resolver_job) = {
            let mut state = lock(&self.state);
            if let Some(existing) = state.flow_cleanup_jobs.get(flow_id) {
                return existing.reason == FlowCleanupReason::Expired;
            }
            let Some(flow) = state.flows.get(flow_id).cloned() else {
                return false;
            };
            let phase = flow.phase();
            if flow.expires_at > Instant::now() || phase == FlowPhase::Committing {
                return false;
            }
            state.flows.remove(flow_id);
            state
                .state_index
                .retain(|_, indexed_flow| indexed_flow != flow_id);
            if phase != FlowPhase::Finished {
                flow.complete(ResolverOutcome::Failed(ResolverFailure::Aborted));
            }
            (flow, phase, state.resolver_jobs.remove(flow_id))
        };
        if phase != FlowPhase::Finished {
            cancel_sdk_authorization(&flow);
            if let Some(job) = &resolver_job {
                job.abort.abort();
            }
        }
        clear_private_flow(&flow);
        drop(resolver_job);
        drop(flow);
        true
    }

    fn flow_record(
        &self,
        request: ConfigFlowRequest,
        provider: Arc<dyn OAuthFlowProvider>,
        registration_id: Uuid,
    ) -> Result<Arc<FlowRecord>, OAuthFlowError> {
        {
            let state = lock(&self.state);
            if !state.accepting {
                return Err(OAuthFlowError::ShuttingDown);
            }
            if let Some(flow) = state.flows.get(&request.flow_id) {
                if flow.registration_id != registration_id {
                    return Err(OAuthFlowError::UnknownFlow);
                }
                if flow.expires_at <= Instant::now() && flow.phase() != FlowPhase::Committing {
                    return Err(OAuthFlowError::ExpiredFlow);
                }
                *lock(&flow.request) = request;
                return Ok(Arc::clone(flow));
            }
        }

        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| OAuthFlowError::TooManyFlows)?;
        let flow = FlowRecord::new(
            request.clone(),
            registration_id,
            provider,
            slot,
            self.options.flow_ttl,
        );
        if flow.expires_at <= Instant::now() {
            return Err(OAuthFlowError::ExpiredFlow);
        }
        let mut state = lock(&self.state);
        if !state.accepting {
            return Err(OAuthFlowError::ShuttingDown);
        }
        if let Some(existing) = state.flows.get(&request.flow_id) {
            if existing.registration_id != registration_id {
                return Err(OAuthFlowError::UnknownFlow);
            }
            if existing.expires_at <= Instant::now() && existing.phase() != FlowPhase::Committing {
                return Err(OAuthFlowError::ExpiredFlow);
            }
            *lock(&existing.request) = request;
            return Ok(Arc::clone(existing));
        }
        if !state
            .providers
            .get(&request.domain)
            .is_some_and(|registered| registered.id == registration_id)
        {
            return Err(OAuthFlowError::OAuthImplementationUnavailable);
        }
        state.flows.insert(request.flow_id, Arc::clone(&flow));
        Ok(flow)
    }

    async fn process_config_flow(
        &self,
        provider: Arc<dyn OAuthFlowProvider>,
        registration_id: Uuid,
        request: ConfigFlowRequest,
    ) -> hass_core::Result<ConfigFlowResult> {
        if !self.registration_is_current(&request.domain, registration_id) {
            return Ok(ConfigFlowResult::Abort {
                reason: "oauth_implementation_unavailable".into(),
            });
        }
        if self.expire_flow_for_driver(&request.flow_id) {
            return Ok(ConfigFlowResult::Abort {
                reason: "oauth_timeout".into(),
            });
        }
        let flow = match self.flow_record(request.clone(), Arc::clone(&provider), registration_id) {
            Ok(flow) => flow,
            Err(OAuthFlowError::TooManyFlows) => {
                return Ok(ConfigFlowResult::Abort {
                    reason: "oauth_failed".into(),
                });
            }
            Err(OAuthFlowError::ExpiredFlow) => {
                self.expire_flow_for_driver(&request.flow_id);
                return Ok(ConfigFlowResult::Abort {
                    reason: "oauth_timeout".into(),
                });
            }
            Err(_) => {
                return Ok(ConfigFlowResult::Abort {
                    reason: "oauth_implementation_unavailable".into(),
                });
            }
        };

        let implementations = match self
            .app_credentials
            .provide_implementations(&request.domain)
            .await
        {
            Ok(implementations) => implementations,
            Err(_) => {
                self.remove_private_flow(&request.flow_id);
                return Ok(ConfigFlowResult::Abort {
                    reason: "oauth_implementation_unavailable".into(),
                });
            }
        };
        let choices = implementations
            .into_iter()
            .map(|implementation| match implementation {
                ResolvedOAuthImplementation::Local(implementation) => ImplementationChoice::Local {
                    label: implementation.name.clone(),
                    implementation,
                },
                ResolvedOAuthImplementation::Custom {
                    auth_domain,
                    implementation,
                } => ImplementationChoice::Custom {
                    label: implementation
                        .name()
                        .map(str::to_owned)
                        .unwrap_or_else(|| auth_domain.clone()),
                    auth_domain,
                },
            })
            .collect::<Vec<_>>();
        if self.expire_flow_for_driver(&request.flow_id) {
            return Ok(ConfigFlowResult::Abort {
                reason: "oauth_timeout".into(),
            });
        }
        if choices.is_empty() {
            self.remove_private_flow(&request.flow_id);
            return Ok(ConfigFlowResult::Abort {
                reason: "missing_credentials".into(),
            });
        }
        let reauth_selection = if request.source == "reauth" {
            request
                .entry_id
                .as_deref()
                .and_then(|entry_id| self.entries.get_entry(entry_id))
                .and_then(|entry| {
                    entry
                        .data
                        .get(AUTH_IMPLEMENTATION_FIELD)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        } else {
            None
        };
        let submitted_selection = request
            .user_input
            .as_ref()
            .and_then(|input| input.get(AUTH_IMPLEMENTATION_FIELD))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let selected = if request.step_id == IMPLEMENTATION_STEP_ID {
            submitted_selection
                .as_deref()
                .and_then(|value| choices.iter().find(|choice| choice.auth_domain() == value))
        } else if request.source == "reauth" {
            reauth_selection
                .as_deref()
                .and_then(|value| choices.iter().find(|choice| choice.auth_domain() == value))
                .or_else(|| {
                    reauth_selection
                        .is_none()
                        .then(|| choices.first())
                        .flatten()
                })
        } else if let Some(selection) = submitted_selection.as_deref() {
            choices
                .iter()
                .find(|choice| choice.auth_domain() == selection)
        } else if choices.len() == 1 {
            choices.first()
        } else {
            None
        };

        if let Some(choice) = selected.cloned() {
            return match self.start_authorization(&flow, provider, choice).await {
                Ok(result) => Ok(result),
                Err(OAuthFlowError::ExpiredFlow) => {
                    self.expire_flow_for_driver(&request.flow_id);
                    Ok(ConfigFlowResult::Abort {
                        reason: "oauth_timeout".into(),
                    })
                }
                Err(OAuthFlowError::UnsupportedCustomImplementation) => {
                    self.remove_private_flow(&request.flow_id);
                    Ok(ConfigFlowResult::Abort {
                        reason: "oauth_implementation_unavailable".into(),
                    })
                }
                Err(_) => {
                    self.remove_private_flow(&request.flow_id);
                    Ok(ConfigFlowResult::Abort {
                        reason: "no_url_available".into(),
                    })
                }
            };
        }

        if request.source == "reauth"
            && reauth_selection.is_some()
            && !choices
                .iter()
                .any(|choice| Some(choice.auth_domain()) == reauth_selection.as_deref())
        {
            self.remove_private_flow(&request.flow_id);
            return Ok(ConfigFlowResult::Abort {
                reason: "oauth_implementation_unavailable".into(),
            });
        }
        let errors = if request.step_id == IMPLEMENTATION_STEP_ID {
            let mut errors = Map::new();
            errors.insert(
                AUTH_IMPLEMENTATION_FIELD.to_owned(),
                json!("invalid_auth_implementation"),
            );
            Some(errors)
        } else {
            None
        };
        if self.expire_flow_for_driver(&request.flow_id) {
            return Ok(ConfigFlowResult::Abort {
                reason: "oauth_timeout".into(),
            });
        }
        Ok(implementation_form(&choices, errors))
    }

    async fn start_authorization(
        &self,
        flow: &Arc<FlowRecord>,
        provider: Arc<dyn OAuthFlowProvider>,
        choice: ImplementationChoice,
    ) -> Result<ConfigFlowResult, OAuthFlowError> {
        let ImplementationChoice::Local { implementation, .. } = choice else {
            return Err(OAuthFlowError::UnsupportedCustomImplementation);
        };
        let request = flow.request();
        let client = self.client_for_implementation(
            flow.registration_id,
            &request.domain,
            Arc::clone(&provider),
            &implementation,
        )?;
        let account_id = if request.source == "reauth" {
            request
                .entry_id
                .as_deref()
                .ok_or(OAuthFlowError::EntryUnavailable)?
                .to_owned()
        } else {
            request.flow_id.clone()
        };
        let session = client.session(account_id)?;
        let scopes = provider.oauth_scopes(&request);
        let step = session
            .start_authorization(
                AuthorizationRequest::new(self.options.callback_url.clone()).with_scopes(scopes),
            )
            .await?;
        let state = step
            .authorization_url()
            .query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned())
            .ok_or(OAuthFlowError::InvalidConfiguration)?;
        let digest = state_digest(&state);
        let admission = {
            let mut state_map = lock(&self.state);
            if !state_map.accepting
                || !state_map
                    .providers
                    .get(&request.domain)
                    .is_some_and(|registered| registered.id == flow.registration_id)
            {
                Err(OAuthFlowError::ShuttingDown)
            } else if flow.expires_at <= Instant::now() && flow.phase() != FlowPhase::Committing {
                Err(OAuthFlowError::ExpiredFlow)
            } else if state_map.state_index.contains_key(&digest) {
                Err(OAuthFlowError::InvalidConfiguration)
            } else {
                state_map
                    .state_index
                    .insert(digest.clone(), request.flow_id.clone());
                // Publish the host's verifier identity under the same lock
                // used by unregister, TTL pruning, and shutdown.
                let mut data = lock(&flow.data);
                data.phase = FlowPhase::AwaitingCallback;
                data.auth_implementation = Some(implementation.auth_domain.clone());
                data.implementation = Some(implementation.clone());
                data.client = Some(client.clone());
                data.session = Some(session.clone());
                data.sdk_flow_id = Some(step.flow_id().clone());
                data.state_digest = Some(digest);
                Ok(())
            }
        };
        if let Err(error) = admission {
            client.cancel_authorization(step.flow_id());
            return Err(error);
        }
        Ok(ConfigFlowResult::ExternalStep {
            step_id: AUTH_STEP_ID.to_owned(),
            url: step.authorization_url().as_str().to_owned(),
            description_placeholders: None,
        })
    }

    fn client_for_implementation(
        &self,
        registration_id: Uuid,
        domain: &str,
        provider: Arc<dyn OAuthFlowProvider>,
        implementation: &LocalOAuth2Implementation,
    ) -> Result<OAuthClient, OAuthFlowError> {
        let key = ClientCacheKey {
            registration_id,
            domain: domain.to_owned(),
            auth_domain: implementation.auth_domain.clone(),
            client_id: implementation.client_id.clone(),
        };
        if let Some(client) = lock(&self.client_cache).get(&key).cloned() {
            return Ok(client);
        }
        let config = provider.oauth_client_config(implementation)?;
        let created = self.factory.client(config)?;
        let mut clients = lock(&self.client_cache);
        Ok(clients.entry(key).or_insert(created).clone())
    }

    async fn session_for_entry(
        &self,
        domain: &str,
        entry_id: &str,
    ) -> Result<OAuthSession, OAuthFlowError> {
        self.ensure_accepting()?;
        let entry = self
            .entries
            .get_entry(entry_id)
            .ok_or(OAuthFlowError::EntryUnavailable)?;
        if entry.domain != domain {
            return Err(OAuthFlowError::EntryUnavailable);
        }
        let selected = entry
            .data
            .get(AUTH_IMPLEMENTATION_FIELD)
            .and_then(Value::as_str)
            .ok_or(OAuthFlowError::OAuthImplementationUnavailable)?;
        let registration = {
            let state = lock(&self.state);
            state
                .providers
                .get(domain)
                .map(|registration| (registration.id, Arc::clone(&registration.provider)))
                .ok_or(OAuthFlowError::OAuthImplementationUnavailable)?
        };
        let implementations = self.app_credentials.provide_implementations(domain).await?;
        let implementation = implementations
            .into_iter()
            .find_map(|candidate| match candidate {
                ResolvedOAuthImplementation::Local(local) if local.auth_domain == selected => {
                    Some(Ok(local))
                }
                ResolvedOAuthImplementation::Custom { auth_domain, .. }
                    if auth_domain == selected =>
                {
                    Some(Err(OAuthFlowError::UnsupportedCustomImplementation))
                }
                _ => None,
            })
            .transpose()?
            .ok_or(OAuthFlowError::OAuthImplementationUnavailable)?;
        let client = self.client_for_implementation(
            registration.0,
            domain,
            registration.1,
            &implementation,
        )?;
        Ok(client.session(entry_id)?)
    }

    async fn accept_callback(
        self: &Arc<Self>,
        params: OAuthCallbackParams,
    ) -> Result<OAuthCallbackResponse, OAuthFlowError> {
        validate_callback_params(&params)?;
        let digest = state_digest(&params.state);
        let (flow_id, flow) = {
            let state = lock(&self.state);
            let flow_id = state
                .state_index
                .get(&digest)
                .cloned()
                .ok_or(OAuthFlowError::UnknownFlow)?;
            let flow = state
                .flows
                .get(&flow_id)
                .cloned()
                .ok_or(OAuthFlowError::UnknownFlow)?;
            (flow_id, flow)
        };
        if self.expire_flow_if_due(&flow_id).await {
            return Err(OAuthFlowError::ExpiredFlow);
        }
        let (client, expected_sdk_flow, phase, stored_digest) = {
            let data = lock(&flow.data);
            (
                data.client.clone().ok_or(OAuthFlowError::UnknownFlow)?,
                data.sdk_flow_id
                    .clone()
                    .ok_or(OAuthFlowError::UnknownFlow)?,
                data.phase,
                data.state_digest
                    .clone()
                    .ok_or(OAuthFlowError::UnknownFlow)?,
            )
        };
        if phase != FlowPhase::AwaitingCallback || stored_digest != digest {
            return Err(OAuthFlowError::ReplayedCallback);
        }
        let proof = client.verify_callback_state(&params.state).await?;
        if proof.flow_id().as_uuid_string() != expected_sdk_flow.as_uuid_string() {
            return Err(OAuthFlowError::InvalidCallback);
        }
        if self.expire_flow_if_due(&flow_id).await {
            return Err(OAuthFlowError::ExpiredFlow);
        }
        let request = flow.request();
        let lease = self
            .entries
            .claim_external_callback(&flow_id, &request.domain, AUTH_STEP_ID)
            .map_err(map_callback_claim_error)?;
        let code_present = params
            .code
            .as_deref()
            .is_some_and(|code| !code.trim().is_empty());
        let next_step = if code_present {
            CREATION_STEP_ID
        } else {
            REJECTED_STEP_ID
        };
        let stage_error = {
            let state = lock(&self.state);
            let is_current = state
                .flows
                .get(&flow_id)
                .is_some_and(|current| Arc::ptr_eq(current, &flow));
            if !is_current {
                Some(OAuthFlowError::UnknownFlow)
            } else if flow.expires_at <= Instant::now() && flow.phase() != FlowPhase::Committing {
                Some(OAuthFlowError::ExpiredFlow)
            } else {
                let mut data = lock(&flow.data);
                if data.phase != FlowPhase::AwaitingCallback {
                    Some(OAuthFlowError::ReplayedCallback)
                } else {
                    data.callback = Some(StagedCallback {
                        proof,
                        payload: OAuthCallbackPayload::new(params.code, params.error),
                        next_step,
                    });
                    data.phase = FlowPhase::CallbackStaged;
                    None
                }
            }
        };
        if let Some(error) = stage_error {
            let _ = self.entries.release_external_flow(lease);
            if matches!(error, OAuthFlowError::ExpiredFlow) {
                self.expire_flow_if_due(&flow_id).await;
            }
            return Err(error);
        }
        match self.entries.publish_external_done(lease.clone(), next_step) {
            Ok(_) => Ok(OAuthCallbackResponse),
            Err(error) => {
                let _ = self.entries.release_external_flow(lease);
                let mut data = lock(&flow.data);
                data.callback = None;
                data.phase = FlowPhase::AwaitingCallback;
                Err(OAuthFlowError::Core(error))
            }
        }
    }

    async fn get_flow(self: &Arc<Self>, flow_id: &str) -> Result<Value, OAuthFlowError> {
        if self.expire_flow_if_due(flow_id).await {
            return Err(OAuthFlowError::ExpiredFlow);
        }
        self.prune_finished_jobs().await;
        let flow = lock(&self.state).flows.get(flow_id).cloned();
        let Some(flow) = flow else {
            return self
                .entries
                .refresh_flow_async(flow_id)
                .await
                .map_err(Into::into);
        };
        match flow.phase() {
            FlowPhase::Finished => {
                let outcome = flow.completion.borrow().clone();
                let result = outcome_to_result(outcome.ok_or(OAuthFlowError::ProviderFailure)?)?;
                self.remove_private_flow(flow_id);
                Ok(result)
            }
            FlowPhase::Resolving | FlowPhase::Committing => self.wait_for_resolver(&flow).await,
            FlowPhase::SelectingImplementation | FlowPhase::AwaitingCallback => self
                .entries
                .refresh_flow_async(flow_id)
                .await
                .map_err(Into::into),
            FlowPhase::CallbackStaged | FlowPhase::AuthorizedReady => {
                let snapshot = self
                    .entries
                    .refresh_flow_async(flow_id)
                    .await
                    .map_err(OAuthFlowError::Core)?;
                if snapshot.get("type").and_then(Value::as_str) != Some("external_done") {
                    return Ok(snapshot);
                }
                self.advance_external_done(flow, snapshot).await
            }
        }
    }

    async fn advance_external_done(
        self: &Arc<Self>,
        flow: Arc<FlowRecord>,
        snapshot: Value,
    ) -> Result<Value, OAuthFlowError> {
        let flow_id = flow.flow_id.clone();
        let (domain, expected_step) = {
            let data = lock(&flow.data);
            let expected_step = match data.phase {
                FlowPhase::CallbackStaged => data
                    .callback
                    .as_ref()
                    .map(|callback| callback.next_step)
                    .ok_or(OAuthFlowError::InvalidCallback)?,
                FlowPhase::AuthorizedReady => CREATION_STEP_ID,
                _ => return Ok(snapshot),
            };
            (flow.request().domain, expected_step)
        };
        let lease = match self
            .entries
            .claim_external_done(&flow_id, &domain, expected_step)
        {
            Ok(Some(lease)) => lease,
            Ok(None) => return self.wait_for_resolver(&flow).await,
            Err(error) => {
                if flow.phase() == FlowPhase::Resolving {
                    return self.wait_for_resolver(&flow).await;
                }
                return Err(OAuthFlowError::Core(error));
            }
        };
        let work = {
            let mut data = lock(&flow.data);
            let work = match data.phase {
                FlowPhase::CallbackStaged => data.callback.take().map(ResolverWork::Callback),
                FlowPhase::AuthorizedReady => data.token.clone().map(ResolverWork::AuthorizedReady),
                _ => None,
            };
            if work.is_some() {
                data.phase = FlowPhase::Resolving;
                flow.completion.send_replace(None);
            }
            work
        };
        let Some(work) = work else {
            let _ = self.entries.release_external_flow(lease);
            return self.wait_for_resolver(&flow).await;
        };
        self.spawn_resolver(Arc::clone(&flow), lease, work)?;
        self.wait_for_resolver(&flow).await
    }

    fn spawn_resolver(
        self: &Arc<Self>,
        flow: Arc<FlowRecord>,
        lease: ExternalFlowLease,
        work: ResolverWork,
    ) -> Result<(), OAuthFlowError> {
        let flow_id = flow.flow_id.clone();
        let entries = self.entries.clone();
        let timeout = self.options.resolver_timeout;
        let host = Arc::downgrade(self);
        let commit_lifetime = Arc::new(Mutex::new(None));
        let mut state = lock(&self.state);
        if !state.accepting {
            let _ = self.entries.release_external_flow(lease);
            return Err(OAuthFlowError::ShuttingDown);
        }
        if state.resolver_jobs.contains_key(&flow_id) {
            let _ = self.entries.release_external_flow(lease);
            return Ok(());
        }
        let recovery_lease = lease.clone();
        let task_flow = Arc::clone(&flow);
        let resolver_lifetime = Arc::clone(&commit_lifetime);
        let host_for_cleanup = host.clone();
        let handle = tokio::spawn(async move {
            let resolver = resolve_external_flow(
                host,
                resolver_lifetime,
                entries.clone(),
                Arc::clone(&task_flow),
                lease,
                work,
                timeout,
            );
            let outcome = match std::panic::AssertUnwindSafe(resolver).catch_unwind().await {
                Ok(outcome) => outcome,
                Err(_) => recover_resolver_panic(&entries, &task_flow, recovery_lease),
            };
            task_flow.complete(outcome);
            let host = lock(&commit_lifetime)
                .take()
                .or_else(|| host_for_cleanup.upgrade());
            if let Some(host) = host {
                host.start_pending_provider_cleanup(task_flow.registration_id);
            }
        });
        let abort = handle.abort_handle();
        state.resolver_jobs.insert(
            flow_id,
            ResolverJob {
                abort,
                join: handle,
            },
        );
        Ok(())
    }

    async fn wait_for_resolver(&self, flow: &Arc<FlowRecord>) -> Result<Value, OAuthFlowError> {
        let mut receiver = flow.completion.subscribe();
        loop {
            if let Some(outcome) = receiver.borrow().clone() {
                match outcome_to_result(outcome) {
                    Ok(value) => {
                        if flow.phase() == FlowPhase::Finished {
                            self.remove_private_flow(&flow.flow_id);
                        }
                        return Ok(value);
                    }
                    Err(error) => {
                        if !matches!(error, OAuthFlowError::ProviderFailure) {
                            self.remove_private_flow(&flow.flow_id);
                        }
                        return Err(error);
                    }
                }
            }
            receiver
                .changed()
                .await
                .map_err(|_| OAuthFlowError::ProviderFailure)?;
        }
    }

    async fn wait_for_committing_resolver(&self, flow_id: &str, flow: &Arc<FlowRecord>) {
        let mut receiver = flow.completion.subscribe();
        while receiver.borrow().is_none() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
        loop {
            let finished = lock(&self.state)
                .resolver_jobs
                .get(flow_id)
                .is_none_or(|job| job.join.is_finished());
            if finished {
                let job = {
                    let mut state = lock(&self.state);
                    state
                        .resolver_jobs
                        .get(flow_id)
                        .is_some_and(|job| job.join.is_finished())
                        .then(|| state.resolver_jobs.remove(flow_id))
                        .flatten()
                };
                if let Some(job) = job {
                    let _ = job.join.await;
                }
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn remove_private_flow(&self, flow_id: &str) {
        let (flow, finished_job) = {
            let mut state = lock(&self.state);
            let flow = state.flows.remove(flow_id);
            state
                .state_index
                .retain(|_, indexed_flow| indexed_flow != flow_id);
            let finished_job = state
                .resolver_jobs
                .get(flow_id)
                .is_some_and(|job| job.join.is_finished())
                .then(|| state.resolver_jobs.remove(flow_id))
                .flatten();
            (flow, finished_job)
        };
        if let Some(flow) = &flow {
            cancel_sdk_authorization(flow);
            let mut data = lock(&flow.data);
            data.callback = None;
            data.token = None;
            data.client = None;
            data.session = None;
            data.implementation = None;
            data.auth_implementation = None;
            data.sdk_flow_id = None;
            data.state_digest = None;
            drop(data);
            lock(&flow._slot).take();
        }
        drop(flow);
        drop(finished_job);
    }

    async fn cancel_flow(&self, flow_id: &str) -> Result<(), OAuthFlowError> {
        let (flow, job, committing, finished) = {
            let mut state = lock(&self.state);
            let current = state.flows.get(flow_id).cloned();
            let phase = current.as_ref().map(|flow| flow.phase());
            let committing = phase == Some(FlowPhase::Committing);
            let finished = phase == Some(FlowPhase::Finished);
            if committing {
                (current, None, true, false)
            } else if finished {
                (current, None, false, true)
            } else {
                let flow = state.flows.remove(flow_id);
                state
                    .state_index
                    .retain(|_, indexed_flow| indexed_flow != flow_id);
                if let Some(flow) = &flow {
                    flow.complete(ResolverOutcome::Failed(ResolverFailure::Aborted));
                }
                let job = state.resolver_jobs.remove(flow_id);
                (flow, job, false, false)
            }
        };
        if committing {
            if let Some(flow) = &flow {
                self.wait_for_committing_resolver(flow_id, flow).await;
            }
            return Ok(());
        }
        if finished {
            return Ok(());
        }
        if let Some(flow) = &flow {
            cancel_sdk_authorization(flow);
        }
        if let Some(job) = &job {
            job.abort.abort();
        }
        let abort_result = self.entries.abort_flow_async(flow_id).await;
        if let Some(job) = job {
            let _ = job.join.await;
        }
        drop(flow);
        abort_result.map_err(Into::into)
    }

    async fn prune_finished_jobs(&self) {
        let jobs = {
            let mut state = lock(&self.state);
            let finished = state
                .resolver_jobs
                .iter()
                .filter(|(_, job)| job.join.is_finished())
                .map(|(flow_id, _)| flow_id.clone())
                .collect::<Vec<_>>();
            finished
                .into_iter()
                .filter_map(|flow_id| state.resolver_jobs.remove(&flow_id))
                .collect::<Vec<_>>()
        };
        for job in jobs {
            let _ = job.join.await;
        }
    }

    fn register_provider(
        self: &Arc<Self>,
        provider: Arc<dyn OAuthFlowProvider>,
    ) -> Result<OAuthFlowRegistrationGuard, OAuthFlowError> {
        let domain = provider.domain().to_owned();
        if !valid_domain(&domain) {
            return Err(OAuthFlowError::InvalidConfiguration);
        }
        {
            let mut state = lock(&self.state);
            state
                .flow_cleanup_jobs
                .retain(|_, cleanup| !cleanup.job.is_complete());
            state
                .provider_cleanup_jobs
                .retain(|_, cleanup| !cleanup.is_complete());
            if !state.accepting {
                return Err(OAuthFlowError::ShuttingDown);
            }
            if state.providers.contains_key(&domain) {
                return Err(OAuthFlowError::InvalidConfiguration);
            }
        }

        let registration_id = Uuid::new_v4();
        let mut platform_guard = Some(
            self.platform_registry
                .register(&domain, provider.application_credentials_platform())?,
        );
        let wrapper: Arc<dyn ConfigEntryDriver> = Arc::new(OAuthFlowDriver {
            host: Arc::downgrade(self),
            provider: Arc::clone(&provider),
            registration_id,
        });
        if let Err(error) = self.entries.register_driver(Arc::clone(&wrapper)) {
            drop(platform_guard);
            return Err(OAuthFlowError::Core(error));
        }

        let inserted = {
            let mut state = lock(&self.state);
            if !state.accepting || state.providers.contains_key(&domain) {
                false
            } else {
                state.providers.insert(
                    domain.clone(),
                    ProviderRegistration {
                        id: registration_id,
                        provider: Arc::clone(&provider),
                        driver: Arc::clone(&wrapper),
                        _platform_guard: platform_guard
                            .take()
                            .expect("platform guard is present before registration"),
                    },
                );
                true
            }
        };
        if !inserted {
            self.entries.unregister_driver(&wrapper);
            drop(platform_guard);
            return Err(if lock(&self.state).accepting {
                OAuthFlowError::InvalidConfiguration
            } else {
                OAuthFlowError::ShuttingDown
            });
        }
        Ok(OAuthFlowRegistrationGuard {
            inner: Arc::downgrade(self),
            domain,
            registration_id,
            active: true,
        })
    }

    fn begin_provider_cleanup(
        self: &Arc<Self>,
        domain: &str,
        registration_id: Uuid,
    ) -> Arc<LifecycleCleanup> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.unregister_provider_sync(domain, registration_id);
            return LifecycleCleanup::completed();
        };
        self.begin_provider_cleanup_with_runtime(domain, registration_id, runtime)
    }

    fn unregister_provider_sync(&self, domain: &str, registration_id: Uuid) {
        let (registration, flows) = {
            let mut state = lock(&self.state);
            if state.shutdown_cleanup_job.is_some()
                || state.provider_cleanup_jobs.contains_key(&registration_id)
            {
                return;
            }
            if !state
                .providers
                .get(domain)
                .is_some_and(|registered| registered.id == registration_id)
            {
                return;
            }
            if state.flow_cleanup_jobs.values().any(|cleanup| {
                cleanup.registration_id == registration_id && !cleanup.job.is_complete()
            }) {
                // An async expiry cleanup already owns these Core flow
                // obligations. Keep the registration reachable until it can
                // transfer this cleanup to a Tokio-owned task.
                state
                    .pending_provider_cleanup
                    .insert(registration_id, domain.to_owned());
                return;
            }
            let has_commit = state.flows.values().any(|flow| {
                flow.registration_id == registration_id && flow.phase() == FlowPhase::Committing
            });
            if has_commit {
                // The resolver will transfer this registration to an owned
                // cleanup task after the durable commit completes.
                state
                    .pending_provider_cleanup
                    .insert(registration_id, domain.to_owned());
                return;
            }
            let registration = state.providers.remove(domain);
            let flow_ids = state
                .flows
                .iter()
                .filter(|(_, flow)| flow.registration_id == registration_id)
                .map(|(flow_id, _)| flow_id.clone())
                .collect::<Vec<_>>();
            let flows = flow_ids
                .into_iter()
                .filter_map(|flow_id| {
                    let flow = state.flows.remove(&flow_id)?;
                    state
                        .state_index
                        .retain(|_, indexed_flow| indexed_flow != &flow_id);
                    let phase = flow.phase();
                    if phase != FlowPhase::Finished {
                        flow.complete(ResolverOutcome::Failed(ResolverFailure::Aborted));
                    }
                    Some((
                        flow_id.clone(),
                        flow,
                        phase,
                        state.resolver_jobs.remove(&flow_id),
                    ))
                })
                .collect::<Vec<_>>();
            (registration, flows)
        };
        for (flow_id, flow, phase, resolver_job) in flows {
            if phase != FlowPhase::Finished {
                cancel_sdk_authorization(&flow);
                if let Some(job) = &resolver_job {
                    job.abort.abort();
                }
                let _ = self.entries.abort_flow(&flow_id);
            }
            clear_private_flow(&flow);
            drop(resolver_job);
            drop(flow);
        }
        if let Some(registration) = registration {
            self.entries.unregister_driver(&registration.driver);
            evict_provider_clients_from(&self.client_cache, registration_id);
            drop(registration);
        }
    }

    fn start_pending_provider_cleanup(self: &Arc<Self>, registration_id: Uuid) {
        let domain = lock(&self.state)
            .pending_provider_cleanup
            .remove(&registration_id);
        if let Some(domain) = domain {
            let cleanup = self.begin_provider_cleanup(&domain, registration_id);
            drop(cleanup);
        }
    }

    fn unregister_provider_fallback(self: &Arc<Self>, domain: &str, registration_id: Uuid) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let cleanup =
                self.begin_provider_cleanup_with_runtime(domain, registration_id, runtime);
            drop(cleanup);
        } else {
            self.unregister_provider_sync(domain, registration_id);
        }
    }

    fn begin_provider_cleanup_with_runtime(
        self: &Arc<Self>,
        domain: &str,
        registration_id: Uuid,
        runtime: tokio::runtime::Handle,
    ) -> Arc<LifecycleCleanup> {
        let (cleanup, completion, registration, dependencies, starts) = {
            let mut state = lock(&self.state);
            if let Some(cleanup) = &state.shutdown_cleanup_job {
                return Arc::clone(cleanup);
            }
            if let Some(cleanup) = state.provider_cleanup_jobs.get(&registration_id) {
                return Arc::clone(cleanup);
            }
            let matches = state
                .providers
                .get(domain)
                .is_some_and(|registered| registered.id == registration_id);
            if !matches {
                return LifecycleCleanup::completed();
            }
            let registration = state
                .providers
                .remove(domain)
                .expect("matching provider registration must be present");
            let flow_ids = state
                .flows
                .iter()
                .filter(|(_, flow)| flow.registration_id == registration_id)
                .map(|(flow_id, _)| flow_id.clone())
                .collect::<Vec<_>>();
            let mut starts = Vec::new();
            for flow_id in flow_ids {
                if state.flow_cleanup_jobs.contains_key(&flow_id) {
                    continue;
                }
                if let Some(start) = take_flow_cleanup_locked(
                    &mut state,
                    &flow_id,
                    FlowCleanupReason::Unregistered,
                    false,
                ) {
                    starts.push(start);
                }
            }
            let dependencies = state
                .flow_cleanup_jobs
                .values()
                .filter(|cleanup| cleanup.registration_id == registration_id)
                .map(|cleanup| Arc::clone(&cleanup.job))
                .collect::<Vec<_>>();
            let (cleanup, completion) = LifecycleCleanup::pending();
            state
                .provider_cleanup_jobs
                .insert(registration_id, Arc::clone(&cleanup));
            (cleanup, completion, registration, dependencies, starts)
        };
        for start in starts {
            launch_flow_cleanup(self.entries.clone(), &runtime, Arc::downgrade(self), start);
        }
        let entries = self.entries.clone();
        let client_cache = Arc::clone(&self.client_cache);
        let cleanup_task = runtime.spawn(async move {
            for dependency in dependencies {
                dependency.wait().await;
            }
            entries.unregister_driver(&registration.driver);
            evict_provider_clients_from(&client_cache, registration_id);
            drop(registration);
            completion.send_replace(true);
        });
        cleanup.retain(cleanup_task);
        cleanup
    }

    async fn shutdown(self: &Arc<Self>) {
        let runtime = tokio::runtime::Handle::current();
        let cleanup = {
            let mut state = lock(&self.state);
            if let Some(cleanup) = &state.shutdown_cleanup_job {
                Arc::clone(cleanup)
            } else {
                state.accepting = false;
                let providers = std::mem::take(&mut state.providers)
                    .into_values()
                    .collect::<Vec<_>>();
                let flow_ids = state.flows.keys().cloned().collect::<Vec<_>>();
                let mut starts = Vec::new();
                for flow_id in flow_ids {
                    if state.flow_cleanup_jobs.contains_key(&flow_id) {
                        continue;
                    }
                    if let Some(start) = take_flow_cleanup_locked(
                        &mut state,
                        &flow_id,
                        FlowCleanupReason::Shutdown,
                        false,
                    ) {
                        starts.push(start);
                    }
                }
                state.state_index.clear();
                let flow_dependencies = state
                    .flow_cleanup_jobs
                    .values()
                    .map(|cleanup| Arc::clone(&cleanup.job))
                    .collect::<Vec<_>>();
                let provider_dependencies = state
                    .provider_cleanup_jobs
                    .values()
                    .map(Arc::clone)
                    .collect::<Vec<_>>();
                let (cleanup, completion) = LifecycleCleanup::pending();
                for provider in &providers {
                    state
                        .provider_cleanup_jobs
                        .insert(provider.id, Arc::clone(&cleanup));
                }
                state.shutdown_cleanup_job = Some(Arc::clone(&cleanup));
                drop(state);
                for start in starts {
                    launch_flow_cleanup(
                        self.entries.clone(),
                        &runtime,
                        Arc::downgrade(self),
                        start,
                    );
                }
                let entries = self.entries.clone();
                let client_cache = Arc::clone(&self.client_cache);
                let cleanup_task = runtime.spawn(async move {
                    for dependency in provider_dependencies {
                        dependency.wait().await;
                    }
                    for dependency in flow_dependencies {
                        dependency.wait().await;
                    }
                    for provider in providers {
                        entries.unregister_driver(&provider.driver);
                        evict_provider_clients_from(&client_cache, provider.id);
                        drop(provider);
                    }
                    clear_client_cache(&client_cache);
                    completion.send_replace(true);
                });
                cleanup.retain(cleanup_task);
                cleanup
            }
        };
        cleanup.wait().await;
    }

    fn ensure_accepting(&self) -> Result<(), OAuthFlowError> {
        if lock(&self.state).accepting {
            Ok(())
        } else {
            Err(OAuthFlowError::ShuttingDown)
        }
    }

    fn admit_commit(&self, flow: &Arc<FlowRecord>) -> bool {
        let state = lock(&self.state);
        if !state.accepting
            || !state
                .providers
                .get(flow.provider.domain())
                .is_some_and(|provider| provider.id == flow.registration_id)
            || !state
                .flows
                .get(&flow.flow_id)
                .is_some_and(|current| Arc::ptr_eq(current, flow))
        {
            return false;
        }
        let mut data = lock(&flow.data);
        if data.phase != FlowPhase::Resolving {
            return false;
        }
        // Abort, unregister, TTL cleanup, and shutdown all take the host
        // state lock before inspecting this phase. Once set, the resolver is
        // the durable-commit owner and must run deferred activation.
        data.phase = FlowPhase::Committing;
        true
    }

    fn registration_is_current(&self, domain: &str, registration_id: Uuid) -> bool {
        lock(&self.state)
            .providers
            .get(domain)
            .is_some_and(|registered| registered.id == registration_id)
    }
}

impl Drop for HostInner {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.accepting = false;
        let flow_ids = state
            .flows
            .iter()
            .filter(|(_, flow)| flow.phase() != FlowPhase::Finished)
            .map(|(flow_id, _)| flow_id.clone())
            .collect::<Vec<_>>();
        for flow in state.flows.values() {
            if flow.phase() != FlowPhase::Finished {
                flow.complete(ResolverOutcome::Failed(ResolverFailure::Aborted));
                cancel_sdk_authorization(flow);
            }
        }
        for (flow_id, job) in &state.resolver_jobs {
            if state
                .flows
                .get(flow_id)
                .is_none_or(|flow| flow.phase() != FlowPhase::Finished)
            {
                job.abort.abort();
            }
        }
        let providers = std::mem::take(&mut state.providers);
        let jobs = std::mem::take(&mut state.resolver_jobs);
        let flows = std::mem::take(&mut state.flows);
        state.state_index.clear();
        for flow_id in &flow_ids {
            let _ = self.entries.abort_flow(flow_id);
        }
        for provider in providers.into_values() {
            self.entries.unregister_driver(&provider.driver);
            evict_provider_clients_from(&self.client_cache, provider.id);
            drop(provider);
        }
        clear_client_cache(&self.client_cache);
        drop((jobs, flows));
    }
}

impl OAuthFlowManager {
    /// Start one provider's initial config flow.
    pub async fn begin(
        &self,
        domain: &str,
        source: &str,
        context: Map<String, Value>,
    ) -> Result<Value, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.ensure_accepting()?;
        if !inner.provider_is_loaded(domain) {
            return Err(OAuthFlowError::OAuthImplementationUnavailable);
        }
        inner.prune_expired().await;
        inner
            .entries
            .init_config_flow_async(domain, source, context)
            .await
            .map_err(Into::into)
    }

    /// Begin reauthentication for one existing entry.
    pub async fn begin_reauth(&self, entry_id: &str) -> Result<Value, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.ensure_accepting()?;
        inner.prune_expired().await;
        inner
            .entries
            .begin_reauth_flow_async(entry_id)
            .await
            .map_err(Into::into)
    }

    /// Submit ordinary user input to an active config flow.
    pub async fn submit(
        &self,
        flow_id: &str,
        user_input: Map<String, Value>,
    ) -> Result<Value, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.ensure_accepting()?;
        if inner.expire_flow_if_due(flow_id).await {
            return Err(OAuthFlowError::ExpiredFlow);
        }
        inner
            .entries
            .submit_flow_async(flow_id, user_input)
            .await
            .map_err(Into::into)
    }

    /// Return a cached flow result or claim and resolve its private OAuth
    /// `external_done` transition exactly once.
    pub async fn get(&self, flow_id: &str) -> Result<Value, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.ensure_accepting()?;
        inner.get_flow(flow_id).await
    }

    /// Abort one flow and cancel/join its host-owned resolver.
    pub async fn abort(&self, flow_id: &str) -> Result<(), OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.cancel_flow(flow_id).await
    }

    /// Verify and stage a one-time external callback. This never exchanges the
    /// code; a later flow GET owns that resolver work.
    pub async fn callback(
        &self,
        params: OAuthCallbackParams,
    ) -> Result<OAuthCallbackResponse, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.ensure_accepting()?;
        inner.accept_callback(params).await
    }

    /// Whether the host currently owns a loaded OAuth provider for this domain.
    pub fn handles_domain(&self, domain: &str) -> bool {
        self.inner.upgrade().is_some_and(|inner| {
            let state = lock(&inner.state);
            state.accepting && state.providers.contains_key(domain)
        })
    }

    /// Whether a flow ID is in this host's private OAuth/config-flow registry.
    pub fn owns_flow(&self, flow_id: &str) -> bool {
        self.inner
            .upgrade()
            .is_some_and(|inner| lock(&inner.state).flows.contains_key(flow_id))
    }

    /// Check whether a provider's existing flow is still active, enforcing
    /// expiry without resolving an external callback or exchanging a code.
    pub async fn flow_is_active(&self, flow_id: &str) -> bool {
        let Ok(inner) = self.upgrade() else {
            return false;
        };
        if inner.ensure_accepting().is_err() || inner.expire_flow_if_due(flow_id).await {
            return false;
        }
        lock(&inner.state).flows.contains_key(flow_id)
    }

    /// Resolve the selected local application-credentials implementation for
    /// one real entry and return its stable-factory session.
    pub async fn session_for_entry(
        &self,
        domain: &str,
        entry_id: &str,
    ) -> Result<OAuthSession, OAuthFlowError> {
        let inner = self.upgrade()?;
        inner.session_for_entry(domain, entry_id).await
    }

    fn upgrade(&self) -> Result<Arc<HostInner>, OAuthFlowError> {
        self.inner.upgrade().ok_or(OAuthFlowError::ShuttingDown)
    }
}

struct OAuthFlowDriver {
    host: Weak<HostInner>,
    provider: Arc<dyn OAuthFlowProvider>,
    registration_id: Uuid,
}

impl ConfigEntryDriver for OAuthFlowDriver {
    fn domain(&self) -> &str {
        self.provider.domain()
    }

    fn integration_type(&self) -> &str {
        self.provider.integration_type()
    }

    fn supports_options(&self) -> bool {
        self.provider.supports_options()
    }

    fn supports_remove_device(&self) -> bool {
        self.provider.supports_remove_device()
    }

    fn supports_reconfigure(&self) -> bool {
        self.provider.supports_reconfigure()
    }

    fn supports_unload(&self) -> bool {
        self.provider.supports_unload()
    }

    fn version(&self) -> (u32, u32) {
        self.provider.version()
    }

    fn setup(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        self.provider.setup(runtime, entry)
    }

    fn unload(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        if let Some(host) = self.host.upgrade() {
            host.evict_entry_client(self.registration_id, entry);
        }
        self.provider.unload(runtime, entry)
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        self.provider.config_flow(request)
    }

    fn config_flow_async(
        &self,
        request: ConfigFlowRequest,
    ) -> Option<Pin<Box<dyn Future<Output = hass_core::Result<ConfigFlowResult>> + Send + 'static>>>
    {
        let host = self.host.clone();
        let provider = Arc::clone(&self.provider);
        let registration_id = self.registration_id;
        Some(Box::pin(async move {
            let Some(host) = host.upgrade() else {
                return Err(CoreError::Service("OAuth flow host is unavailable".into()));
            };
            host.process_config_flow(provider, registration_id, request)
                .await
        }))
    }

    fn on_create_entry(
        &self,
        manager: &ConfigEntries,
        request: &ConfigFlowRequest,
        entry: &ConfigEntry,
        result: &ConfigFlowResult,
    ) -> hass_core::Result<Option<ConfigFlowContinuation>> {
        self.provider
            .on_create_entry(manager, request, entry, result)
    }

    fn supported_subentry_types(
        &self,
        entry: &ConfigEntry,
    ) -> std::collections::BTreeMap<String, ConfigSubentryTypeInfo> {
        self.provider.supported_subentry_types(entry)
    }

    fn subentry_flow(
        &self,
        request: &ConfigSubentryFlowRequest,
    ) -> hass_core::Result<ConfigSubentryFlowResult> {
        self.provider.subentry_flow(request)
    }

    fn options_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        self.provider.options_flow(request)
    }

    fn migrate(
        &self,
        entry: &ConfigEntry,
        target_version: u32,
        target_minor_version: u32,
    ) -> hass_core::Result<Map<String, Value>> {
        self.provider
            .migrate(entry, target_version, target_minor_version)
    }

    fn remove(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        self.provider.remove(runtime, entry)
    }
}

impl HostInner {
    fn evict_entry_client(&self, registration_id: Uuid, entry: &ConfigEntry) {
        let Some(auth_domain) = entry
            .data
            .get(AUTH_IMPLEMENTATION_FIELD)
            .and_then(Value::as_str)
        else {
            return;
        };
        let removed = {
            let mut clients = lock(&self.client_cache);
            let keys = clients
                .keys()
                .filter(|key| {
                    key.registration_id == registration_id
                        && key.domain == entry.domain
                        && key.auth_domain == auth_domain
                })
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| clients.remove(&key))
                .collect::<Vec<_>>()
        };
        drop(removed);
    }
}

fn implementation_form(
    choices: &[ImplementationChoice],
    errors: Option<Map<String, Value>>,
) -> ConfigFlowResult {
    let options = choices
        .iter()
        .map(|choice| {
            json!({
                "value": choice.auth_domain(),
                "label": choice.label(),
            })
        })
        .collect::<Vec<_>>();
    ConfigFlowResult::Form {
        step_id: IMPLEMENTATION_STEP_ID.to_owned(),
        data_schema: Some(json!([{
            "name": AUTH_IMPLEMENTATION_FIELD,
            "required": true,
            "selector": {"select": {"options": options}},
        }])),
        errors,
        description_placeholders: None,
    }
}

fn validate_options(options: &OAuthFlowHostOptions) -> Result<(), OAuthFlowError> {
    if !(1..=DEFAULT_MAX_FLOWS).contains(&options.max_active_flows)
        || options.resolver_timeout.is_zero()
        || options.resolver_timeout > Duration::from_secs(300)
        || options.flow_ttl.is_zero()
        || options.flow_ttl > Duration::from_secs(60 * 60)
    {
        return Err(OAuthFlowError::InvalidConfiguration);
    }
    let url = &options.callback_url;
    let secure = url.scheme() == "https";
    let loopback_http = url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
    if (!secure && !loopback_http)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(OAuthFlowError::InvalidConfiguration);
    }
    Ok(())
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 128
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

fn validate_callback_params(params: &OAuthCallbackParams) -> Result<(), OAuthFlowError> {
    if params.state.is_empty()
        || params.state.len() > MAX_CALLBACK_VALUE_BYTES
        || params.state.chars().any(char::is_control)
    {
        return Err(OAuthFlowError::InvalidCallback);
    }
    for value in [&params.code, &params.error].into_iter().flatten() {
        if value.len() > MAX_CALLBACK_VALUE_BYTES || value.chars().any(char::is_control) {
            return Err(OAuthFlowError::InvalidCallback);
        }
    }
    if !params
        .code
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        && !params
            .error
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(OAuthFlowError::InvalidCallback);
    }
    Ok(())
}

fn state_digest(state: &str) -> String {
    Sha256::digest(state.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn map_callback_claim_error(error: CoreError) -> OAuthFlowError {
    match error {
        CoreError::NotFound(_) => OAuthFlowError::UnknownFlow,
        CoreError::InvalidInput(_) => OAuthFlowError::ReplayedCallback,
        other => OAuthFlowError::Core(other),
    }
}

enum ResolverWork {
    Callback(StagedCallback),
    AuthorizedReady(ExternalOAuthToken),
}

async fn resolve_external_flow(
    host: Weak<HostInner>,
    commit_lifetime: Arc<Mutex<Option<Arc<HostInner>>>>,
    entries: ConfigEntries,
    flow: Arc<FlowRecord>,
    lease: ExternalFlowLease,
    work: ResolverWork,
    resolver_timeout: Duration,
) -> ResolverOutcome {
    let (client, session, implementation, auth_implementation) = {
        let data = lock(&flow.data);
        (
            data.client.clone(),
            data.session.clone(),
            data.implementation.clone(),
            data.auth_implementation.clone(),
        )
    };
    let Some(client) = client else {
        return finish_abort(&entries, lease, "oauth_error");
    };
    let Some(session) = session else {
        return finish_abort(&entries, lease, "oauth_error");
    };
    let Some(implementation) = implementation else {
        return finish_abort(&entries, lease, "oauth_error");
    };
    let Some(auth_implementation) = auth_implementation else {
        return finish_abort(&entries, lease, "oauth_error");
    };

    let token = match work {
        ResolverWork::AuthorizedReady(token) => token,
        ResolverWork::Callback(callback) => {
            let completion = tokio::time::timeout(
                resolver_timeout,
                client.complete_verified_callback(callback.proof, callback.payload),
            )
            .await;
            match completion {
                Err(_) => return finish_abort(&entries, lease, "oauth_timeout"),
                Ok(Err(error)) => {
                    let reason = match error {
                        OAuthError::Timeout | OAuthError::ExpiredFlow => "oauth_timeout",
                        OAuthError::ReauthenticationRequired(_) => "oauth_unauthorized",
                        OAuthError::Network
                        | OAuthError::ResponseTooLarge
                        | OAuthError::Transient(_)
                        | OAuthError::ProviderError => "oauth_failed",
                        _ => "oauth_error",
                    };
                    return finish_abort(&entries, lease, reason);
                }
                Ok(Ok(OAuthCallbackResult::Denied { flow_id })) => {
                    if Some(flow_id) != lock(&flow.data).sdk_flow_id {
                        return finish_abort(&entries, lease, "oauth_error");
                    }
                    return finish_abort(&entries, lease, "user_rejected_authorize");
                }
                Ok(Ok(OAuthCallbackResult::Authorized { flow_id, token })) => {
                    if Some(flow_id) != lock(&flow.data).sdk_flow_id {
                        return finish_abort(&entries, lease, "oauth_error");
                    }
                    lock(&flow.data).token = Some(token.clone());
                    token
                }
            }
        }
    };

    let request = flow.request();
    let existing_entry = request
        .entry_id
        .as_deref()
        .and_then(|entry_id| entries.get_entry(entry_id));
    let context = OAuthPostAuthorizationContext {
        request,
        existing_entry,
        implementation,
        token: token.clone(),
    };
    let provider = Arc::clone(&flow.provider);
    let details =
        match tokio::time::timeout(resolver_timeout, provider.post_authorize(context)).await {
            Ok(Ok(details)) => details,
            Ok(Err(OAuthFlowError::ProviderAbort(reason))) => {
                return finish_abort(&entries, lease, reason);
            }
            Ok(Err(_)) | Err(_) => {
                if entries.release_external_flow(lease.clone()).is_err() {
                    return ResolverOutcome::Failed(ResolverFailure::Core);
                }
                return ResolverOutcome::RetryProvider;
            }
        };

    let Some(host) = host.upgrade() else {
        return finish_abort(&entries, lease, "oauth_error");
    };
    if !host.admit_commit(&flow) {
        return finish_abort(&entries, lease, "oauth_error");
    }
    *lock(&commit_lifetime) = Some(Arc::clone(&host));

    let mut data = details.data;
    data.insert(
        AUTH_IMPLEMENTATION_FIELD.to_owned(),
        Value::String(auth_implementation),
    );
    let details_title = details.title;
    let details_options = details.options;
    let details_unique_id = details.unique_id;
    let version = details.version;
    let minor_version = details.minor_version;
    let entries_for_commit = entries.clone();
    let lease_for_commit = lease.clone();
    let auth_implementation_data = data;
    let completion = session
        .commit_authorization(&token, move |provider_token| async move {
            let mut data = auth_implementation_data;
            data.insert(TOKEN_FIELD.to_owned(), provider_token);
            let result = ConfigFlowResult::CreateEntry {
                title: details_title,
                data,
                options: details_options,
                unique_id: details_unique_id,
                version,
                minor_version,
            };
            entries_for_commit
                .commit_external_flow(lease_for_commit, result)
                .map_err(|_| OAuthError::Storage)
        })
        .await;

    match completion {
        Ok(completion) => finish_completion_async(entries, completion).await,
        Err(_) => finish_abort(&entries, lease, "oauth_error"),
    }
}

fn finish_abort(
    entries: &ConfigEntries,
    lease: ExternalFlowLease,
    reason: &str,
) -> ResolverOutcome {
    match entries.commit_external_flow(
        lease,
        ConfigFlowResult::Abort {
            reason: reason.to_owned(),
        },
    ) {
        Ok(completion) => finish_completion(entries, completion),
        Err(_) => ResolverOutcome::Failed(ResolverFailure::Core),
    }
}

fn finish_completion(
    entries: &ConfigEntries,
    completion: DeferredExternalFlowCompletion,
) -> ResolverOutcome {
    match entries.finish_external_flow(completion) {
        Ok(value) => ResolverOutcome::Complete(value),
        Err(_) => ResolverOutcome::Failed(ResolverFailure::Core),
    }
}

async fn finish_completion_async(
    entries: ConfigEntries,
    completion: DeferredExternalFlowCompletion,
) -> ResolverOutcome {
    // The resolver's Committing lease stays held while real synchronous
    // setup/reload hooks finish. Those hooks may drive async integrations from
    // this blocking thread; they must not run on a Tokio worker.
    match tokio::task::spawn_blocking(move || finish_completion(&entries, completion)).await {
        Ok(outcome) => outcome,
        Err(_) => ResolverOutcome::Failed(ResolverFailure::Core),
    }
}

fn recover_resolver_panic(
    entries: &ConfigEntries,
    flow: &FlowRecord,
    lease: ExternalFlowLease,
) -> ResolverOutcome {
    if lock(&flow.data).token.is_some() {
        if entries.release_external_flow(lease).is_ok() {
            return ResolverOutcome::RetryProvider;
        }
        return ResolverOutcome::Failed(ResolverFailure::Core);
    }
    finish_abort(entries, lease, "oauth_error")
}

fn outcome_to_result(outcome: ResolverOutcome) -> Result<Value, OAuthFlowError> {
    match outcome {
        ResolverOutcome::Complete(value) => Ok(value),
        ResolverOutcome::RetryProvider => Err(OAuthFlowError::ProviderFailure),
        ResolverOutcome::Failed(ResolverFailure::Core) => Err(OAuthFlowError::Core(
            CoreError::Service("OAuth flow resolution failed".into()),
        )),
        ResolverOutcome::Failed(ResolverFailure::Aborted) => Err(OAuthFlowError::FlowAborted),
    }
}

#[cfg(test)]
mod lifecycle_cleanup_tests {
    use super::*;

    #[test]
    fn completed_flow_cleanup_registry_is_reaped_but_pending_work_is_retained() {
        let registration_id = Uuid::new_v4();
        let mut state = HostState {
            accepting: true,
            providers: HashMap::new(),
            flows: HashMap::new(),
            state_index: HashMap::new(),
            resolver_jobs: HashMap::new(),
            flow_cleanup_jobs: HashMap::new(),
            provider_cleanup_jobs: HashMap::new(),
            pending_provider_cleanup: HashMap::new(),
            shutdown_cleanup_job: None,
        };
        let mut completed = Vec::new();
        for index in 0..2048 {
            let flow_id = format!("expired-{index}");
            let job = LifecycleCleanup::completed();
            completed.push((flow_id.clone(), job.id));
            state.flow_cleanup_jobs.insert(
                flow_id,
                FlowCleanupRegistration {
                    registration_id,
                    reason: FlowCleanupReason::Expired,
                    job,
                },
            );
        }
        let (pending, finish_pending) = LifecycleCleanup::pending();
        let pending_id = pending.id;
        state.flow_cleanup_jobs.insert(
            "still-cleaning".to_owned(),
            FlowCleanupRegistration {
                registration_id,
                reason: FlowCleanupReason::Expired,
                job: pending,
            },
        );

        for (flow_id, cleanup_id) in completed {
            remove_completed_flow_cleanup(&mut state, &flow_id, cleanup_id);
        }
        assert_eq!(state.flow_cleanup_jobs.len(), 1);
        assert!(
            state
                .flow_cleanup_jobs
                .get("still-cleaning")
                .is_some_and(|cleanup| !cleanup.job.is_complete())
        );

        finish_pending.send_replace(true);
        remove_completed_flow_cleanup(&mut state, "still-cleaning", pending_id);
        assert!(state.flow_cleanup_jobs.is_empty());
    }
}
