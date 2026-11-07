//! Native application credential storage and provider callbacks.
//!
//! The credential collection lives in its own versioned Runtime metadata key.
//! It is deliberately separate from `ConfigEntry::data`; callers that expose
//! the admin DTOs must enforce the admin boundary because those DTOs include
//! `client_secret`, matching the pinned Home Assistant WebSocket contract.
//!
//! This crate supplies credential management and typed provider construction
//! seams. It does not implement OAuth authorization, callback validation, token
//! exchange, refresh, or token persistence.

mod slug;

use slug::slugify_id;

use futures_util::future::BoxFuture;
use hass_config::{ConfigEntries, ConfigEntry};
use hass_core::Runtime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

/// Home Assistant's pinned application-credentials Store version.
pub const STORAGE_VERSION: u64 = 1;
/// Home Assistant's pinned application-credentials Store key.
pub const STORAGE_KEY: &str = "application_credentials";
/// Name supplied for imported credentials without a configured name.
pub const DEFAULT_IMPORT_NAME: &str = "Import from configuration.yaml";
/// Maximum number of pending collection events held by one subscriber.
pub const SUBSCRIPTION_CAPACITY: usize = 64;

#[derive(Debug, Error)]
pub enum ApplicationCredentialsError {
    #[error("application credentials runtime has stopped")]
    IntegrationStopped,
    #[error("application credentials storage operation failed")]
    StorageUnavailable,
    #[error("application credentials storage document is malformed")]
    MalformedStore,
    #[error("unsupported application credentials storage version {0}")]
    UnsupportedStoreVersion(u64),
    #[error("No application_credentials platform for {0}")]
    NoPlatform(String),
    #[error(
        "Integration '{0}' application_credentials platform did not implement an authorization server or custom auth factory"
    )]
    InvalidPlatform(String),
    #[error("application_credentials platform already registered for {0}")]
    DuplicatePlatform(String),
    #[error("Updates not supported")]
    UpdatesNotSupported,
    #[error("Unable to find application_credentials_id {0}")]
    ItemNotFound(String),
    #[error("Cannot delete credential in use by integration {integration_domain}")]
    CredentialInUse { integration_domain: String },
    #[error("Config entry not found: {0}")]
    ConfigEntryNotFound(String),
    #[error("application_credentials {operation} callback failed for {domain}")]
    PlatformCallback {
        domain: String,
        operation: &'static str,
    },
}

pub type Result<T> = std::result::Result<T, ApplicationCredentialsError>;

/// The Home Assistant source type passed to custom platform factories.
#[derive(Clone, Eq, PartialEq)]
pub struct ClientCredential {
    pub client_id: String,
    pub client_secret: String,
    pub name: Option<String>,
}

impl fmt::Debug for ClientCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredential")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("name", &self.name)
            .finish()
    }
}

/// The Home Assistant source type returned by the default platform callback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationServer {
    pub authorize_url: String,
    pub token_url: String,
}

/// Typed input for the generic collection create command.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CreateCredential {
    pub domain: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl fmt::Debug for CreateCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateCredential")
            .field("domain", &self.domain)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("auth_domain", &self.auth_domain)
            .field("name", &self.name)
            .finish()
    }
}

/// Admin collection DTO. Serialization intentionally includes the secret for
/// parity with Home Assistant's admin-only `create` and `list` responses.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    pub id: String,
    pub domain: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl CredentialRecord {
    /// Return the provider key used by the pinned implementation.
    pub fn effective_auth_domain(&self) -> &str {
        self.auth_domain.as_deref().unwrap_or(&self.id)
    }

    fn client_credential(&self) -> ClientCredential {
        ClientCredential {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            name: self.name.clone(),
        }
    }
}

impl fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("id", &self.id)
            .field("domain", &self.domain)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("auth_domain", &self.auth_domain)
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoreDocument {
    version: u64,
    items: Vec<CredentialRecord>,
}

/// Ordered provider projection. Duplicate `auth_domain` values retain the
/// first key position and the last credential value, like the source dict.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderCredential {
    pub auth_domain: String,
    pub credential: ClientCredential,
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("auth_domain", &self.auth_domain)
            .field("credential", &self.credential)
            .finish()
    }
}

/// Non-secret catalog response from `application_credentials/config`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigCatalog {
    pub domains: Vec<String>,
    pub integrations: BTreeMap<String, DomainConfiguration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DomainConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_placeholders: Option<BTreeMap<String, String>>,
}

/// High-level category for a failed provider callback. It intentionally has
/// no free-form error string that could accidentally contain a client secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformFailure {
    Unavailable,
    InvalidResponse,
    Internal,
}

/// Provider-specific custom OAuth implementation marker. The OAuth engine is
/// outside this crate; this value is an opaque factory result only.
pub trait CustomOAuth2Implementation: Send + Sync + 'static {
    fn name(&self) -> Option<&str> {
        None
    }
}

pub trait AuthorizationServerFactory: Send + Sync + 'static {
    fn get<'a>(
        &'a self,
        runtime: &'a Runtime,
    ) -> BoxFuture<'a, std::result::Result<AuthorizationServer, PlatformFailure>>;
}

pub trait CustomAuthImplementationFactory: Send + Sync + 'static {
    fn create<'a>(
        &'a self,
        runtime: &'a Runtime,
        auth_domain: &'a str,
        credential: ClientCredential,
    ) -> BoxFuture<'a, std::result::Result<Arc<dyn CustomOAuth2Implementation>, PlatformFailure>>;
}

pub trait DescriptionPlaceholdersFactory: Send + Sync + 'static {
    fn get<'a>(
        &'a self,
        runtime: &'a Runtime,
    ) -> BoxFuture<'a, std::result::Result<BTreeMap<String, String>, PlatformFailure>>;
}

struct ClosureAuthorizationServerFactory<F>(F);

impl<F> AuthorizationServerFactory for ClosureAuthorizationServerFactory<F>
where
    F: for<'a> Fn(
            &'a Runtime,
        )
            -> BoxFuture<'a, std::result::Result<AuthorizationServer, PlatformFailure>>
        + Send
        + Sync
        + 'static,
{
    fn get<'a>(
        &'a self,
        runtime: &'a Runtime,
    ) -> BoxFuture<'a, std::result::Result<AuthorizationServer, PlatformFailure>> {
        (self.0)(runtime)
    }
}

struct ClosureCustomAuthFactory<F>(F);

impl<F> CustomAuthImplementationFactory for ClosureCustomAuthFactory<F>
where
    F: for<'a> Fn(
            &'a Runtime,
            &'a str,
            ClientCredential,
        ) -> BoxFuture<
            'a,
            std::result::Result<Arc<dyn CustomOAuth2Implementation>, PlatformFailure>,
        > + Send
        + Sync
        + 'static,
{
    fn create<'a>(
        &'a self,
        runtime: &'a Runtime,
        auth_domain: &'a str,
        credential: ClientCredential,
    ) -> BoxFuture<'a, std::result::Result<Arc<dyn CustomOAuth2Implementation>, PlatformFailure>>
    {
        (self.0)(runtime, auth_domain, credential)
    }
}

struct ClosureDescriptionFactory<F>(F);

impl<F> DescriptionPlaceholdersFactory for ClosureDescriptionFactory<F>
where
    F: for<'a> Fn(
            &'a Runtime,
        )
            -> BoxFuture<'a, std::result::Result<BTreeMap<String, String>, PlatformFailure>>
        + Send
        + Sync
        + 'static,
{
    fn get<'a>(
        &'a self,
        runtime: &'a Runtime,
    ) -> BoxFuture<'a, std::result::Result<BTreeMap<String, String>, PlatformFailure>> {
        (self.0)(runtime)
    }
}

/// The callbacks implemented by one actually loaded integration platform.
/// Custom auth construction wins when both auth callbacks are present.
#[derive(Clone, Default)]
pub struct ApplicationCredentialsPlatform {
    authorization_server: Option<Arc<dyn AuthorizationServerFactory>>,
    custom_auth: Option<Arc<dyn CustomAuthImplementationFactory>>,
    description_placeholders: Option<Arc<dyn DescriptionPlaceholdersFactory>>,
}

impl ApplicationCredentialsPlatform {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_authorization_server_factory<F>(mut self, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a Runtime,
            )
                -> BoxFuture<'a, std::result::Result<AuthorizationServer, PlatformFailure>>
            + Send
            + Sync
            + 'static,
    {
        self.authorization_server = Some(Arc::new(ClosureAuthorizationServerFactory(factory)));
        self
    }

    pub fn with_custom_auth_factory<F>(mut self, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a Runtime,
                &'a str,
                ClientCredential,
            ) -> BoxFuture<
                'a,
                std::result::Result<Arc<dyn CustomOAuth2Implementation>, PlatformFailure>,
            > + Send
            + Sync
            + 'static,
    {
        self.custom_auth = Some(Arc::new(ClosureCustomAuthFactory(factory)));
        self
    }

    pub fn with_description_placeholders<F>(mut self, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a Runtime,
            ) -> BoxFuture<
                'a,
                std::result::Result<BTreeMap<String, String>, PlatformFailure>,
            > + Send
            + Sync
            + 'static,
    {
        self.description_placeholders = Some(Arc::new(ClosureDescriptionFactory(factory)));
        self
    }

    fn has_auth_factory(&self) -> bool {
        self.custom_auth.is_some() || self.authorization_server.is_some()
    }
}

impl fmt::Debug for ApplicationCredentialsPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationCredentialsPlatform")
            .field("authorization_server", &self.authorization_server.is_some())
            .field("custom_auth", &self.custom_auth.is_some())
            .field(
                "description_placeholders",
                &self.description_placeholders.is_some(),
            )
            .finish()
    }
}

/// Default local implementation descriptor. It carries client material for a
/// later OAuth engine, but does not perform any HTTP or token operation here.
#[derive(Clone, Eq, PartialEq)]
pub struct LocalOAuth2Implementation {
    pub auth_domain: String,
    pub client_id: String,
    client_secret: String,
    pub name: String,
    pub authorization_server: AuthorizationServer,
}

impl LocalOAuth2Implementation {
    pub fn client_secret(&self) -> &str {
        &self.client_secret
    }
}

impl fmt::Debug for LocalOAuth2Implementation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalOAuth2Implementation")
            .field("auth_domain", &self.auth_domain)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("name", &self.name)
            .field("authorization_server", &self.authorization_server)
            .finish()
    }
}

pub enum ResolvedOAuthImplementation {
    Local(LocalOAuth2Implementation),
    Custom {
        auth_domain: String,
        implementation: Arc<dyn CustomOAuth2Implementation>,
    },
}

impl fmt::Debug for ResolvedOAuthImplementation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(implementation) => f.debug_tuple("Local").field(implementation).finish(),
            Self::Custom { auth_domain, .. } => f
                .debug_struct("Custom")
                .field("auth_domain", auth_domain)
                .field("implementation", &"[OPAQUE]")
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialChangeType {
    Added,
    Removed,
}

/// Private collection observer event for the admin adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialEvent {
    pub change: CredentialChangeType,
    pub item: CredentialRecord,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionError {
    #[error("credential subscription fell behind by {0} events")]
    Lagged(u64),
    #[error("credential subscription is closed")]
    Closed,
}

struct SubscriptionState {
    snapshot: Option<Vec<CredentialRecord>>,
    pending: VecDeque<CredentialEvent>,
    lagged: u64,
    closed: bool,
}

struct SubscriptionChannel {
    id: Uuid,
    state: Mutex<SubscriptionState>,
    notify: Notify,
}

impl SubscriptionChannel {
    fn new(id: Uuid, snapshot: Vec<CredentialRecord>) -> Self {
        Self {
            id,
            state: Mutex::new(SubscriptionState {
                snapshot: Some(snapshot),
                pending: VecDeque::new(),
                lagged: 0,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    fn push(&self, event: CredentialEvent) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        if state.pending.len() == SUBSCRIPTION_CAPACITY {
            state.pending.pop_front();
            state.lagged = state.lagged.saturating_add(1);
        }
        state.pending.push_back(event);
        drop(state);
        self.notify.notify_one();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.snapshot = None;
        state.pending.clear();
        state.lagged = 0;
        drop(state);
        self.notify.notify_waiters();
    }
}

/// Admin-only observer handle. It holds neither Runtime nor ConfigEntries; its
/// owner and subscription registration are both weak/exact-lifetime handles.
pub struct CredentialSubscription {
    owner: Weak<IntegrationInner>,
    channel: Arc<SubscriptionChannel>,
}

impl CredentialSubscription {
    /// Take the initial snapshot once. A shutdown before this call clears it.
    pub fn take_snapshot(&self) -> Option<Vec<CredentialRecord>> {
        lock(&self.channel.state).snapshot.take()
    }

    pub async fn recv(&mut self) -> std::result::Result<CredentialEvent, SubscriptionError> {
        loop {
            let notified = self.channel.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(event) = self.try_recv()? {
                return Ok(event);
            }
            notified.await;
        }
    }

    fn try_recv(&self) -> std::result::Result<Option<CredentialEvent>, SubscriptionError> {
        let mut state = lock(&self.channel.state);
        if state.lagged > 0 {
            let lagged = std::mem::take(&mut state.lagged);
            return Err(SubscriptionError::Lagged(lagged));
        }
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        if state.closed {
            return Err(SubscriptionError::Closed);
        }
        Ok(None)
    }
}

impl Drop for CredentialSubscription {
    fn drop(&mut self) {
        self.channel.close();
        if let Some(owner) = self.owner.upgrade() {
            let mut state = lock(&owner.collection);
            state.subscribers.retain(|channel| {
                channel
                    .upgrade()
                    .is_some_and(|registered| registered.id != self.channel.id)
            });
        }
    }
}

struct CredentialState {
    items: Vec<CredentialRecord>,
    subscribers: Vec<Weak<SubscriptionChannel>>,
}

struct RegisteredPlatform {
    registration_id: Uuid,
    platform: Arc<ApplicationCredentialsPlatform>,
}

#[derive(Default)]
struct LifecycleState {
    closed: bool,
    active_operations: usize,
}

struct IntegrationInner {
    runtime: Runtime,
    config_entries: ConfigEntries,
    collection: Mutex<CredentialState>,
    platforms: Mutex<BTreeMap<String, RegisteredPlatform>>,
    lifecycle: Mutex<LifecycleState>,
    shutdown_notify: Notify,
}

impl IntegrationInner {
    fn enter(owner: &Weak<Self>) -> Result<ActiveOperation> {
        let inner = owner
            .upgrade()
            .ok_or(ApplicationCredentialsError::IntegrationStopped)?;
        {
            let mut lifecycle = lock(&inner.lifecycle);
            if lifecycle.closed {
                return Err(ApplicationCredentialsError::IntegrationStopped);
            }
            lifecycle.active_operations += 1;
        }
        Ok(ActiveOperation { inner })
    }

    fn close(&self) {
        {
            let mut lifecycle = lock(&self.lifecycle);
            if lifecycle.closed {
                return;
            }
            lifecycle.closed = true;
        }
        // Platform closures may own other registration guards. Drop them
        // after releasing the registry lock so their destructors can reenter.
        let platforms = std::mem::take(&mut *lock(&self.platforms));
        drop(platforms);
        let mut collection = lock(&self.collection);
        for subscriber in collection
            .subscribers
            .drain(..)
            .filter_map(|weak| weak.upgrade())
        {
            subscriber.close();
        }
        drop(collection);
        self.shutdown_notify.notify_waiters();
    }

    fn is_idle(&self) -> bool {
        lock(&self.lifecycle).active_operations == 0
    }
}

struct ActiveOperation {
    inner: Arc<IntegrationInner>,
}

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        let became_idle = {
            let mut lifecycle = lock(&self.inner.lifecycle);
            lifecycle.active_operations = lifecycle.active_operations.saturating_sub(1);
            lifecycle.closed && lifecycle.active_operations == 0
        };
        if became_idle {
            self.inner.shutdown_notify.notify_waiters();
        }
    }
}

/// Strong owner for one live native Application Credentials integration.
/// Manager and provider-registry handles returned from this object are weak.
pub struct ApplicationCredentialsIntegration {
    inner: Arc<IntegrationInner>,
}

impl ApplicationCredentialsIntegration {
    /// Load the private versioned collection from Runtime metadata.
    pub fn new(runtime: Runtime, config_entries: ConfigEntries) -> Result<Self> {
        let items = load_items(&runtime)?;
        Ok(Self {
            inner: Arc::new(IntegrationInner {
                runtime,
                config_entries,
                collection: Mutex::new(CredentialState {
                    items,
                    subscribers: Vec::new(),
                }),
                platforms: Mutex::new(BTreeMap::new()),
                lifecycle: Mutex::new(LifecycleState::default()),
                shutdown_notify: Notify::new(),
            }),
        })
    }

    /// Return a cloneable manager handle that does not retain this owner.
    pub fn manager(&self) -> ApplicationCredentialsManager {
        ApplicationCredentialsManager {
            owner: Arc::downgrade(&self.inner),
        }
    }

    /// Return a weak registry handle for actually loaded provider platforms.
    pub fn platform_registry(&self) -> ApplicationCredentialsPlatformRegistry {
        ApplicationCredentialsPlatformRegistry {
            owner: Arc::downgrade(&self.inner),
        }
    }

    /// Stop new calls, clear providers and pending observer data, then wait for
    /// all already-admitted sync or async callbacks to finish.
    /// Callbacks must request teardown through their outer owner rather than
    /// await this barrier from inside their own admitted operation.
    pub async fn shutdown(&self) {
        self.inner.close();
        loop {
            let notified = self.inner.shutdown_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.is_idle() {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for ApplicationCredentialsIntegration {
    fn drop(&mut self) {
        self.inner.close();
    }
}

/// Weak handle to the loaded platform registry.
#[derive(Clone)]
pub struct ApplicationCredentialsPlatformRegistry {
    owner: Weak<IntegrationInner>,
}

impl ApplicationCredentialsPlatformRegistry {
    /// Install one loaded platform. Dropping the returned guard unregisters
    /// only this exact registration.
    pub fn register(
        &self,
        domain: impl Into<String>,
        platform: ApplicationCredentialsPlatform,
    ) -> Result<PlatformRegistrationGuard> {
        let active = IntegrationInner::enter(&self.owner)?;
        let domain = domain.into();
        if !platform.has_auth_factory() {
            return Err(ApplicationCredentialsError::InvalidPlatform(domain));
        }
        let registration_id = Uuid::new_v4();
        let mut platforms = lock(&active.inner.platforms);
        if lock(&active.inner.lifecycle).closed {
            return Err(ApplicationCredentialsError::IntegrationStopped);
        }
        if platforms.contains_key(&domain) {
            return Err(ApplicationCredentialsError::DuplicatePlatform(domain));
        }
        platforms.insert(
            domain.clone(),
            RegisteredPlatform {
                registration_id,
                platform: Arc::new(platform),
            },
        );
        drop(platforms);
        drop(active);
        Ok(PlatformRegistrationGuard {
            owner: self.owner.clone(),
            domain,
            registration_id,
        })
    }

    /// List only provider platforms currently registered by loaded owners.
    pub fn loaded_domains(&self) -> Result<Vec<String>> {
        let active = IntegrationInner::enter(&self.owner)?;
        let domains = lock(&active.inner.platforms).keys().cloned().collect();
        Ok(domains)
    }
}

/// Exact RAII registration handle. It does not keep the integration alive.
pub struct PlatformRegistrationGuard {
    owner: Weak<IntegrationInner>,
    domain: String,
    registration_id: Uuid,
}

impl Drop for PlatformRegistrationGuard {
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let removed = {
            let mut platforms = lock(&owner.platforms);
            if platforms
                .get(&self.domain)
                .is_some_and(|registered| registered.registration_id == self.registration_id)
            {
                platforms.remove(&self.domain)
            } else {
                None
            }
        };
        drop(removed);
    }
}

/// Cloneable weak manager handle for the native credential collection.
#[derive(Clone)]
pub struct ApplicationCredentialsManager {
    owner: Weak<IntegrationInner>,
}

impl ApplicationCredentialsManager {
    /// Admin DTO list. The root API adapter must require admin authority before
    /// calling this method because every returned item includes its secret.
    pub fn list_admin(&self) -> Result<Vec<CredentialRecord>> {
        let active = IntegrationInner::enter(&self.owner)?;
        let items = lock(&active.inner.collection).items.clone();
        Ok(items)
    }

    /// Create one item after checking the real loaded-provider registry.
    pub fn create_admin(&self, input: CreateCredential) -> Result<CredentialRecord> {
        let active = IntegrationInner::enter(&self.owner)?;
        create_item(&active.inner, input)
    }

    /// The source collection rejects updates after checking that the ID exists.
    pub fn update_admin(&self, item_id: &str) -> Result<CredentialRecord> {
        let active = IntegrationInner::enter(&self.owner)?;
        let state = lock(&active.inner.collection);
        if !state.items.iter().any(|item| item.id == item_id) {
            return Err(ApplicationCredentialsError::ItemNotFound(
                item_id.to_owned(),
            ));
        }
        Err(ApplicationCredentialsError::UpdatesNotSupported)
    }

    /// Delete an item only when neither its stable ID nor its compatibility
    /// auth-domain alias is referenced by an entry in the same domain.
    pub fn delete_admin(&self, item_id: &str) -> Result<()> {
        let active = IntegrationInner::enter(&self.owner)?;
        let mut state = lock(&active.inner.collection);
        let Some(index) = state.items.iter().position(|item| item.id == item_id) else {
            return Err(ApplicationCredentialsError::ItemNotFound(
                item_id.to_owned(),
            ));
        };
        let item = state.items[index].clone();
        for entry in active.inner.config_entries.list_entries(Some(&item.domain)) {
            let Some(auth_implementation) = entry
                .data
                .get("auth_implementation")
                .and_then(Value::as_str)
            else {
                continue;
            };
            if auth_implementation == item.id
                || item.auth_domain.as_deref() == Some(auth_implementation)
            {
                return Err(ApplicationCredentialsError::CredentialInUse {
                    integration_domain: entry.domain,
                });
            }
        }
        let mut candidate = state.items.clone();
        candidate.remove(index);
        persist_items(&active.inner.runtime, &candidate)?;
        state.items = candidate;
        publish(
            &mut state,
            CredentialEvent {
                change: CredentialChangeType::Removed,
                item,
            },
        );
        Ok(())
    }

    /// Import a legacy YAML credential. A duplicate suggested base ID is a
    /// no-op and never overwrites the existing secret.
    pub fn import_client_credential(
        &self,
        domain: impl Into<String>,
        credential: ClientCredential,
        auth_domain: Option<String>,
    ) -> Result<bool> {
        let active = IntegrationInner::enter(&self.owner)?;
        let domain = domain.into();
        let raw_suggestion = format!("{domain}.{}", credential.client_id);
        let base_id = slugify_id(&raw_suggestion);
        let mut state = lock(&active.inner.collection);
        if state.items.iter().any(|item| item.id == base_id) {
            return Ok(false);
        }

        let imported_auth_domain = auth_domain
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| domain.clone());
        let imported_name = credential
            .name
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_IMPORT_NAME.to_owned());
        let input = CreateCredential {
            domain,
            client_id: credential.client_id,
            client_secret: credential.client_secret,
            auth_domain: Some(imported_auth_domain),
            name: Some(imported_name),
        };
        create_item_locked(&active.inner, &mut state, input)?;
        Ok(true)
    }

    /// Return stored client credentials for one provider domain, preserving
    /// source order and last-write-wins duplicate aliases.
    pub fn credentials_for_provider(&self, domain: &str) -> Result<Vec<ProviderCredential>> {
        let active = IntegrationInner::enter(&self.owner)?;
        let state = lock(&active.inner.collection);
        Ok(project_credentials(&state.items, domain))
    }

    /// Resolve the selected credential ID for a real ConfigEntry.
    pub fn credential_id_for_config_entry(&self, config_entry_id: &str) -> Result<Option<String>> {
        let active = IntegrationInner::enter(&self.owner)?;
        let entry = active
            .inner
            .config_entries
            .get_entry(config_entry_id)
            .ok_or_else(|| {
                ApplicationCredentialsError::ConfigEntryNotFound(config_entry_id.to_owned())
            })?;
        credential_id_for_entry(&active.inner, &entry)
    }

    /// Return the non-secret supported-domain catalog and invoke each loaded
    /// provider's placeholders callback when one is supplied.
    pub async fn config(&self) -> Result<ConfigCatalog> {
        let active = IntegrationInner::enter(&self.owner)?;
        let platforms: Vec<(String, Arc<ApplicationCredentialsPlatform>)> =
            lock(&active.inner.platforms)
                .iter()
                .map(|(domain, registered)| (domain.clone(), Arc::clone(&registered.platform)))
                .collect();
        let mut domains = Vec::with_capacity(platforms.len());
        let mut integrations = BTreeMap::new();
        for (domain, platform) in platforms {
            domains.push(domain.clone());
            let placeholders = if let Some(factory) = &platform.description_placeholders {
                Some(factory.get(&active.inner.runtime).await.map_err(|_| {
                    ApplicationCredentialsError::PlatformCallback {
                        domain: domain.clone(),
                        operation: "description_placeholders",
                    }
                })?)
            } else {
                None
            };
            integrations.insert(
                domain,
                DomainConfiguration {
                    description_placeholders: placeholders,
                },
            );
        }
        Ok(ConfigCatalog {
            domains,
            integrations,
        })
    }

    /// Create the typed default descriptor or invoke each loaded platform's
    /// custom factory. No OAuth HTTP engine is run here.
    pub async fn provide_implementations(
        &self,
        domain: &str,
    ) -> Result<Vec<ResolvedOAuthImplementation>> {
        let active = IntegrationInner::enter(&self.owner)?;
        let platform = match lock(&active.inner.platforms)
            .get(domain)
            .map(|registered| Arc::clone(&registered.platform))
        {
            Some(platform) => platform,
            None => return Ok(Vec::new()),
        };
        let credentials = {
            let state = lock(&active.inner.collection);
            project_credentials(&state.items, domain)
        };

        if let Some(factory) = &platform.custom_auth {
            let mut implementations = Vec::with_capacity(credentials.len());
            for provider_credential in credentials {
                let auth_domain = provider_credential.auth_domain;
                let implementation = factory
                    .create(
                        &active.inner.runtime,
                        &auth_domain,
                        provider_credential.credential,
                    )
                    .await
                    .map_err(|_| ApplicationCredentialsError::PlatformCallback {
                        domain: domain.to_owned(),
                        operation: "custom_auth_implementation",
                    })?;
                implementations.push(ResolvedOAuthImplementation::Custom {
                    auth_domain,
                    implementation,
                });
            }
            return Ok(implementations);
        }

        let Some(factory) = &platform.authorization_server else {
            return Err(ApplicationCredentialsError::InvalidPlatform(
                domain.to_owned(),
            ));
        };
        // The source resolves the authorization server even for an empty
        // credential collection, before building its result list.
        let authorization_server = factory.get(&active.inner.runtime).await.map_err(|_| {
            ApplicationCredentialsError::PlatformCallback {
                domain: domain.to_owned(),
                operation: "authorization_server",
            }
        })?;
        Ok(credentials
            .into_iter()
            .map(|provider_credential| {
                let credential = provider_credential.credential;
                let name = credential
                    .name
                    .clone()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| credential.client_id.clone());
                ResolvedOAuthImplementation::Local(LocalOAuth2Implementation {
                    auth_domain: provider_credential.auth_domain,
                    client_id: credential.client_id,
                    client_secret: credential.client_secret,
                    name,
                    authorization_server: authorization_server.clone(),
                })
            })
            .collect())
    }

    /// Create a race-free initial snapshot plus a private bounded observer.
    /// The adapter should send its subscribe acknowledgement before draining
    /// events; any event after this snapshot is queued for the returned handle.
    pub fn subscribe_admin(&self) -> Result<CredentialSubscription> {
        let active = IntegrationInner::enter(&self.owner)?;
        let mut state = lock(&active.inner.collection);
        if lock(&active.inner.lifecycle).closed {
            return Err(ApplicationCredentialsError::IntegrationStopped);
        }
        let channel = Arc::new(SubscriptionChannel::new(
            Uuid::new_v4(),
            state.items.clone(),
        ));
        state.subscribers.push(Arc::downgrade(&channel));
        Ok(CredentialSubscription {
            owner: Arc::downgrade(&active.inner),
            channel,
        })
    }
}

fn credential_id_for_entry(
    owner: &IntegrationInner,
    entry: &ConfigEntry,
) -> Result<Option<String>> {
    if lock(&owner.platforms).get(&entry.domain).is_none() {
        return Ok(None);
    }
    let Some(auth_implementation) = entry
        .data
        .get("auth_implementation")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let state = lock(&owner.collection);
    Ok(state
        .items
        .iter()
        .find(|item| {
            item.domain == entry.domain && item.effective_auth_domain() == auth_implementation
        })
        .map(|item| item.id.clone()))
}

fn create_item(owner: &IntegrationInner, input: CreateCredential) -> Result<CredentialRecord> {
    let mut state = lock(&owner.collection);
    create_item_locked(owner, &mut state, input)
}

fn create_item_locked(
    owner: &IntegrationInner,
    state: &mut CredentialState,
    input: CreateCredential,
) -> Result<CredentialRecord> {
    if lock(&owner.platforms).get(&input.domain).is_none() {
        return Err(ApplicationCredentialsError::NoPlatform(input.domain));
    }
    let mut normalized = input;
    normalized.client_id = python_strip(&normalized.client_id).to_owned();
    normalized.client_secret = python_strip(&normalized.client_secret).to_owned();

    let suggested_id = format!("{}.{}", normalized.domain, normalized.client_id);
    let base_id = slugify_id(&suggested_id);
    let mut item_id = base_id.clone();
    let mut attempt = 1_u64;
    while state.items.iter().any(|item| item.id == item_id) {
        attempt += 1;
        item_id = format!("{base_id}_{attempt}");
    }
    let item = CredentialRecord {
        id: item_id,
        domain: normalized.domain,
        client_id: normalized.client_id,
        client_secret: normalized.client_secret,
        auth_domain: normalized.auth_domain,
        name: normalized.name,
    };
    let mut candidate = state.items.clone();
    candidate.push(item.clone());
    persist_items(&owner.runtime, &candidate)?;
    state.items = candidate;
    publish(
        state,
        CredentialEvent {
            change: CredentialChangeType::Added,
            item: item.clone(),
        },
    );
    Ok(item)
}

fn project_credentials(items: &[CredentialRecord], domain: &str) -> Vec<ProviderCredential> {
    let mut credentials: Vec<ProviderCredential> = Vec::new();
    for item in items.iter().filter(|item| item.domain == domain) {
        let auth_domain = item.effective_auth_domain().to_owned();
        let credential = item.client_credential();
        if let Some(existing) = credentials
            .iter_mut()
            .find(|candidate| candidate.auth_domain == auth_domain)
        {
            existing.credential = credential;
        } else {
            credentials.push(ProviderCredential {
                auth_domain,
                credential,
            });
        }
    }
    credentials
}

fn publish(state: &mut CredentialState, event: CredentialEvent) {
    state.subscribers.retain(|weak| {
        if let Some(subscriber) = weak.upgrade() {
            subscriber.push(event.clone());
            true
        } else {
            false
        }
    });
}

fn load_items(runtime: &Runtime) -> Result<Vec<CredentialRecord>> {
    let Some(value) = runtime
        .get_data(STORAGE_KEY)
        .map_err(|_| ApplicationCredentialsError::StorageUnavailable)?
    else {
        return Ok(Vec::new());
    };
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(ApplicationCredentialsError::MalformedStore)?;
    if version != STORAGE_VERSION {
        return Err(ApplicationCredentialsError::UnsupportedStoreVersion(
            version,
        ));
    }
    let document: StoreDocument =
        serde_json::from_value(value).map_err(|_| ApplicationCredentialsError::MalformedStore)?;
    let mut ids = std::collections::HashSet::new();
    if document
        .items
        .iter()
        .any(|item| !ids.insert(item.id.as_str()))
    {
        return Err(ApplicationCredentialsError::MalformedStore);
    }
    Ok(document.items)
}

fn persist_items(runtime: &Runtime, items: &[CredentialRecord]) -> Result<()> {
    let document = StoreDocument {
        version: STORAGE_VERSION,
        items: items.to_vec(),
    };
    let value = serde_json::to_value(document)
        .map_err(|_| ApplicationCredentialsError::StorageUnavailable)?;
    runtime
        .set_data(STORAGE_KEY, &value)
        .map_err(|_| ApplicationCredentialsError::StorageUnavailable)
}

fn python_strip(value: &str) -> &str {
    // CPython includes these four ASCII information separators in str.strip().
    value.trim_matches(|character: char| {
        character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
