//! Durable config entries, native integration lifecycles, and config flows.
//!
//! Integration behavior is supplied by registered [`ConfigEntryDriver`]s. An
//! entry without a driver cannot be reported as successfully configured.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::task::{Context as TaskContext, Poll};

use chrono::{DateTime, Duration, Utc};
use hass_core::{Context, Error, HomeContext, Result, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use uuid::Uuid;

const METADATA_KEY: &str = "hass.config_entries.v1";
const DOCUMENT_VERSION: u32 = 1;
const MAX_RETRY_DELAY_SECS: u64 = 86_400;
const MAX_DISCOVERY_KEYS_PER_DOMAIN: usize = 10;

/// Config entry state, following Home Assistant's persisted lifecycle values.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEntryState {
    #[default]
    NotLoaded,
    SetupInProgress,
    Loaded,
    SetupError,
    SetupRetry,
    MigrationError,
    UnloadInProgress,
    FailedUnload,
}

impl ConfigEntryState {
    fn is_loaded(self) -> bool {
        self == Self::Loaded
    }
}

/// Serializable identity for one discovery provider's view of a config entry.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryKey {
    pub domain: String,
    pub key: DiscoveryKeyValue,
    pub version: u32,
}

/// A discovery key is either one string or an ordered tuple of strings.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum DiscoveryKeyValue {
    String(String),
    Tuple(Vec<String>),
}

/// Trusted integration view of a config discovery flow's original data.
/// This is deliberately not serializable and is not an ordinary API fragment.
#[derive(Clone)]
pub struct ConfigDiscoveryFlowSnapshot {
    pub flow_id: String,
    pub domain: String,
    pub initial_data: Map<String, Value>,
    pub data_kind: Option<DiscoveryDataKind>,
}

/// Native init-data provenance. This is never read from JSON flow context or
/// serialized into an API response; a source string alone cannot supply it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryDataKind {
    Usb,
}

/// Persisted config entry data. `data` and `options` are available to trusted
/// native drivers, but are intentionally omitted from ordinary API fragments.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigEntry {
    pub entry_id: String,
    pub domain: String,
    pub title: String,
    pub source: String,
    pub unique_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub discovery_keys: BTreeMap<String, Vec<DiscoveryKey>>,
    pub version: u32,
    pub minor_version: u32,
    pub data: Map<String, Value>,
    pub options: Map<String, Value>,
    /// Persistent child configurations, serialized in creation order like
    /// Home Assistant's `subentries` storage list.
    #[serde(default, deserialize_with = "deserialize_subentries")]
    pub subentries: Vec<ConfigSubentry>,
    pub state: ConfigEntryState,
    pub reason: Option<String>,
    pub retry_attempts: u32,
    pub retry_at: Option<DateTime<Utc>>,
    pub disabled_by: Option<String>,
    pub needs_reauth: bool,
    pub require_reload: bool,
    pub pref_disable_new_entities: bool,
    pub pref_disable_polling: bool,
    #[serde(default)]
    pub error_reason_translation_domain: Option<String>,
    #[serde(default)]
    pub error_reason_translation_key: Option<String>,
    #[serde(default)]
    pub error_reason_translation_placeholders: Option<Map<String, Value>>,
    #[serde(default)]
    pub supports_options: bool,
    #[serde(default)]
    pub supports_remove_device: bool,
    #[serde(default)]
    pub supports_unload: bool,
    #[serde(default)]
    pub supports_reconfigure: bool,
    #[serde(default)]
    pub supported_subentry_types: Map<String, Value>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    #[serde(default = "default_integration_type")]
    pub integration_type: String,
}

fn default_integration_type() -> String {
    "hub".into()
}

impl fmt::Debug for ConfigEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigEntry")
            .field("entry_id", &self.entry_id)
            .field("domain", &self.domain)
            .field("title", &self.title)
            .field("source", &self.source)
            .field("unique_id", &self.unique_id)
            .field("version", &self.version)
            .field("minor_version", &self.minor_version)
            .field("data", &"[REDACTED]")
            .field("options", &"[REDACTED]")
            .field("subentries", &self.subentries.len())
            .field("state", &self.state)
            .field("reason", &self.reason)
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_at", &self.retry_at)
            .field("disabled_by", &self.disabled_by)
            .field("needs_reauth", &self.needs_reauth)
            .field("require_reload", &self.require_reload)
            .field("created_at", &self.created_at)
            .field("modified_at", &self.modified_at)
            .finish()
    }
}

impl ConfigEntry {
    /// Produce the redacted fragment returned by config-entry list/single APIs.
    pub fn public_json(&self) -> Value {
        json!({
            "entry_id": self.entry_id,
            "domain": self.domain,
            "title": self.title,
            "source": self.source,
            "state": self.state,
            "reason": self.reason,
            "error_reason_translation_domain": self.error_reason_translation_domain,
            "error_reason_translation_key": self.error_reason_translation_key,
            "error_reason_translation_placeholders": self.error_reason_translation_placeholders,
            "supported_subentry_types": self.supported_subentry_types,
            "supports_options": self.supports_options,
            "supports_reconfigure": self.supports_reconfigure,
            "supports_remove_device": self.supports_remove_device,
            "supports_unload": self.supports_unload,
            "disabled_by": self.disabled_by,
            "pref_disable_new_entities": self.pref_disable_new_entities,
            "pref_disable_polling": self.pref_disable_polling,
            "created_at": self.created_at.timestamp() as f64 + self.created_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0,
            "modified_at": self.modified_at.timestamp() as f64 + self.modified_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0,
            "num_subentries": self.subentries.len(),
        })
    }
}

/// Persisted child configuration belonging to a config entry.
///
/// The private `data` map is available to the owning integration driver and
/// typed manager APIs. It is omitted by [`ConfigSubentry::public_json`] and
/// redacted from debug output.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSubentry {
    pub subentry_id: String,
    pub subentry_type: String,
    pub title: String,
    pub unique_id: Option<String>,
    pub data: Map<String, Value>,
}

impl fmt::Debug for ConfigSubentry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigSubentry")
            .field("subentry_id", &self.subentry_id)
            .field("subentry_type", &self.subentry_type)
            .field("title", &self.title)
            .field("unique_id", &self.unique_id)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl ConfigSubentry {
    /// Return the public subentry fragment used by config listing adapters.
    pub fn public_json(&self) -> Value {
        json!({
            "subentry_id": self.subentry_id,
            "subentry_type": self.subentry_type,
            "title": self.title,
            "unique_id": self.unique_id,
        })
    }
}

/// Input for adding a new subentry. A persistent ID is assigned by the manager.
#[derive(Clone)]
pub struct NewConfigSubentry {
    pub subentry_type: String,
    pub title: String,
    pub unique_id: Option<String>,
    pub data: Map<String, Value>,
}

impl NewConfigSubentry {
    /// Create a new child configuration seed with no optional identity.
    pub fn new(
        subentry_type: impl Into<String>,
        title: impl Into<String>,
        data: Map<String, Value>,
    ) -> Self {
        Self {
            subentry_type: subentry_type.into(),
            title: title.into(),
            unique_id: None,
            data,
        }
    }
}

/// Fields to replace when updating an existing subentry.
///
/// `unique_id: None` means unchanged; `Some(None)` clears the optional ID.
#[derive(Clone, Default)]
pub struct ConfigSubentryUpdate {
    pub title: Option<String>,
    pub unique_id: Option<Option<String>>,
    pub data: Option<Map<String, Value>>,
}

/// Capability advertised for one supported subentry type.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSubentryTypeInfo {
    pub supports_reconfigure: bool,
}

/// Request for one native subentry-flow step.
#[derive(Clone)]
pub struct ConfigSubentryFlowRequest {
    pub flow_id: String,
    pub domain: String,
    pub entry_id: String,
    pub subentry_type: String,
    pub subentry_id: Option<String>,
    pub step_id: String,
    pub source: String,
    pub user_input: Option<Map<String, Value>>,
    pub context: Map<String, Value>,
}

/// Result from a registered native subentry-flow callback.
#[derive(Clone)]
pub enum ConfigSubentryFlowResult {
    Form {
        step_id: String,
        data_schema: Option<Value>,
        errors: Option<Map<String, Value>>,
        description_placeholders: Option<Map<String, Value>>,
    },
    CreateEntry {
        title: String,
        data: Map<String, Value>,
        unique_id: Option<String>,
    },
    UpdateAndAbort {
        update: ConfigSubentryUpdate,
        data_updates: Option<Map<String, Value>>,
        reason: Option<String>,
        reload: bool,
    },
    Abort {
        reason: String,
    },
}

impl fmt::Debug for ConfigSubentryFlowResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Form { step_id, .. } => f
                .debug_struct("Form")
                .field("step_id", step_id)
                .finish_non_exhaustive(),
            Self::CreateEntry {
                title, unique_id, ..
            } => f
                .debug_struct("CreateEntry")
                .field("title", title)
                .field("data", &"[REDACTED]")
                .field("unique_id", unique_id)
                .finish(),
            Self::UpdateAndAbort {
                update,
                data_updates,
                reason,
                reload,
            } => f
                .debug_struct("UpdateAndAbort")
                .field(
                    "update",
                    &format_args!(
                        "title={:?}, unique_id={:?}, data={}",
                        update.title,
                        update.unique_id,
                        update.data.is_some() || data_updates.is_some()
                    ),
                )
                .field("reason", reason)
                .field("reload", reload)
                .finish(),
            Self::Abort { reason } => f.debug_struct("Abort").field("reason", reason).finish(),
        }
    }
}

/// A committed config-entry lifecycle change relevant to native integrations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigLifecycleEvent {
    EntryRemoved {
        entry_id: String,
        domain: String,
    },
    EntryDisabledByChanged {
        entry_id: String,
        disabled_by: Option<String>,
    },
    /// Refresh derived device/entity disablers before enable setup or after
    /// disable unload. Observers read the entry's current committed state.
    EntryRegistryDisabledByChanged {
        entry_id: String,
    },
    SubentryRemoved {
        entry_id: String,
        subentry_id: String,
    },
}

/// Owned observer for committed config-entry and subentry lifecycle changes.
///
/// Implementations run after persistence and outside config-manager locks. An
/// error is reported to the initiating caller after commit and cannot undo the
/// config change.
pub trait ConfigLifecycleObserver: Send + Sync {
    fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()>;
}

/// Minimal private notification emitted after a keyed entry is removed.
#[derive(Clone, Debug)]
pub struct DiscoveryKeyRemoval {
    pub entry_id: String,
    pub keys: Vec<DiscoveryKey>,
}

/// Receives live entry-removal notifications for native discovery providers.
///
/// Observer failures are isolated: removal is committed before this background
/// notification and cannot be rolled back by a consumer.
pub trait DiscoveryKeyRemovalObserver: Send + Sync {
    fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> Result<()>;
}

fn deserialize_subentries<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ConfigSubentry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredSubentries {
        List(Vec<ConfigSubentry>),
        LegacyMap(BTreeMap<String, ConfigSubentry>),
    }

    match StoredSubentries::deserialize(deserializer)? {
        StoredSubentries::List(entries) => Ok(entries),
        StoredSubentries::LegacyMap(entries) => {
            for (subentry_id, subentry) in &entries {
                if subentry_id != &subentry.subentry_id {
                    return Err(serde::de::Error::custom(
                        "stored subentry map key does not match subentry_id",
                    ));
                }
            }
            Ok(entries.into_values().collect())
        }
    }
}

/// Input for creating an entry from an import, user flow, or native setup.
#[derive(Clone)]
pub struct NewConfigEntry {
    pub domain: String,
    pub title: String,
    pub source: String,
    pub unique_id: Option<String>,
    pub version: u32,
    pub minor_version: u32,
    pub data: Map<String, Value>,
    pub options: Map<String, Value>,
}

impl NewConfigEntry {
    /// Create an entry seed with standard user-flow defaults.
    pub fn new(
        domain: impl Into<String>,
        title: impl Into<String>,
        data: Map<String, Value>,
    ) -> Self {
        Self {
            domain: domain.into(),
            title: title.into(),
            source: "user".into(),
            unique_id: None,
            version: 1,
            minor_version: 1,
            data,
            options: Map::new(),
        }
    }
}

/// Result of a driver setup attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupOutcome {
    /// Setup completed and the integration is ready.
    Ready,
    /// Setup is temporarily unavailable and can be retried after this delay.
    RetryAfter {
        seconds: u64,
        reason: Option<String>,
    },
}

/// Request passed to a native config or options flow handler.
#[derive(Clone)]
pub struct ConfigFlowRequest {
    pub flow_id: String,
    pub domain: String,
    pub step_id: String,
    pub source: String,
    pub entry_id: Option<String>,
    pub unique_id: Option<String>,
    pub user_input: Option<Map<String, Value>>,
    pub context: Map<String, Value>,
}

/// Flow manager that a parent flow can launch after creating its entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFlowType {
    Config,
    Options,
    ConfigSubentries,
}

impl ConfigFlowType {
    /// Name used by the config-flow continuation protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config_flow",
            Self::Options => "options_flow",
            Self::ConfigSubentries => "config_subentries_flow",
        }
    }
}

/// Public snapshot of one currently active native flow.
///
/// The result is the cached API fragment returned by [`ConfigEntries::flow_result`].
/// `flow_type` remains a Rust enum so adapters can validate a continuation
/// without guessing from a JSON `handler` value.
#[derive(Clone)]
pub struct ConfigFlowInfo {
    pub flow_type: ConfigFlowType,
    pub flow_id: String,
    pub domain: String,
    pub result: Value,
}

/// A valid next flow to attach to a parent config flow's create result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigFlowContinuation {
    pub flow_type: ConfigFlowType,
    pub flow_id: String,
}

impl ConfigFlowContinuation {
    /// Continue with a registered native config flow.
    pub fn config_flow(flow_id: impl Into<String>) -> Self {
        Self {
            flow_type: ConfigFlowType::Config,
            flow_id: flow_id.into(),
        }
    }

    /// Continue with a registered native options flow.
    pub fn options_flow(flow_id: impl Into<String>) -> Self {
        Self {
            flow_type: ConfigFlowType::Options,
            flow_id: flow_id.into(),
        }
    }

    /// Continue with a registered native config-subentry flow.
    pub fn config_subentries_flow(flow_id: impl Into<String>) -> Self {
        Self {
            flow_type: ConfigFlowType::ConfigSubentries,
            flow_id: flow_id.into(),
        }
    }
}

/// A native handler's response for a config or options flow.
#[derive(Clone)]
pub enum ConfigFlowResult {
    /// Ask the client to render a form and return the input to this step.
    Form {
        step_id: String,
        data_schema: Option<Value>,
        errors: Option<Map<String, Value>>,
        description_placeholders: Option<Map<String, Value>>,
    },
    /// Redirect the client to an external authorization or confirmation URL.
    ExternalStep {
        step_id: String,
        url: String,
        description_placeholders: Option<Map<String, Value>>,
    },
    /// Wait for the client to refresh the flow and run the named completion step.
    ExternalDone { step_id: String },
    /// Complete a config-entry creation, reconfigure, reauth, or options flow.
    CreateEntry {
        title: String,
        data: Map<String, Value>,
        options: Map<String, Value>,
        unique_id: Option<String>,
        version: u32,
        minor_version: u32,
    },
    /// Finish the flow without changing an entry.
    Abort { reason: String },
}

impl fmt::Debug for ConfigFlowResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Form { step_id, .. } => f
                .debug_struct("Form")
                .field("step_id", step_id)
                .finish_non_exhaustive(),
            Self::ExternalStep { step_id, .. } => f
                .debug_struct("ExternalStep")
                .field("step_id", step_id)
                .field("url", &"[REDACTED]")
                .finish_non_exhaustive(),
            Self::ExternalDone { step_id } => f
                .debug_struct("ExternalDone")
                .field("step_id", step_id)
                .finish(),
            Self::CreateEntry {
                title,
                unique_id,
                version,
                minor_version,
                ..
            } => f
                .debug_struct("CreateEntry")
                .field("title", title)
                .field("data", &"[REDACTED]")
                .field("options", &"[REDACTED]")
                .field("unique_id", unique_id)
                .field("version", version)
                .field("minor_version", minor_version)
                .finish(),
            Self::Abort { reason } => f.debug_struct("Abort").field("reason", reason).finish(),
        }
    }
}

impl ConfigFlowResult {
    /// Return a form result with no errors or placeholders.
    pub fn form(step_id: impl Into<String>, data_schema: Option<Value>) -> Self {
        Self::Form {
            step_id: step_id.into(),
            data_schema,
            errors: None,
            description_placeholders: None,
        }
    }

    /// Return a successful entry result with the default initial versions.
    pub fn create_entry(
        title: impl Into<String>,
        data: Map<String, Value>,
        unique_id: Option<String>,
    ) -> Self {
        Self::CreateEntry {
            title: title.into(),
            data,
            options: Map::new(),
            unique_id,
            version: 1,
            minor_version: 1,
        }
    }
}

/// Opaque, single-use capability for one external-flow transition.
///
/// A lease is bound to the exact registered driver instance, flow ID, step,
/// and in-memory flow generation. It intentionally contains no provider code,
/// state proof, or token data and cannot be serialized by consumers.
pub struct ExternalFlowLease {
    flow_id: String,
    domain: String,
    step_id: String,
    revision: u64,
    driver_generation: u64,
    driver: Weak<dyn ConfigEntryDriver>,
}

impl Clone for ExternalFlowLease {
    fn clone(&self) -> Self {
        Self {
            flow_id: self.flow_id.clone(),
            domain: self.domain.clone(),
            step_id: self.step_id.clone(),
            revision: self.revision,
            driver_generation: self.driver_generation,
            driver: self.driver.clone(),
        }
    }
}

impl fmt::Debug for ExternalFlowLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalFlowLease")
            .field("flow_id", &self.flow_id)
            .field("domain", &self.domain)
            .field("step_id", &self.step_id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// Opaque, already-persisted flow completion whose callbacks must be deferred.
///
/// `commit_external_flow` returns this value so OAuth callers can release
/// their account lock before `finish_external_flow` runs setup, reload, or
/// `on_create_entry` callbacks. Its Debug output never includes entry data.
pub struct DeferredExternalFlowCompletion {
    request: ConfigFlowRequest,
    kind: FlowKind,
    result: ConfigFlowResult,
    entry: Option<ConfigEntry>,
    driver: Arc<dyn ConfigEntryDriver>,
    action: DeferredFlowAction,
}

impl fmt::Debug for DeferredExternalFlowCompletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeferredExternalFlowCompletion")
            .field("flow_id", &self.request.flow_id)
            .field("domain", &self.request.domain)
            .field("source", &self.request.source)
            .field("action", &self.action)
            .field("entry", &self.entry.as_ref().map(|entry| &entry.entry_id))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
enum DeferredFlowAction {
    Create,
    Reconfigure,
    Reauth,
    Options,
    Abort,
}

/// Synchronous native integration callbacks.
///
/// Callbacks run outside config-entry locks. Async integrations can capture a
/// Tokio handle and block on it from the adapter's `spawn_blocking` context.
pub trait ConfigEntryDriver: Send + Sync {
    /// Integration domain (for example, `mqtt` or `my_sensor`).
    fn domain(&self) -> &str;

    /// HA integration type used by `type_filter`.
    fn integration_type(&self) -> &str {
        "hub"
    }

    /// Whether this integration implements a user options flow.
    fn supports_options(&self) -> bool {
        false
    }

    /// Whether this integration implements a device-removal callback.
    fn supports_remove_device(&self) -> bool {
        false
    }

    /// Whether this integration supports reconfiguration without removal.
    fn supports_reconfigure(&self) -> bool {
        false
    }

    /// Whether the integration can unload its runtime resources.
    fn supports_unload(&self) -> bool {
        true
    }

    /// Native integration's current entry schema version.
    fn version(&self) -> (u32, u32) {
        (1, 1)
    }

    /// Execute real setup logic for this entry.
    fn setup(&self, runtime: &Runtime, entry: &ConfigEntry) -> Result<SetupOutcome>;

    /// Execute real unload logic and release integration resources.
    fn unload(&self, runtime: &Runtime, entry: &ConfigEntry) -> Result<()>;

    /// Execute one config-flow step.
    fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult>;

    /// Optionally execute one config-flow step asynchronously.
    ///
    /// The returned future must own everything it needs and be `'static` so
    /// the manager can retain the work after an HTTP caller disconnects. A
    /// driver returning `None` keeps the existing synchronous callback, which
    /// the asynchronous manager wrappers run on Tokio's blocking pool.
    fn config_flow_async(
        &self,
        _request: ConfigFlowRequest,
    ) -> Option<Pin<Box<dyn Future<Output = Result<ConfigFlowResult>> + Send + 'static>>> {
        None
    }

    /// Run after a new entry has been persisted and setup attempted. A driver
    /// can use the manager to begin a related config, options, or subentry flow
    /// and return that flow as the parent's `next_flow` result.
    fn on_create_entry(
        &self,
        _manager: &ConfigEntries,
        _request: &ConfigFlowRequest,
        _entry: &ConfigEntry,
        _result: &ConfigFlowResult,
    ) -> Result<Option<ConfigFlowContinuation>> {
        Ok(None)
    }

    /// Supported child-configuration flow types for this entry. The returned
    /// values are exposed in the parent's redacted API fragment.
    fn supported_subentry_types(
        &self,
        _entry: &ConfigEntry,
    ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
        BTreeMap::new()
    }

    /// Execute one native subentry-flow step for a registered type.
    fn subentry_flow(
        &self,
        _request: &ConfigSubentryFlowRequest,
    ) -> Result<ConfigSubentryFlowResult> {
        Err(Error::InvalidInput(format!(
            "integration {} does not implement subentry flows",
            self.domain()
        )))
    }

    /// Execute one options-flow step. Integrations without options may retain
    /// this default, which fails explicitly if invoked.
    fn options_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        Err(Error::InvalidInput(format!(
            "integration {} does not implement an options flow",
            self.domain()
        )))
    }

    /// Migrate persisted entry data to the requested schema version.
    fn migrate(
        &self,
        _entry: &ConfigEntry,
        target_version: u32,
        target_minor_version: u32,
    ) -> Result<Map<String, Value>> {
        Err(Error::InvalidInput(format!(
            "integration {} has no migration from the current version to {target_version}.{target_minor_version}",
            self.domain()
        )))
    }

    /// Remove integration-owned resources after a successful unload.
    fn remove(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
        Ok(())
    }
}

/// Unit system exposed by the core config API.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSystem {
    pub name: String,
    pub length: String,
    pub mass: String,
    pub temperature: String,
    pub volume: String,
    pub pressure: String,
    pub wind_speed: String,
    pub accumulated_precipitation: String,
}

impl UnitSystem {
    fn metric() -> Self {
        Self {
            name: "metric".into(),
            length: "km".into(),
            mass: "g".into(),
            temperature: "°C".into(),
            volume: "L".into(),
            pressure: "Pa".into(),
            wind_speed: "m/s".into(),
            accumulated_precipitation: "mm".into(),
        }
    }

    fn imperial() -> Self {
        Self {
            name: "imperial".into(),
            length: "mi".into(),
            mass: "lb".into(),
            temperature: "°F".into(),
            volume: "gal".into(),
            pressure: "psi".into(),
            wind_speed: "mph".into(),
            accumulated_precipitation: "in".into(),
        }
    }

    fn validate(&self) -> Result<()> {
        let expected = match self.name.as_str() {
            "metric" => Self::metric(),
            "imperial" => Self::imperial(),
            _ => {
                return Err(Error::InvalidInput(
                    "unit_system.name must be metric or imperial".into(),
                ));
            }
        };
        if self != &expected {
            return Err(Error::InvalidInput(
                "unit_system fields do not match the selected system".into(),
            ));
        }
        Ok(())
    }
}

/// Persisted and validated subset of Home Assistant core configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    pub location_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
    pub time_zone: String,
    pub unit_system: UnitSystem,
    pub external_url: Option<String>,
    pub internal_url: Option<String>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            location_name: "Home".into(),
            latitude: 0.0,
            longitude: 0.0,
            elevation: 0.0,
            time_zone: "UTC".into(),
            unit_system: UnitSystem::metric(),
            external_url: None,
            internal_url: None,
        }
    }
}

impl CoreConfig {
    /// Validate core location, timezone, units, and URLs.
    pub fn validate(&self) -> Result<()> {
        if self.location_name.trim().is_empty() || self.location_name.chars().count() > 255 {
            return Err(Error::InvalidInput(
                "location_name must contain between 1 and 255 characters".into(),
            ));
        }
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            return Err(Error::InvalidInput(
                "latitude must be finite and between -90 and 90".into(),
            ));
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            return Err(Error::InvalidInput(
                "longitude must be finite and between -180 and 180".into(),
            ));
        }
        if !self.elevation.is_finite() {
            return Err(Error::InvalidInput("elevation must be finite".into()));
        }
        self.time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| Error::InvalidInput("unknown IANA time zone".into()))?;
        self.unit_system.validate()?;
        validate_optional_url("external_url", self.external_url.as_deref())?;
        validate_optional_url("internal_url", self.internal_url.as_deref())?;
        Ok(())
    }

    /// Return the public API config object.
    pub fn to_json(&self) -> Value {
        json!({
            "location_name": self.location_name,
            "latitude": self.latitude,
            "longitude": self.longitude,
            "elevation": self.elevation,
            "time_zone": self.time_zone,
            "unit_system": self.unit_system,
            "external_url": self.external_url,
            "internal_url": self.internal_url,
        })
    }
}

fn validate_optional_url(field: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let valid_scheme = value.starts_with("http://") || value.starts_with("https://");
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    if !valid_scheme || authority.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(Error::InvalidInput(format!(
            "{field} must be an absolute HTTP or HTTPS URL"
        )));
    }
    Ok(())
}

#[derive(Clone)]
struct FlowSession {
    request: ConfigFlowRequest,
    kind: FlowKind,
    initial_data: Option<Map<String, Value>>,
    data_kind: Option<DiscoveryDataKind>,
    result: Value,
    phase: FlowPhase,
    revision: u64,
    driver_generation: u64,
    driver: Weak<dyn ConfigEntryDriver>,
}

struct FlowStepGuard {
    lease: Option<ExternalFlowLease>,
    backup: Option<FlowSession>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowPhase {
    Ready,
    Running,
    AwaitingExternalCallback,
    ResolvingExternalCallback,
    ExternalDone,
    ResolvingExternalDone,
}

#[derive(Clone)]
struct AsyncFlowTask {
    revision: u64,
    result: watch::Receiver<Option<Result<Value>>>,
    cancel: watch::Sender<bool>,
    join: Arc<OwnedFlowTaskJoin>,
}

struct OwnedFlowTaskJoin {
    handle: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    result: Mutex<Option<std::result::Result<(), String>>>,
}

impl OwnedFlowTaskJoin {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            handle: AsyncMutex::new(Some(handle)),
            result: Mutex::new(None),
        }
    }

    async fn join(&self) -> std::result::Result<(), String> {
        let mut handle = self.handle.lock().await;
        if let Some(task) = handle.as_mut() {
            let result = task.await.map_err(|error| error.to_string());
            *self.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(result.clone());
            *handle = None;
            result
        } else {
            self.result
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .unwrap_or(Ok(()))
        }
    }
}

type FlowTaskShutdownResult = std::result::Result<usize, String>;

#[derive(Clone)]
struct FlowTaskShutdown {
    result: watch::Receiver<Option<FlowTaskShutdownResult>>,
    join: Arc<OwnedFlowTaskJoin>,
}

struct FlowTaskControlState {
    closed: bool,
    admissions: usize,
    tasks: HashMap<String, AsyncFlowTask>,
    shutdown_result: Option<FlowTaskShutdown>,
}

struct FlowTaskControl {
    state: Mutex<FlowTaskControlState>,
    changed: Notify,
}

impl FlowTaskControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(FlowTaskControlState {
                closed: false,
                admissions: 0,
                tasks: HashMap::new(),
                shutdown_result: None,
            }),
            changed: Notify::new(),
        }
    }
}

struct FlowAdmissionGuard {
    control: Arc<FlowTaskControl>,
}

impl Drop for FlowAdmissionGuard {
    fn drop(&mut self) {
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.admissions = state.admissions.saturating_sub(1);
        drop(state);
        self.control.changed.notify_one();
    }
}

#[derive(Clone)]
struct SubentryFlowLease {
    flow_id: String,
    domain: String,
    step_id: String,
    revision: u64,
    driver_generation: u64,
    driver: Weak<dyn ConfigEntryDriver>,
}

struct AbortOnDropJoin<T>(tokio::task::JoinHandle<T>);

impl<T> AbortOnDropJoin<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(handle)
    }
}

impl<T> Unpin for AbortOnDropJoin<T> {}

impl<T> Future for AbortOnDropJoin<T> {
    type Output = std::result::Result<T, tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(context)
    }
}

impl<T> Drop for AbortOnDropJoin<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
struct RegisteredDrivers {
    drivers: HashMap<String, RegisteredDriver>,
    next_generation: u64,
}

#[derive(Clone)]
struct RegisteredDriver {
    driver: Arc<dyn ConfigEntryDriver>,
    generation: u64,
}

fn external_lease(flow: &FlowSession) -> ExternalFlowLease {
    ExternalFlowLease {
        flow_id: flow.request.flow_id.clone(),
        domain: flow.request.domain.clone(),
        step_id: flow.request.step_id.clone(),
        revision: flow.revision,
        driver_generation: flow.driver_generation,
        driver: flow.driver.clone(),
    }
}

fn subentry_flow_lease(flow: &ConfigSubentryFlowSession) -> SubentryFlowLease {
    SubentryFlowLease {
        flow_id: flow.request.flow_id.clone(),
        domain: flow.request.domain.clone(),
        step_id: flow.request.step_id.clone(),
        revision: flow.revision,
        driver_generation: flow.driver_generation,
        driver: flow.driver.clone(),
    }
}

fn ensure_subentry_flow_matches_lease(
    flow: &ConfigSubentryFlowSession,
    lease: &SubentryFlowLease,
    phase: FlowPhase,
) -> Result<()> {
    let same_driver = flow
        .driver
        .upgrade()
        .zip(lease.driver.upgrade())
        .is_some_and(|(flow_driver, lease_driver)| Arc::ptr_eq(&flow_driver, &lease_driver));
    if flow.request.flow_id != lease.flow_id
        || flow.request.domain != lease.domain
        || flow.request.step_id != lease.step_id
        || flow.revision != lease.revision
        || flow.driver_generation != lease.driver_generation
        || flow.phase != phase
        || !same_driver
    {
        return Err(Error::NotFound(
            "config subentry flow lease is stale".into(),
        ));
    }
    Ok(())
}

fn ensure_flow_driver(
    flow: &FlowSession,
    registration: &RegisteredDriver,
    expected_domain: &str,
) -> Result<()> {
    let same_driver = flow
        .driver
        .upgrade()
        .is_some_and(|driver| Arc::ptr_eq(&driver, &registration.driver));
    if flow.request.domain != expected_domain
        || flow.driver_generation != registration.generation
        || !same_driver
    {
        return Err(Error::NotFound(
            "external flow driver registration changed".into(),
        ));
    }
    Ok(())
}

fn ensure_lease_driver(registrations: &RegisteredDrivers, lease: &ExternalFlowLease) -> Result<()> {
    let registration = registrations
        .drivers
        .get(&lease.domain)
        .ok_or_else(|| Error::NotFound("external flow driver is not registered".into()))?;
    let same_driver = lease
        .driver
        .upgrade()
        .is_some_and(|driver| Arc::ptr_eq(&driver, &registration.driver));
    if registration.generation != lease.driver_generation || !same_driver {
        return Err(Error::NotFound(
            "external flow driver registration changed".into(),
        ));
    }
    Ok(())
}

fn ensure_flow_matches_lease(
    flow: &FlowSession,
    lease: &ExternalFlowLease,
    phase: FlowPhase,
) -> Result<()> {
    let same_driver = flow
        .driver
        .upgrade()
        .zip(lease.driver.upgrade())
        .is_some_and(|(flow_driver, lease_driver)| Arc::ptr_eq(&flow_driver, &lease_driver));
    if flow.request.flow_id != lease.flow_id
        || flow.request.domain != lease.domain
        || flow.request.step_id != lease.step_id
        || flow.revision != lease.revision
        || flow.driver_generation != lease.driver_generation
        || flow.phase != phase
        || !same_driver
    {
        return Err(Error::NotFound("external flow lease is stale".into()));
    }
    Ok(())
}

fn validate_flow_step(step_id: &str) -> Result<()> {
    if step_id.is_empty()
        || step_id.len() > 128
        || !step_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(Error::InvalidInput("invalid config flow step ID".into()));
    }
    Ok(())
}

fn new_entry_from_seed(
    seed: NewConfigEntry,
    entry_id: String,
    _driver: &Arc<dyn ConfigEntryDriver>,
    discovery_key: Option<DiscoveryKey>,
) -> ConfigEntry {
    let now = Utc::now();
    let mut entry = ConfigEntry {
        entry_id,
        domain: seed.domain,
        title: seed.title.trim().to_owned(),
        source: seed.source,
        unique_id: seed.unique_id,
        discovery_keys: BTreeMap::new(),
        version: seed.version,
        minor_version: seed.minor_version,
        data: seed.data,
        options: seed.options,
        subentries: Vec::new(),
        state: ConfigEntryState::NotLoaded,
        reason: None,
        retry_attempts: 0,
        retry_at: None,
        disabled_by: None,
        needs_reauth: false,
        require_reload: false,
        pref_disable_new_entities: false,
        pref_disable_polling: false,
        error_reason_translation_domain: None,
        error_reason_translation_key: None,
        error_reason_translation_placeholders: None,
        supports_options: false,
        supports_remove_device: false,
        supports_unload: false,
        supports_reconfigure: false,
        supported_subentry_types: Map::new(),
        created_at: now,
        modified_at: now,
        integration_type: default_integration_type(),
    };
    if let Some(key) = discovery_key {
        record_discovery_key(&mut entry, key);
    }
    entry
}

fn refresh_entry_capabilities(
    manager: &ConfigEntries,
    entry_id: &str,
    driver: &Arc<dyn ConfigEntryDriver>,
) -> Result<ConfigEntry> {
    let snapshot = manager.require_entry(entry_id)?;
    let integration_type = driver.integration_type().to_owned();
    let supports_options = driver.supports_options();
    let supports_remove_device = driver.supports_remove_device();
    let supports_unload = driver.supports_unload();
    let supports_reconfigure = driver.supports_reconfigure();
    let supported_subentry_types =
        supported_subentry_types_json(driver.supported_subentry_types(&snapshot));
    manager.mutate(|state| {
        let entry = state
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
        if entry.domain != snapshot.domain {
            return Err(Error::InvalidInput("config entry domain changed".into()));
        }
        entry.integration_type = integration_type;
        entry.supports_options = supports_options;
        entry.supports_remove_device = supports_remove_device;
        entry.supports_unload = supports_unload;
        entry.supports_reconfigure = supports_reconfigure;
        entry.supported_subentry_types = supported_subentry_types;
        Ok(entry.clone())
    })
}

#[derive(Clone)]
struct ConfigSubentryFlowSession {
    request: ConfigSubentryFlowRequest,
    result: Value,
    phase: FlowPhase,
    revision: u64,
    driver_generation: u64,
    driver: Weak<dyn ConfigEntryDriver>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FlowKind {
    Config,
    Options,
}

#[derive(Clone)]
struct PersistedState {
    entries: BTreeMap<String, ConfigEntry>,
    core_config: CoreConfig,
    core_config_initialized: bool,
    pending_cleanup: Vec<PendingCleanup>,
    flows: BTreeMap<String, FlowSession>,
    subentry_flows: BTreeMap<String, ConfigSubentryFlowSession>,
}

/// The deliberately small, non-secret identity needed to retry registry cleanup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PendingCleanupEvent {
    EntryRemoved {
        entry_id: String,
        domain: String,
    },
    SubentryRemoved {
        entry_id: String,
        subentry_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingCleanup {
    id: Uuid,
    event: PendingCleanupEvent,
}

impl PendingCleanup {
    fn lifecycle_event(&self) -> ConfigLifecycleEvent {
        match &self.event {
            PendingCleanupEvent::EntryRemoved { entry_id, domain } => {
                ConfigLifecycleEvent::EntryRemoved {
                    entry_id: entry_id.clone(),
                    domain: domain.clone(),
                }
            }
            PendingCleanupEvent::SubentryRemoved {
                entry_id,
                subentry_id,
            } => ConfigLifecycleEvent::SubentryRemoved {
                entry_id: entry_id.clone(),
                subentry_id: subentry_id.clone(),
            },
        }
    }
}

enum CleanupDispatch {
    Acknowledged,
    Pending,
    InFlight,
}

struct CleanupFlightGuard<'a> {
    manager: &'a ConfigEntries,
    id: Uuid,
}

impl Drop for CleanupFlightGuard<'_> {
    fn drop(&mut self) {
        self.manager
            .cleanup_in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

fn queue_pending_cleanup(state: &mut PersistedState, event: PendingCleanupEvent) -> PendingCleanup {
    if let PendingCleanupEvent::EntryRemoved { entry_id, .. } = &event {
        // A parent cleanup covers every child, so obsolete child records do not
        // outlive the parent that now owns their cleanup scope.
        state.pending_cleanup.retain(|pending| {
            !matches!(
                &pending.event,
                PendingCleanupEvent::SubentryRemoved {
                    entry_id: pending_entry_id,
                    ..
                } if pending_entry_id == entry_id
            )
        });
    }
    let pending = PendingCleanup {
        id: Uuid::new_v4(),
        event,
    };
    state.pending_cleanup.push(pending.clone());
    pending
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedDocument {
    document_version: u32,
    entries: Vec<ConfigEntry>,
    core_config: CoreConfig,
    #[serde(default)]
    core_config_initialized: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_cleanup: Vec<PendingCleanup>,
}

/// Persistent config-entry, config-flow, and core-config manager.
#[derive(Clone)]
pub struct ConfigEntries {
    runtime: Runtime,
    state: Arc<Mutex<PersistedState>>,
    drivers: Arc<RwLock<RegisteredDrivers>>,
    flow_tasks: Arc<FlowTaskControl>,
    flow_transitions: Arc<Mutex<()>>,
    operation_lock: Arc<Mutex<()>>,
    lifecycle_observers: Arc<RwLock<Vec<Arc<dyn ConfigLifecycleObserver>>>>,
    discovery_key_removal_observers: Arc<RwLock<Vec<Arc<dyn DiscoveryKeyRemovalObserver>>>>,
    cleanup_in_flight: Arc<Mutex<HashSet<Uuid>>>,
}

/// Owned registration for one config lifecycle observer.
///
/// Dropping the guard unregisters only the observer instance it installed.
pub struct ConfigLifecycleObserverGuard {
    manager: ConfigEntries,
    observer: Arc<dyn ConfigLifecycleObserver>,
}

impl Drop for ConfigLifecycleObserverGuard {
    fn drop(&mut self) {
        let _ = self.manager.unregister_lifecycle_observer(&self.observer);
    }
}

/// Owned registration for one discovery-key-removal observer.
///
/// Dropping the guard unregisters only the observer instance it installed.
pub struct DiscoveryKeyRemovalObserverGuard {
    manager: ConfigEntries,
    observer: Arc<dyn DiscoveryKeyRemovalObserver>,
}

impl Drop for DiscoveryKeyRemovalObserverGuard {
    fn drop(&mut self) {
        let _ = self
            .manager
            .unregister_discovery_key_removal_observer(&self.observer);
    }
}

impl ConfigEntries {
    /// Load the config-entry and core-config document from runtime metadata.
    pub fn new(runtime: Runtime) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut core_config = CoreConfig::default();
        let mut core_config_initialized = false;
        let mut pending_cleanup = Vec::new();
        if let Some(value) = runtime.get_data(METADATA_KEY)? {
            let document: PersistedDocument = serde_json::from_value(value).map_err(|error| {
                Error::Storage(format!("invalid persisted config document: {error}"))
            })?;
            if document.document_version != DOCUMENT_VERSION {
                return Err(Error::Storage(format!(
                    "unsupported config document version {}",
                    document.document_version
                )));
            }
            core_config = document.core_config;
            core_config_initialized = document.core_config_initialized;
            pending_cleanup = document.pending_cleanup;
            core_config
                .validate()
                .map_err(|error| Error::Storage(format!("invalid stored core config: {error}")))?;
            for mut entry in document.entries {
                validate_entry(&entry)
                    .map_err(|error| Error::Storage(format!("invalid stored entry: {error}")))?;
                if matches!(
                    entry.state,
                    ConfigEntryState::Loaded
                        | ConfigEntryState::SetupInProgress
                        | ConfigEntryState::UnloadInProgress
                ) {
                    // A process restart loses the integration's live handles.
                    entry.state = ConfigEntryState::NotLoaded;
                    entry.reason = None;
                    entry.retry_at = None;
                    entry.require_reload = true;
                }
                if entries.insert(entry.entry_id.clone(), entry).is_some() {
                    return Err(Error::Storage(
                        "duplicate config entry id in persisted document".into(),
                    ));
                }
            }
            validate_unique_ids(entries.values())
                .map_err(|error| Error::Storage(format!("invalid stored unique id: {error}")))?;
        }
        let manager = Self {
            runtime,
            state: Arc::new(Mutex::new(PersistedState {
                entries,
                core_config,
                core_config_initialized,
                pending_cleanup,
                flows: BTreeMap::new(),
                subentry_flows: BTreeMap::new(),
            })),
            drivers: Arc::new(RwLock::new(RegisteredDrivers::default())),
            flow_tasks: Arc::new(FlowTaskControl::new()),
            flow_transitions: Arc::new(Mutex::new(())),
            operation_lock: Arc::new(Mutex::new(())),
            lifecycle_observers: Arc::new(RwLock::new(Vec::new())),
            discovery_key_removal_observers: Arc::new(RwLock::new(Vec::new())),
            cleanup_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        // Persist any restart state normalization before returning.
        {
            let state = manager.lock_state();
            manager.persist(&state)?;
        }
        manager.apply_runtime_core_config(&manager.core_config())?;
        Ok(manager)
    }

    /// Register a native implementation for one integration domain.
    pub fn register_driver(&self, driver: Arc<dyn ConfigEntryDriver>) -> Result<()> {
        validate_domain(driver.domain())?;
        if driver.version().0 == 0 {
            return Err(Error::InvalidInput(
                "integration version must be at least 1".into(),
            ));
        }
        let domain = driver.domain().to_owned();
        let mut drivers = self.drivers.write().unwrap_or_else(PoisonError::into_inner);
        if drivers.drivers.contains_key(&domain) {
            return Err(Error::InvalidInput(format!(
                "integration driver already registered for {domain}"
            )));
        }
        drivers.next_generation = drivers
            .next_generation
            .checked_add(1)
            .ok_or_else(|| Error::Service("driver registration generation exhausted".into()))?;
        let generation = drivers.next_generation;
        drivers.drivers.insert(
            domain.clone(),
            RegisteredDriver {
                driver: Arc::clone(&driver),
                generation,
            },
        );
        drop(drivers);
        let supported_by_entry = self
            .list_entries(Some(&domain))
            .into_iter()
            .map(|entry| (entry.entry_id, entry.supported_subentry_types))
            .collect::<HashMap<_, _>>();
        if let Err(error) = self.mutate(|state| {
            for entry in state
                .entries
                .values_mut()
                .filter(|entry| entry.domain == domain)
            {
                entry.integration_type = driver.integration_type().to_owned();
                entry.supports_options = driver.supports_options();
                entry.supports_remove_device = driver.supports_remove_device();
                entry.supports_unload = driver.supports_unload();
                entry.supports_reconfigure = driver.supports_reconfigure();
                entry.supported_subentry_types = supported_by_entry
                    .get(&entry.entry_id)
                    .cloned()
                    .unwrap_or_default();
            }
            Ok(())
        }) {
            self.drivers
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .drivers
                .remove(&domain);
            return Err(error);
        }
        Ok(())
    }

    /// Unregister only this exact driver instance. Hosts must stop its work
    /// and unload its entries first; this does not run unload callbacks.
    /// A stale host cannot remove a later driver's registration.
    pub fn unregister_driver(&self, expected: &Arc<dyn ConfigEntryDriver>) -> bool {
        let domain = expected.domain().to_owned();
        let mut drivers = self.drivers.write().unwrap_or_else(PoisonError::into_inner);
        if drivers
            .drivers
            .get(&domain)
            .is_some_and(|registered| Arc::ptr_eq(&registered.driver, expected))
        {
            drivers.drivers.remove(&domain);
            true
        } else {
            false
        }
    }

    /// Register one owned lifecycle observer.
    ///
    /// Registration is pointer-owned: duplicate registration is rejected and
    /// `unregister_lifecycle_observer` only removes the exact same `Arc`.
    pub fn register_lifecycle_observer(
        &self,
        observer: Arc<dyn ConfigLifecycleObserver>,
    ) -> Result<()> {
        let mut observers = self
            .lifecycle_observers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if observers
            .iter()
            .any(|registered| Arc::ptr_eq(registered, &observer))
        {
            return Err(Error::InvalidInput(
                "config lifecycle observer is already registered".into(),
            ));
        }
        observers.push(observer);
        Ok(())
    }

    /// Register an observer with an owned guard that unregisters on drop.
    pub fn observe_lifecycle(
        &self,
        observer: Arc<dyn ConfigLifecycleObserver>,
    ) -> Result<ConfigLifecycleObserverGuard> {
        self.register_lifecycle_observer(Arc::clone(&observer))?;
        Ok(ConfigLifecycleObserverGuard {
            manager: self.clone(),
            observer,
        })
    }

    /// Unregister only this exact observer instance.
    pub fn unregister_lifecycle_observer(
        &self,
        expected: &Arc<dyn ConfigLifecycleObserver>,
    ) -> bool {
        let mut observers = self
            .lifecycle_observers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(index) = observers
            .iter()
            .position(|registered| Arc::ptr_eq(registered, expected))
        else {
            return false;
        };
        observers.remove(index);
        true
    }

    /// Register a private discovery-key-removal observer.
    ///
    /// Registration is pointer-owned: duplicate registration is rejected and
    /// `unregister_discovery_key_removal_observer` only removes the exact same
    /// `Arc`.
    pub fn register_discovery_key_removal_observer(
        &self,
        observer: Arc<dyn DiscoveryKeyRemovalObserver>,
    ) -> Result<()> {
        let mut observers = self
            .discovery_key_removal_observers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if observers
            .iter()
            .any(|registered| Arc::ptr_eq(registered, &observer))
        {
            return Err(Error::InvalidInput(
                "discovery key removal observer is already registered".into(),
            ));
        }
        observers.push(observer);
        Ok(())
    }

    /// Register an observer with an owned guard that unregisters on drop.
    pub fn observe_discovery_key_removals(
        &self,
        observer: Arc<dyn DiscoveryKeyRemovalObserver>,
    ) -> Result<DiscoveryKeyRemovalObserverGuard> {
        self.register_discovery_key_removal_observer(Arc::clone(&observer))?;
        Ok(DiscoveryKeyRemovalObserverGuard {
            manager: self.clone(),
            observer,
        })
    }

    /// Unregister only this exact discovery-key-removal observer instance.
    pub fn unregister_discovery_key_removal_observer(
        &self,
        expected: &Arc<dyn DiscoveryKeyRemovalObserver>,
    ) -> bool {
        let mut observers = self
            .discovery_key_removal_observers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(index) = observers
            .iter()
            .position(|registered| Arc::ptr_eq(registered, expected))
        else {
            return false;
        };
        observers.remove(index);
        true
    }

    /// Retry durable parent and child cleanup notifications.
    ///
    /// Hosts call this after installing the cleanup observers that own their
    /// registries. Observer registration and manager construction do not
    /// replay or acknowledge pending records. This retries cleanup callbacks
    /// only; it does not replay public config-entry change events.
    pub fn replay_pending_cleanup(&self) -> Result<usize> {
        let pending = self.lock_state().pending_cleanup.clone();
        let mut acknowledged = 0;
        let mut errors = Vec::new();
        for record in pending {
            match self.dispatch_pending_cleanup(&record) {
                Ok(CleanupDispatch::Acknowledged) => acknowledged += 1,
                Ok(CleanupDispatch::Pending | CleanupDispatch::InFlight) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        if errors.is_empty() {
            Ok(acknowledged)
        } else {
            Err(Error::Service(format!(
                "pending config cleanup replay failed: {}",
                errors.join("; ")
            )))
        }
    }

    /// Return a registered integration driver by domain.
    pub fn driver(&self, domain: &str) -> Option<Arc<dyn ConfigEntryDriver>> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .drivers
            .get(domain)
            .map(|registration| Arc::clone(&registration.driver))
    }

    fn driver_registration(&self, domain: &str) -> Option<(Arc<dyn ConfigEntryDriver>, u64)> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .drivers
            .get(domain)
            .map(|registration| (Arc::clone(&registration.driver), registration.generation))
    }

    fn with_supported_subentry_types(&self, mut entry: ConfigEntry) -> ConfigEntry {
        if let Some(driver) = self.driver(&entry.domain) {
            entry.supported_subentry_types =
                supported_subentry_types_json(driver.supported_subentry_types(&entry));
        }
        entry
    }

    /// List only actual registered native flow handlers.
    pub fn flow_handlers(&self) -> Vec<String> {
        let mut handlers = self
            .drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .drivers
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        handlers.sort();
        handlers
    }

    /// Return the current public form without executing its driver again.
    pub fn flow_result(&self, flow_id: &str) -> Result<Value> {
        let state = self.lock_state();
        if let Some(flow) = state.flows.get(flow_id) {
            if flow.result.is_null() {
                return Err(Error::Service("config flow is still initializing".into()));
            }
            return Ok(flow.result.clone());
        }
        state
            .subentry_flows
            .get(flow_id)
            .map(|flow| flow.result.clone())
            .ok_or_else(|| Error::NotFound("unknown config flow".into()))
    }

    /// Return one active native flow only when its typed flow kind matches.
    ///
    /// The snapshot includes the cached result and owning integration domain.
    /// This is intended for adapters resolving a validated
    /// [`ConfigFlowContinuation`]; unknown, completed, or mismatched flows fail
    /// closed with `NotFound`.
    pub fn active_flow(&self, flow_type: ConfigFlowType, flow_id: &str) -> Result<ConfigFlowInfo> {
        let state = self.lock_state();
        let session = match flow_type {
            ConfigFlowType::Config => state
                .flows
                .get(flow_id)
                .filter(|flow| flow.kind == FlowKind::Config)
                .map(|flow| (flow.request.domain.clone(), flow.result.clone())),
            ConfigFlowType::Options => state
                .flows
                .get(flow_id)
                .filter(|flow| flow.kind == FlowKind::Options)
                .map(|flow| (flow.request.domain.clone(), flow.result.clone())),
            ConfigFlowType::ConfigSubentries => state
                .subentry_flows
                .get(flow_id)
                .map(|flow| (flow.request.domain.clone(), flow.result.clone())),
        }
        .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
        Ok(ConfigFlowInfo {
            flow_type,
            flow_id: flow_id.to_owned(),
            domain: session.0,
            result: session.1,
        })
    }

    pub fn abort_flow(&self, flow_id: &str) -> Result<()> {
        {
            let _transition = self
                .flow_transitions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.mutate(|state| {
                let removed_entry_flow = state.flows.remove(flow_id).is_some();
                let removed_subentry_flow = state.subentry_flows.remove(flow_id).is_some();
                if !removed_entry_flow && !removed_subentry_flow {
                    return Err(Error::NotFound("unknown config flow".into()));
                }
                Ok(())
            })?;
        }
        self.cancel_flow_task(flow_id);
        self.notify_flow_progress(flow_id)
    }

    /// Claim the callback transition from an active `external` result.
    ///
    /// Only the expected domain and exact current external step can claim it.
    /// Concurrent callbacks receive an error after the first claim changes the
    /// flow phase; callbacks must verify their OAuth proof before calling this.
    pub fn claim_external_callback(
        &self,
        flow_id: &str,
        expected_domain: &str,
        expected_step: &str,
    ) -> Result<ExternalFlowLease> {
        validate_domain(expected_domain)?;
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        let registration = registrations
            .drivers
            .get(expected_domain)
            .ok_or_else(|| Error::NotFound("external flow driver is not registered".into()))?;
        let lease = self.mutate(|state| {
            let flow = state
                .flows
                .get_mut(flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_driver(flow, registration, expected_domain)?;
            if flow.kind != FlowKind::Config
                || flow.phase != FlowPhase::AwaitingExternalCallback
                || flow.request.step_id != expected_step
                || flow.result.get("type").and_then(Value::as_str) != Some("external")
            {
                return Err(Error::InvalidInput(
                    "external callback does not match the active flow step".into(),
                ));
            }
            flow.phase = FlowPhase::ResolvingExternalCallback;
            flow.revision = flow
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
            Ok(external_lease(flow))
        })?;
        Ok(lease)
    }

    /// Publish the public `external_done` transition after a trusted callback
    /// has been privately stored by its OAuth owner.
    pub fn publish_external_done(
        &self,
        lease: ExternalFlowLease,
        step_id: impl Into<String>,
    ) -> Result<Value> {
        let step_id = step_id.into();
        validate_flow_step(&step_id)?;
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        ensure_lease_driver(&registrations, &lease)?;
        let result = self.mutate(|state| {
            let flow = state
                .flows
                .get_mut(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_matches_lease(flow, &lease, FlowPhase::ResolvingExternalCallback)?;
            let mut request = flow.request.clone();
            request.step_id = step_id.clone();
            request.user_input = None;
            let result = flow_result_json(
                &request,
                FlowKind::Config,
                &ConfigFlowResult::ExternalDone {
                    step_id: step_id.clone(),
                },
                None,
                None,
            );
            flow.request = request;
            flow.result = result.clone();
            flow.phase = FlowPhase::ExternalDone;
            flow.revision = flow
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
            Ok(result)
        })?;
        drop(registrations);
        self.notify_flow_progress(&lease.flow_id)?;
        Ok(result)
    }

    /// Claim an `external_done` creation/rejection step once. `None` means
    /// another concurrent GET already owns the transition.
    pub fn claim_external_done(
        &self,
        flow_id: &str,
        expected_domain: &str,
        expected_step: &str,
    ) -> Result<Option<ExternalFlowLease>> {
        validate_domain(expected_domain)?;
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        let registration = registrations
            .drivers
            .get(expected_domain)
            .ok_or_else(|| Error::NotFound("external flow driver is not registered".into()))?;
        self.mutate(|state| {
            let flow = state
                .flows
                .get_mut(flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_driver(flow, registration, expected_domain)?;
            if flow.kind != FlowKind::Config {
                return Err(Error::InvalidInput("external flow type mismatch".into()));
            }
            if flow.phase == FlowPhase::ResolvingExternalDone {
                return Ok(None);
            }
            if flow.phase != FlowPhase::ExternalDone
                || flow.request.step_id != expected_step
                || flow.result.get("type").and_then(Value::as_str) != Some("external_done")
            {
                return Err(Error::InvalidInput(
                    "external completion does not match the active flow step".into(),
                ));
            }
            flow.phase = FlowPhase::ResolvingExternalDone;
            flow.revision = flow
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
            Ok(Some(external_lease(flow)))
        })
    }

    /// Release a failed resolver claim so an owned host task can retry it.
    /// The lease must still be current; abort or removal makes release stale.
    pub fn release_external_flow(&self, lease: ExternalFlowLease) -> Result<()> {
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        ensure_lease_driver(&registrations, &lease)?;
        self.mutate(|state| {
            let flow = state
                .flows
                .get_mut(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            let next_phase = match flow.phase {
                FlowPhase::ResolvingExternalCallback => FlowPhase::AwaitingExternalCallback,
                FlowPhase::ResolvingExternalDone => FlowPhase::ExternalDone,
                _ => {
                    return Err(Error::InvalidInput(
                        "external flow lease is no longer resolving".into(),
                    ));
                }
            };
            ensure_flow_matches_lease(flow, &lease, flow.phase)?;
            flow.phase = next_phase;
            flow.revision = flow
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
            Ok(())
        })
    }

    /// Atomically commit a trusted external-flow `CreateEntry` or `Abort`.
    ///
    /// For reauth and reconfigure, `CreateEntry.data` is merged into the
    /// existing private data map while the original entry ID, identity,
    /// options, and preferences are retained. Runtime callbacks are returned
    /// as a deferred plan and run only after the caller releases its SDK lock.
    pub fn commit_external_flow(
        &self,
        lease: ExternalFlowLease,
        mut result: ConfigFlowResult,
    ) -> Result<DeferredExternalFlowCompletion> {
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        ensure_lease_driver(&registrations, &lease)?;
        let registration = registrations
            .drivers
            .get(&lease.domain)
            .ok_or_else(|| Error::NotFound("external flow driver is not registered".into()))?;
        let driver = Arc::clone(&registration.driver);
        let (request, entry, action) = self.mutate(|state| {
            let flow = state
                .flows
                .get(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_matches_lease(flow, &lease, FlowPhase::ResolvingExternalDone)?;
            if flow.kind != FlowKind::Config {
                return Err(Error::InvalidInput("external flow type mismatch".into()));
            }
            let request = flow.request.clone();
            let (entry, action) = match &result {
                ConfigFlowResult::Abort { .. } => (None, DeferredFlowAction::Abort),
                ConfigFlowResult::CreateEntry {
                    title,
                    data,
                    options,
                    unique_id,
                    version,
                    minor_version,
                } => match request.source.as_str() {
                    "reauth" | "reconfigure" => {
                        let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                            Error::InvalidInput(format!(
                                "{} flow is missing entry_id",
                                request.source
                            ))
                        })?;
                        let original = state
                            .entries
                            .get_mut(entry_id)
                            .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
                        if original.domain != request.domain {
                            return Err(Error::InvalidInput(
                                "external flow entry domain mismatch".into(),
                            ));
                        }
                        if request.source == "reauth" && !original.needs_reauth {
                            return Err(Error::InvalidInput(
                                "config entry is no longer awaiting reauthentication".into(),
                            ));
                        }
                        let _ = (title, options, unique_id, version, minor_version);
                        original.data.extend(data.clone());
                        original.needs_reauth = false;
                        original.modified_at = Utc::now();
                        let committed = original.clone();
                        (
                            Some(committed),
                            if request.source == "reauth" {
                                DeferredFlowAction::Reauth
                            } else {
                                DeferredFlowAction::Reconfigure
                            },
                        )
                    }
                    _ => {
                        let mut seed = NewConfigEntry::new(&request.domain, title, data.clone());
                        seed.source = request.source.clone();
                        seed.unique_id = unique_id.clone().or_else(|| request.unique_id.clone());
                        seed.version = *version;
                        seed.minor_version = *minor_version;
                        seed.options = options.clone();
                        validate_new_entry(&seed)?;
                        let entry = new_entry_from_seed(
                            seed,
                            Uuid::new_v4().to_string(),
                            &driver,
                            discovery_key_from_context(&request.context),
                        );
                        ensure_unique_id(
                            &state.entries,
                            &entry.domain,
                            entry.unique_id.as_deref(),
                            None,
                        )?;
                        state.entries.insert(entry.entry_id.clone(), entry.clone());
                        (Some(entry), DeferredFlowAction::Create)
                    }
                },
                _ => {
                    return Err(Error::InvalidInput(
                        "external flow commit accepts only CreateEntry or Abort".into(),
                    ));
                }
            };
            state.flows.remove(&lease.flow_id);
            Ok((request, entry, action))
        })?;
        drop(registrations);
        if matches!(
            action,
            DeferredFlowAction::Reauth | DeferredFlowAction::Reconfigure
        ) && let Some(entry) = entry.as_ref()
            && let ConfigFlowResult::CreateEntry {
                title,
                options,
                unique_id,
                version,
                minor_version,
                ..
            } = &mut result
        {
            *title = entry.title.clone();
            *options = entry.options.clone();
            *unique_id = entry.unique_id.clone();
            *version = entry.version;
            *minor_version = entry.minor_version;
        }
        Ok(DeferredExternalFlowCompletion {
            request,
            kind: FlowKind::Config,
            result,
            entry,
            driver,
            action,
        })
    }

    /// Run the setup/reload and entry-created callbacks deferred by
    /// `commit_external_flow`. Call this only after OAuth account locks have
    /// been released.
    pub fn finish_external_flow(
        &self,
        completion: DeferredExternalFlowCompletion,
    ) -> Result<Value> {
        let DeferredExternalFlowCompletion {
            request,
            kind,
            result,
            mut entry,
            driver,
            action,
        } = completion;
        let mut next_flow = None;
        // Entry/token persistence already committed. Observer delivery must not
        // consume this one-shot plan before its activation callbacks run.
        let mut notification_error = None;
        match action {
            DeferredFlowAction::Create => {
                let entry_id = entry
                    .as_ref()
                    .ok_or_else(|| Error::Service("created flow entry is missing".into()))?
                    .entry_id
                    .clone();
                let refreshed = refresh_entry_capabilities(self, &entry_id, &driver)?;
                notification_error = self.emit_public_entry_change("added", &refreshed).err();
                {
                    let _operation = self.lock_operation();
                    let _ = self.setup_entry_with_notifications(&entry_id, &mut notification_error);
                }
                let current = self.require_entry(&entry_id)?;
                next_flow = driver.on_create_entry(self, &request, &current, &result)?;
                if let Some(continuation) = next_flow.as_ref() {
                    self.validate_flow_continuation(continuation)?;
                }
                entry = Some(current);
            }
            DeferredFlowAction::Reauth | DeferredFlowAction::Reconfigure => {
                let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                    Error::InvalidInput("external flow is missing entry_id".into())
                })?;
                let current = entry
                    .as_ref()
                    .ok_or_else(|| Error::Service("updated flow entry is missing".into()))?;
                notification_error = self.emit_public_entry_change("updated", current).err();
                {
                    let _operation = self.lock_operation();
                    self.reload_entry_with_notifications(entry_id, &mut notification_error)?;
                }
                entry = Some(self.require_entry(entry_id)?);
            }
            DeferredFlowAction::Options => {
                let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                    Error::InvalidInput("options flow is missing entry_id".into())
                })?;
                let current = entry
                    .as_ref()
                    .ok_or_else(|| Error::Service("updated options entry is missing".into()))?;
                notification_error = self.emit_public_entry_change("updated", current).err();
                if current.state.is_loaded() {
                    let _operation = self.lock_operation();
                    self.reload_entry_with_notifications(entry_id, &mut notification_error)?;
                }
                entry = Some(self.require_entry(entry_id)?);
            }
            DeferredFlowAction::Abort => {}
        }
        let progress_result = self.notify_flow_progress(&request.flow_id);
        if let Some(error) = notification_error {
            return Err(error);
        }
        progress_result?;
        Ok(flow_result_json(&request, kind, &result, entry, next_flow))
    }

    /// Check for an in-progress config discovery flow with the same handler,
    /// a context superset, source, and original initialization data.
    pub fn has_matching_discovery_flow(
        &self,
        domain: &str,
        source: &str,
        match_context: &Map<String, Value>,
        initial_data: &Map<String, Value>,
    ) -> bool {
        self.has_matching_discovery_flow_with_kind(
            domain,
            source,
            match_context,
            initial_data,
            None,
        )
    }

    pub fn has_matching_discovery_flow_with_kind(
        &self,
        domain: &str,
        source: &str,
        match_context: &Map<String, Value>,
        initial_data: &Map<String, Value>,
        data_kind: Option<DiscoveryDataKind>,
    ) -> bool {
        self.lock_state().flows.values().any(|flow| {
            flow.kind == FlowKind::Config
                && flow.data_kind == data_kind
                && flow.request.domain == domain
                && flow.request.source == source
                && match_context
                    .iter()
                    .all(|(key, value)| flow.request.context.get(key) == Some(value))
                && flow.initial_data.as_ref() == Some(initial_data)
        })
    }

    /// Atomically clone entries and original config discovery data for a
    /// trusted provider such as USB usage inference. Ordinary API fragments
    /// continue to redact entry data and never expose this snapshot directly.
    pub fn snapshot_with_discovery_flows(
        &self,
        source: &str,
    ) -> (Vec<ConfigEntry>, Vec<ConfigDiscoveryFlowSnapshot>) {
        let state = self.lock_state();
        let entries = state.entries.values().cloned().collect();
        let mut flows = state
            .flows
            .iter()
            .filter_map(|(flow_id, flow)| {
                if flow.kind != FlowKind::Config || flow.request.source != source {
                    return None;
                }
                Some(ConfigDiscoveryFlowSnapshot {
                    flow_id: flow_id.clone(),
                    domain: flow.request.domain.clone(),
                    initial_data: flow.initial_data.clone()?,
                    data_kind: flow.data_kind,
                })
            })
            .collect::<Vec<_>>();
        flows.sort_by(|left, right| left.flow_id.cmp(&right.flow_id));
        (entries, flows)
    }

    /// Dismiss matching config discovery flows for one handler and source.
    /// User-protected flows, options flows, and subentry flows are left alone.
    pub fn dismiss_discovery_flows(
        &self,
        domain: &str,
        source: &str,
        matcher: impl Fn(&Map<String, Value>) -> bool,
    ) -> Result<usize> {
        self.remove_matching_discovery_flows(domain, source, true, None, matcher)
    }

    /// Abort matching config discovery flows, including user-protected flows.
    /// USB removal uses the pinned source's unconditional abort semantics.
    /// Options and subentry flows remain outside this operation.
    pub fn abort_matching_discovery_flows(
        &self,
        domain: &str,
        source: &str,
        data_kind: DiscoveryDataKind,
        matcher: impl Fn(&Map<String, Value>) -> bool,
    ) -> Result<usize> {
        self.remove_matching_discovery_flows(domain, source, false, Some(data_kind), matcher)
    }

    fn remove_matching_discovery_flows(
        &self,
        domain: &str,
        source: &str,
        respect_protection: bool,
        data_kind: Option<DiscoveryDataKind>,
        matcher: impl Fn(&Map<String, Value>) -> bool,
    ) -> Result<usize> {
        let candidates = {
            let state = self.lock_state();
            state
                .flows
                .iter()
                .filter(|(_, flow)| {
                    flow.kind == FlowKind::Config
                        && data_kind.is_none_or(|kind| flow.data_kind == Some(kind))
                        && flow.request.domain == domain
                        && flow.request.source == source
                        && (!respect_protection
                            || flow.request.context.get("dismiss_protected")
                                != Some(&Value::Bool(true)))
                })
                .filter_map(|(flow_id, flow)| {
                    flow.initial_data
                        .clone()
                        .map(|initial_data| (flow_id.clone(), initial_data))
                })
                .collect::<Vec<_>>()
        };
        let candidates = candidates
            .into_iter()
            .filter(|(_, initial_data)| matcher(initial_data))
            .collect::<Vec<_>>();
        let removed = self.mutate(|state| {
            let mut removed = Vec::new();
            for (flow_id, initial_data) in &candidates {
                let matching = state.flows.get(flow_id).is_some_and(|flow| {
                    flow.kind == FlowKind::Config
                        && data_kind.is_none_or(|kind| flow.data_kind == Some(kind))
                        && flow.request.domain == domain
                        && flow.request.source == source
                        && (!respect_protection
                            || flow.request.context.get("dismiss_protected")
                                != Some(&Value::Bool(true)))
                        && flow.initial_data.as_ref() == Some(initial_data)
                });
                if matching {
                    state.flows.remove(flow_id);
                    removed.push(flow_id.clone());
                }
            }
            Ok(removed)
        })?;
        for flow_id in &removed {
            self.notify_flow_progress(flow_id)?;
        }
        Ok(removed.len())
    }

    fn notify_flow_progress(&self, flow_id: &str) -> Result<()> {
        self.runtime.dispatch_signal(
            "config_flow_progress",
            json!({"flow_id":flow_id})
                .as_object()
                .expect("object")
                .clone(),
            hass_core::Context::default(),
        )?;
        Ok(())
    }

    fn cancel_flow_task(&self, flow_id: &str) {
        if let Some(task) = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(flow_id)
        {
            let _ = task.cancel.send(true);
        }
    }

    fn begin_flow_admission(&self) -> Result<FlowAdmissionGuard> {
        let mut state = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        state.admissions = state
            .admissions
            .checked_add(1)
            .ok_or_else(|| Error::Service("config flow admission counter exhausted".into()))?;
        Ok(FlowAdmissionGuard {
            control: Arc::clone(&self.flow_tasks),
        })
    }

    fn flow_task(&self, flow_id: &str, revision: u64) -> Option<AsyncFlowTask> {
        self.flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(flow_id)
            .filter(|task| task.revision == revision)
            .cloned()
    }

    fn flow_task_any(&self, flow_id: &str) -> Option<AsyncFlowTask> {
        self.flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(flow_id)
            .cloned()
    }

    async fn wait_for_flow_task(&self, flow_id: &str, task: AsyncFlowTask) -> Result<Value> {
        let revision = task.revision;
        let result = Self::receive_flow_task_result(task.result.clone()).await;
        let join_result = task.join.join().await;
        self.drain_completed_flow_task(flow_id, revision, &task)
            .await;
        match (result, join_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(Error::Service(format!(
                "config flow task join failed: {error}"
            ))),
        }
    }

    async fn receive_flow_task_result(
        mut receiver: watch::Receiver<Option<Result<Value>>>,
    ) -> Result<Value> {
        loop {
            if let Some(result) = receiver.borrow().clone() {
                break result;
            }
            if receiver.changed().await.is_err() {
                break Err(Error::Service(
                    "owned config-flow task ended without a result".into(),
                ));
            }
        }
    }

    async fn drain_completed_flow_task(
        &self,
        flow_id: &str,
        revision: u64,
        expected: &AsyncFlowTask,
    ) {
        let task = {
            let state = self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if state.tasks.get(flow_id).is_some_and(|task| {
                task.revision == revision && Arc::ptr_eq(&task.join, &expected.join)
            }) {
                state.tasks.get(flow_id).cloned()
            } else {
                None
            }
        };
        if let Some(task) = task {
            let _ = task.join.join().await;
            let mut state = self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if state.tasks.get(flow_id).is_some_and(|current| {
                current.revision == revision && Arc::ptr_eq(&current.join, &expected.join)
            }) {
                state.tasks.remove(flow_id);
            }
        }
    }

    fn begin_flow_task_shutdown(&self) -> FlowTaskShutdown {
        let mut state = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(task) = state.shutdown_result.as_ref() {
            return task.clone();
        }
        state.closed = true;
        for task in state.tasks.values() {
            let _ = task.cancel.send(true);
        }
        let (sender, receiver) = watch::channel(None);
        let manager = self.clone();
        let handle = tokio::spawn(async move {
            let result = manager.drain_flow_tasks_after_close().await;
            let _ = sender.send(Some(result));
        });
        let task = FlowTaskShutdown {
            result: receiver,
            join: Arc::new(OwnedFlowTaskJoin::new(handle)),
        };
        state.shutdown_result = Some(task.clone());
        task
    }

    async fn drain_flow_tasks_after_close(&self) -> FlowTaskShutdownResult {
        loop {
            let notified = self.flow_tasks.changed.notified();
            if self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .admissions
                == 0
            {
                break;
            }
            notified.await;
        }

        let flow_ids = {
            let _transition = self
                .flow_transitions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut state = self.lock_state();
            let mut ids = state.flows.keys().cloned().collect::<Vec<_>>();
            ids.extend(state.subentry_flows.keys().cloned());
            state.flows.clear();
            state.subentry_flows.clear();
            ids
        };
        for flow_id in &flow_ids {
            let _ = self.notify_flow_progress(flow_id);
        }
        let tasks = {
            let mut state = self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for task in state.tasks.values() {
                let _ = task.cancel.send(true);
            }
            std::mem::take(&mut state.tasks)
                .into_values()
                .collect::<Vec<_>>()
        };
        let mut errors = Vec::new();
        for task in tasks {
            let _ = Self::receive_flow_task_result(task.result.clone()).await;
            if let Err(error) = task.join.join().await {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(flow_ids.len())
        } else {
            Err(format!(
                "config flow task shutdown failed: {}",
                errors.join("; ")
            ))
        }
    }

    async fn wait_for_flow_task_shutdown(task: FlowTaskShutdown) -> Result<usize> {
        let mut receiver = task.result;
        let result = loop {
            if let Some(result) = receiver.borrow().clone() {
                break result.map_err(Error::Service);
            }
            if receiver.changed().await.is_err() {
                break Err(Error::Service(
                    "config flow shutdown ended without a result".into(),
                ));
            }
        };
        let join_result = task.join.join().await;
        let count = result?;
        join_result.map_err(|error| {
            Error::Service(format!("config flow shutdown join failed: {error}"))
        })?;
        Ok(count)
    }

    /// List entries, optionally scoped to an integration domain.
    pub fn list_entries(&self, domain: Option<&str>) -> Vec<ConfigEntry> {
        let entries = self
            .lock_state()
            .entries
            .values()
            .filter(|entry| domain.is_none_or(|domain| entry.domain == domain))
            .cloned()
            .collect::<Vec<_>>();
        entries
            .into_iter()
            .map(|entry| self.with_supported_subentry_types(entry))
            .collect()
    }

    /// Fetch one trusted entry, including its private data and options.
    pub fn get_entry(&self, entry_id: &str) -> Option<ConfigEntry> {
        let entry = { self.lock_state().entries.get(entry_id).cloned() };
        entry.map(|entry| self.with_supported_subentry_types(entry))
    }

    /// Return entries that have already been associated with this discovery key.
    pub fn entries_for_discovery_key(&self, key: &DiscoveryKey) -> Vec<ConfigEntry> {
        let entries = self
            .lock_state()
            .entries
            .values()
            .filter(|entry| {
                entry
                    .discovery_keys
                    .get(&key.domain)
                    .is_some_and(|keys| keys.contains(key))
            })
            .cloned()
            .collect::<Vec<_>>();
        entries
            .into_iter()
            .map(|entry| self.with_supported_subentry_types(entry))
            .collect()
    }

    /// Persist a discovery-key association once, retaining the latest ten keys
    /// for the key's discovery domain.
    pub fn associate_discovery_key(&self, entry_id: &str, key: DiscoveryKey) -> Result<bool> {
        validate_domain(&key.domain)?;
        let changed = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            let changed = record_discovery_key(entry, key);
            if changed {
                entry.modified_at = Utc::now();
            }
            Ok(changed)
        })?;
        if changed {
            self.emit_current_entry_change("updated", entry_id)?;
        }
        Ok(changed)
    }

    /// Fetch one trusted subentry by its exact parent and child IDs.
    pub fn get_subentry(&self, entry_id: &str, subentry_id: &str) -> Option<ConfigSubentry> {
        self.get_entry(entry_id)?
            .subentries
            .into_iter()
            .find(|subentry| subentry.subentry_id == subentry_id)
    }

    /// Return the parent's persisted subentries in their creation order.
    pub fn list_subentries(&self, entry_id: &str) -> Result<Vec<ConfigSubentry>> {
        self.require_entry(entry_id).map(|entry| entry.subentries)
    }

    /// Check that the child belongs to this exact persisted parent entry.
    pub fn subentry_exists(&self, entry_id: &str, subentry_id: &str) -> bool {
        self.lock_state()
            .entries
            .get(entry_id)
            .is_some_and(|entry| {
                entry
                    .subentries
                    .iter()
                    .any(|subentry| subentry.subentry_id == subentry_id)
            })
    }

    /// Persist a new subentry and assign its stable ID.
    pub fn create_subentry(
        &self,
        entry_id: &str,
        seed: NewConfigSubentry,
    ) -> Result<ConfigSubentry> {
        let subentry = ConfigSubentry {
            subentry_id: Uuid::new_v4().to_string(),
            subentry_type: seed.subentry_type,
            title: seed.title,
            unique_id: seed.unique_id,
            data: seed.data,
        };
        let committed_entry = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            ensure_subentry_unique_id(&entry.subentries, subentry.unique_id.as_deref(), None)?;
            if entry
                .subentries
                .iter()
                .any(|existing| existing.subentry_id == subentry.subentry_id)
            {
                return Err(Error::Service("generated subentry ID collision".into()));
            }
            entry.subentries.push(subentry.clone());
            entry.modified_at = Utc::now();
            Ok(entry.clone())
        })?;
        self.emit_entry_change("updated", &committed_entry)?;
        Ok(subentry)
    }

    /// Replace selected subentry fields. Returns false when no field changed.
    pub fn update_subentry(
        &self,
        entry_id: &str,
        subentry_id: &str,
        update: ConfigSubentryUpdate,
    ) -> Result<bool> {
        let changed_entry = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            let subentry_index = entry
                .subentries
                .iter()
                .position(|subentry| subentry.subentry_id == subentry_id)
                .ok_or_else(|| Error::NotFound("Config subentry not found".into()))?;
            let previous_unique_id = entry.subentries[subentry_index].unique_id.clone();
            let mut changed = false;
            if let Some(unique_id) = update.unique_id.as_ref()
                && previous_unique_id != *unique_id
            {
                ensure_subentry_unique_id(
                    &entry.subentries,
                    unique_id.as_deref(),
                    Some(subentry_id),
                )?;
                entry.subentries[subentry_index].unique_id = unique_id.clone();
                changed = true;
            }
            if let Some(title) = update.title.as_ref()
                && entry.subentries[subentry_index].title != *title
            {
                entry.subentries[subentry_index].title = title.clone();
                changed = true;
            }
            if let Some(data) = update.data.as_ref()
                && entry.subentries[subentry_index].data != *data
            {
                entry.subentries[subentry_index].data = data.clone();
                changed = true;
            }
            if !changed {
                return Ok(None);
            }
            entry.modified_at = Utc::now();
            Ok(Some(entry.clone()))
        })?;
        let Some(entry) = changed_entry else {
            return Ok(false);
        };
        self.emit_entry_change("updated", &entry)?;
        Ok(true)
    }

    /// Persist subentry removal and notify lifecycle observers after commit.
    pub fn remove_subentry(&self, entry_id: &str, subentry_id: &str) -> Result<bool> {
        let pending = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            let index = entry
                .subentries
                .iter()
                .position(|subentry| subentry.subentry_id == subentry_id)
                .ok_or_else(|| Error::NotFound("Config subentry not found".into()))?;
            let subentry = entry.subentries.remove(index);
            entry.modified_at = Utc::now();
            let pending = queue_pending_cleanup(
                state,
                PendingCleanupEvent::SubentryRemoved {
                    entry_id: entry_id.to_owned(),
                    subentry_id: subentry.subentry_id.clone(),
                },
            );
            Ok(pending)
        })?;
        // Home Assistant clears child registry references before notifying
        // update listeners; Repairs cleanup follows the same post-commit order.
        let observer_result = self.dispatch_direct_cleanup(&pending);
        let entry_change_result = self.emit_current_entry_change("updated", entry_id);
        observer_result?;
        entry_change_result?;
        Ok(true)
    }

    /// Create and persist a config entry, checking its identity and source.
    pub fn create_entry(&self, seed: NewConfigEntry) -> Result<ConfigEntry> {
        self.create_entry_with_discovery_key(seed, None)
    }

    fn create_entry_with_discovery_key(
        &self,
        seed: NewConfigEntry,
        discovery_key: Option<DiscoveryKey>,
    ) -> Result<ConfigEntry> {
        validate_new_entry(&seed)?;
        if let Some(key) = discovery_key.as_ref() {
            validate_domain(&key.domain)?;
        }
        let now = Utc::now();
        let driver = self.driver(&seed.domain);
        let integration_type = driver
            .as_ref()
            .map(|driver| driver.integration_type().to_owned())
            .unwrap_or_else(default_integration_type);
        let mut entry = ConfigEntry {
            entry_id: Uuid::new_v4().to_string(),
            domain: seed.domain,
            title: seed.title.trim().to_owned(),
            source: seed.source,
            unique_id: seed.unique_id,
            discovery_keys: BTreeMap::new(),
            version: seed.version,
            minor_version: seed.minor_version,
            data: seed.data,
            options: seed.options,
            subentries: Vec::new(),
            state: ConfigEntryState::NotLoaded,
            reason: None,
            retry_attempts: 0,
            retry_at: None,
            disabled_by: None,
            needs_reauth: false,
            require_reload: false,
            pref_disable_new_entities: false,
            pref_disable_polling: false,
            error_reason_translation_domain: None,
            error_reason_translation_key: None,
            error_reason_translation_placeholders: None,
            supports_options: driver
                .as_ref()
                .is_some_and(|driver| driver.supports_options()),
            supports_remove_device: driver
                .as_ref()
                .is_some_and(|driver| driver.supports_remove_device()),
            supports_unload: driver
                .as_ref()
                .is_some_and(|driver| driver.supports_unload()),
            supports_reconfigure: driver
                .as_ref()
                .is_some_and(|driver| driver.supports_reconfigure()),
            supported_subentry_types: Map::new(),
            created_at: now,
            modified_at: now,
            integration_type,
        };
        if let Some(key) = discovery_key {
            record_discovery_key(&mut entry, key);
        }
        entry = self.with_supported_subentry_types(entry);
        let entry_id = entry.entry_id.clone();
        self.mutate(|state| {
            ensure_unique_id(
                &state.entries,
                &entry.domain,
                entry.unique_id.as_deref(),
                None,
            )?;
            state.entries.insert(entry_id.clone(), entry.clone());
            Ok(())
        })?;
        self.emit_entry_change("added", &entry)?;
        Ok(entry)
    }

    /// Set up an entry through its registered native driver.
    pub fn setup_entry(&self, entry_id: &str) -> Result<ConfigEntry> {
        let _operation = self.lock_operation();
        self.setup_entry_locked(entry_id)
    }

    /// Retry a failed or temporarily unavailable setup.
    pub fn retry_entry(&self, entry_id: &str) -> Result<ConfigEntry> {
        let entry = self.require_entry(entry_id)?;
        if !matches!(
            entry.state,
            ConfigEntryState::SetupError | ConfigEntryState::SetupRetry
        ) {
            return Err(Error::InvalidInput(format!(
                "config entry {entry_id} is not in a retryable state"
            )));
        }
        self.setup_entry(entry_id)
    }

    /// Retry setup entries whose persisted retry time has elapsed.
    pub fn retry_due_entries(&self, now: DateTime<Utc>) -> Vec<Result<ConfigEntry>> {
        let due = self
            .list_entries(None)
            .into_iter()
            .filter(|entry| {
                entry.state == ConfigEntryState::SetupRetry
                    && entry.retry_at.is_some_and(|retry_at| retry_at <= now)
            })
            .map(|entry| entry.entry_id)
            .collect::<Vec<_>>();
        due.into_iter()
            .map(|entry_id| self.retry_entry(&entry_id))
            .collect()
    }

    /// Unload an entry through its registered driver.
    pub fn unload_entry(&self, entry_id: &str) -> Result<ConfigEntry> {
        let _operation = self.lock_operation();
        self.unload_entry_locked(entry_id)?;
        self.require_entry(entry_id)
    }

    /// Unload then set up an entry again.
    pub fn reload_entry(&self, entry_id: &str) -> Result<ConfigEntry> {
        let _operation = self.lock_operation();
        let mut notification_error = None;
        let result = self.reload_entry_with_notifications(entry_id, &mut notification_error);
        finish_lifecycle_notifications(result, notification_error)
    }

    fn reload_entry_with_notifications(
        &self,
        entry_id: &str,
        notification_error: &mut Option<Error>,
    ) -> Result<ConfigEntry> {
        let entry = self.require_entry(entry_id)?;
        if entry.state.is_loaded() {
            self.unload_entry_with_notifications(entry_id, notification_error)?;
        }
        self.update_lifecycle_metadata(notification_error, entry_id, |current| {
            current.require_reload = false;
            current.state = ConfigEntryState::NotLoaded;
            current.reason = None;
            current.modified_at = Utc::now();
        })?;
        self.setup_entry_with_notifications(entry_id, notification_error)
    }

    /// Disable or re-enable an entry. Returns false when unloading failed and
    /// a process restart is required to guarantee the requested state.
    pub fn disable_entry(&self, entry_id: &str, disabled_by: Option<&str>) -> Result<bool> {
        if !matches!(disabled_by, None | Some("user")) {
            return Err(Error::InvalidInput(
                "disabled_by must be `user` or null".into(),
            ));
        }
        let operation = self.lock_operation();
        let entry = self.require_entry(entry_id)?;
        if entry.disabled_by.as_deref() == disabled_by {
            return Ok(true);
        }
        let committed_entry = self.mutate(|state| {
            let current = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            current.disabled_by = disabled_by.map(str::to_owned);
            if disabled_by.is_none() && current.state == ConfigEntryState::FailedUnload {
                current.require_reload = true;
            }
            current.modified_at = Utc::now();
            Ok(current.clone())
        })?;

        // The disable state is durable before the parent lifecycle callbacks,
        // matching Home Assistant's issue/registry notification ordering.
        let entry_change_result = self.emit_entry_change("updated", &committed_entry);
        drop(operation);

        let observer_result =
            self.notify_lifecycle_observers(&ConfigLifecycleEvent::EntryDisabledByChanged {
                entry_id: entry_id.to_owned(),
                disabled_by: disabled_by.map(str::to_owned),
            });
        let registry_enable_result = if disabled_by.is_none() {
            self.notify_lifecycle_observers(&ConfigLifecycleEvent::EntryRegistryDisabledByChanged {
                entry_id: entry_id.to_owned(),
            })
        } else {
            Ok(())
        };

        // The observer runs before unload/setup, like the pinned source. The
        // state may be changed by a concurrent request while it runs, so only
        // complete this transition when the requested value is still current.
        let operation = self.lock_operation();
        let operation_result = match self.require_entry(entry_id) {
            Err(error) => Err(error),
            Ok(current) if current.disabled_by.as_deref() != disabled_by => Ok(true),
            Ok(current) if disabled_by.is_some() => {
                if current.state.is_loaded() && self.unload_entry_locked(entry_id).is_err() {
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            Ok(current) if current.state == ConfigEntryState::FailedUnload => Ok(false),
            Ok(_) => match self.setup_entry_locked(entry_id) {
                Ok(_) => Ok(true),
                Err(error) => match self.require_entry(entry_id) {
                    Ok(current)
                        if matches!(
                            current.state,
                            ConfigEntryState::SetupError | ConfigEntryState::SetupRetry
                        ) =>
                    {
                        Ok(false)
                    }
                    Ok(_) => Err(error),
                    Err(read_error) => Err(read_error),
                },
            },
        };
        drop(operation);
        let registry_disable_result = if disabled_by.is_some() {
            self.notify_lifecycle_observers(&ConfigLifecycleEvent::EntryRegistryDisabledByChanged {
                entry_id: entry_id.to_owned(),
            })
        } else {
            Ok(())
        };
        let success = operation_result?;
        entry_change_result?;
        observer_result?;
        registry_enable_result?;
        registry_disable_result?;
        Ok(success)
    }

    /// Remove an entry after running its unload and cleanup callbacks.
    pub fn remove_entry(&self, entry_id: &str) -> Result<()> {
        let operation = self.lock_operation();
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        if entry.state.is_loaded() {
            self.unload_entry_locked(entry_id)?;
        }
        let entry = self.require_entry(entry_id)?;
        driver.remove(&self.runtime, &entry)?;
        let (pending, removed_entry, removed_flow_ids) = self.mutate(|state| {
            let current = state
                .entries
                .get(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            let domain = current.domain.clone();
            let pending = queue_pending_cleanup(
                state,
                PendingCleanupEvent::EntryRemoved {
                    entry_id: entry_id.to_owned(),
                    domain,
                },
            );
            let removed_entry = state
                .entries
                .remove(entry_id)
                .expect("the entry was present in this mutation");
            let removed_flow_ids = state
                .flows
                .iter()
                .filter(|(_, flow)| flow.request.entry_id.as_deref() == Some(entry_id))
                .map(|(flow_id, _)| flow_id.clone())
                .chain(
                    state
                        .subentry_flows
                        .iter()
                        .filter(|(_, flow)| flow.request.entry_id == entry_id)
                        .map(|(flow_id, _)| flow_id.clone()),
                )
                .collect::<Vec<_>>();
            state
                .flows
                .retain(|_, flow| flow.request.entry_id.as_deref() != Some(entry_id));
            state
                .subentry_flows
                .retain(|_, flow| flow.request.entry_id != entry_id);
            Ok((pending, removed_entry, removed_flow_ids))
        })?;
        drop(operation);
        for flow_id in removed_flow_ids {
            self.cancel_flow_task(&flow_id);
        }
        // Source removal dispatch follows registry/issue cleanup. Keep both
        // outside manager locks so observers can reenter the committed state.
        let cleanup_result = self.dispatch_direct_cleanup(&pending);
        let entry_change_result = if cleanup_result.is_ok() {
            self.emit_entry_change("removed", &removed_entry)
        } else {
            Ok(())
        };
        // The deletion is already committed. A cleanup error must not lose the
        // live private notification, whose keys deliberately are not journaled.
        self.notify_discovery_key_removal(&removed_entry);
        cleanup_result.and(entry_change_result)
    }

    /// Replace the private integration options and reload a loaded entry.
    pub fn update_options(
        &self,
        entry_id: &str,
        options: Map<String, Value>,
    ) -> Result<ConfigEntry> {
        let _operation = self.lock_operation();
        let entry = self.require_entry(entry_id)?;
        self.update_entry_metadata(entry_id, |current| {
            current.options = options;
            current.modified_at = Utc::now();
        })?;
        if entry.state.is_loaded() {
            self.reload_entry_locked(entry_id)?;
        }
        self.require_entry(entry_id)
    }

    /// Atomically merge trusted provider-owned fields into an entry's private
    /// data map. The domain guard prevents an adapter from applying a token
    /// patch to another integration's entry. This operation never reloads the
    /// entry or calls its driver.
    pub fn patch_entry_data(
        &self,
        entry_id: &str,
        expected_domain: &str,
        patch: Map<String, Value>,
    ) -> Result<ConfigEntry> {
        validate_domain(expected_domain)?;
        let committed = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
            if entry.domain != expected_domain {
                return Err(Error::InvalidInput(
                    "config entry domain does not match the expected domain".into(),
                ));
            }
            entry.data.extend(patch);
            entry.modified_at = Utc::now();
            Ok(entry.clone())
        })?;
        self.emit_public_entry_change("updated", &committed)?;
        Ok(committed)
    }

    /// Update the entry's title and polling/entity preferences.
    pub fn update_entry_preferences(
        &self,
        entry_id: &str,
        title: Option<String>,
        pref_disable_new_entities: Option<bool>,
        pref_disable_polling: Option<bool>,
    ) -> Result<ConfigEntry> {
        let _operation = self.lock_operation();
        let previous = self.require_entry(entry_id)?;
        if title.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(Error::InvalidInput("entry title cannot be empty".into()));
        }
        self.update_entry_metadata(entry_id, |entry| {
            if let Some(title) = title {
                entry.title = title.trim().to_owned();
            }
            if let Some(value) = pref_disable_new_entities {
                entry.pref_disable_new_entities = value;
            }
            if let Some(value) = pref_disable_polling {
                entry.pref_disable_polling = value;
            }
            entry.modified_at = Utc::now();
        })?;
        let updated = self.require_entry(entry_id)?;
        if previous.pref_disable_polling != updated.pref_disable_polling
            && previous.state.is_loaded()
            && let Err(error) = self.reload_entry_locked(entry_id)
        {
            let current = self.require_entry(entry_id)?;
            if !matches!(
                current.state,
                ConfigEntryState::SetupError
                    | ConfigEntryState::SetupRetry
                    | ConfigEntryState::FailedUnload
            ) {
                return Err(error);
            }
        }
        self.require_entry(entry_id)
    }

    /// Mark an entry as requiring authentication and begin its reauth flow.
    pub fn begin_reauth_flow(&self, entry_id: &str) -> Result<Value> {
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        self.update_entry_metadata(entry_id, |current| {
            current.needs_reauth = true;
            current.modified_at = Utc::now();
        })?;
        let mut context = Map::new();
        context.insert("entry_id".into(), json!(entry_id));
        context.insert("source".into(), json!("reauth"));
        let flow_id = Uuid::new_v4().to_string();
        let request = ConfigFlowRequest {
            flow_id: flow_id.clone(),
            domain: entry.domain.clone(),
            step_id: "reauth".into(),
            source: "reauth".into(),
            entry_id: Some(entry_id.to_owned()),
            unique_id: entry.unique_id.clone(),
            user_input: None,
            context,
        };
        self.run_flow_step(request, FlowKind::Config, driver, None, None)
    }

    /// Begin an entry's reauthentication flow using its optional asynchronous
    /// driver callback. Existing synchronous callbacks run on the blocking
    /// pool, and the owned flow task survives cancellation of this caller.
    pub async fn begin_reauth_flow_async(&self, entry_id: &str) -> Result<Value> {
        let admission = self.begin_flow_admission()?;
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        self.update_entry_metadata(entry_id, |current| {
            current.needs_reauth = true;
            current.modified_at = Utc::now();
        })?;
        let mut context = Map::new();
        context.insert("entry_id".into(), json!(entry_id));
        context.insert("source".into(), json!("reauth"));
        let request = ConfigFlowRequest {
            flow_id: Uuid::new_v4().to_string(),
            domain: entry.domain.clone(),
            step_id: "reauth".into(),
            source: "reauth".into(),
            entry_id: Some(entry_id.to_owned()),
            unique_id: entry.unique_id,
            user_input: None,
            context,
        };
        let lease = self.reserve_initial_async_flow(
            request.clone(),
            FlowKind::Config,
            Arc::clone(&driver),
            None,
            None,
        )?;
        self.run_owned_flow_task(request, driver, lease, None, admission)
            .await
    }

    /// Begin a user/import/discovery config flow through the registered driver.
    pub fn init_config_flow(
        &self,
        domain: &str,
        source: &str,
        context: Map<String, Value>,
    ) -> Result<Value> {
        self.init_config_flow_with_input(domain, source, context, None)
    }

    /// Begin a flow and pass the initial HTTP request data to its user step.
    pub fn init_config_flow_with_input(
        &self,
        domain: &str,
        source: &str,
        context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        self.init_config_flow_with_typed_input(domain, source, context, user_input, None)
    }

    /// Trusted native discovery entrypoint. HTTP/WS initialization uses the
    /// untyped wrapper, so user JSON cannot assert native payload provenance.
    pub fn init_config_flow_with_typed_input(
        &self,
        domain: &str,
        source: &str,
        mut context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
        data_kind: Option<DiscoveryDataKind>,
    ) -> Result<Value> {
        validate_domain(domain)?;
        validate_source(source)?;
        let driver = self.driver(domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {domain}"
            ))
        })?;
        context.insert("source".into(), json!(source));
        let flow_id = Uuid::new_v4().to_string();
        let initial_data = user_input.clone();
        let request = ConfigFlowRequest {
            flow_id,
            domain: domain.to_owned(),
            step_id: "user".into(),
            source: source.to_owned(),
            entry_id: context
                .get("entry_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            unique_id: context
                .get("unique_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            user_input,
            context,
        };
        self.run_flow_step(request, FlowKind::Config, driver, initial_data, data_kind)
    }

    /// Begin a config flow with an optional asynchronous driver callback.
    pub async fn init_config_flow_async(
        &self,
        domain: &str,
        source: &str,
        context: Map<String, Value>,
    ) -> Result<Value> {
        self.init_config_flow_with_typed_input_async(domain, source, context, None, None)
            .await
    }

    /// Begin a config flow and pass initial request data to the driver.
    pub async fn init_config_flow_with_input_async(
        &self,
        domain: &str,
        source: &str,
        context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        self.init_config_flow_with_typed_input_async(domain, source, context, user_input, None)
            .await
    }

    /// Trusted typed-provenance async config-flow entrypoint.
    pub async fn init_config_flow_with_typed_input_async(
        &self,
        domain: &str,
        source: &str,
        mut context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
        data_kind: Option<DiscoveryDataKind>,
    ) -> Result<Value> {
        validate_domain(domain)?;
        validate_source(source)?;
        let admission = self.begin_flow_admission()?;
        let driver = self.driver(domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {domain}"
            ))
        })?;
        context.insert("source".into(), json!(source));
        let flow_id = Uuid::new_v4().to_string();
        let initial_data = user_input.clone();
        let request = ConfigFlowRequest {
            flow_id,
            domain: domain.to_owned(),
            step_id: "user".into(),
            source: source.to_owned(),
            entry_id: context
                .get("entry_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            unique_id: context
                .get("unique_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            user_input,
            context,
        };
        let lease = self.reserve_initial_async_flow(
            request.clone(),
            FlowKind::Config,
            Arc::clone(&driver),
            initial_data,
            data_kind,
        )?;
        self.run_owned_flow_task(request, driver, lease, None, admission)
            .await
    }

    /// Begin an entry's registered options flow.
    pub fn init_options_flow(&self, entry_id: &str) -> Result<Value> {
        self.init_options_flow_with_input(entry_id, None)
    }

    /// Begin an options flow and pass optional initial request data.
    pub fn init_options_flow_with_input(
        &self,
        entry_id: &str,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        let flow_id = Uuid::new_v4().to_string();
        let mut context = Map::new();
        context.insert("source".into(), json!("options"));
        context.insert("entry_id".into(), json!(entry_id));
        let request = ConfigFlowRequest {
            flow_id,
            domain: entry.domain.clone(),
            step_id: "init".into(),
            source: "options".into(),
            entry_id: Some(entry_id.to_owned()),
            unique_id: entry.unique_id,
            user_input,
            context,
        };
        self.run_flow_step(request, FlowKind::Options, driver, None, None)
    }

    /// Begin an entry's registered options flow with manager-owned callback work.
    pub async fn init_options_flow_async(&self, entry_id: &str) -> Result<Value> {
        self.init_options_flow_with_input_async(entry_id, None)
            .await
    }

    /// Begin an options flow and pass optional initial request data.
    pub async fn init_options_flow_with_input_async(
        &self,
        entry_id: &str,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        let admission = self.begin_flow_admission()?;
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        let flow_id = Uuid::new_v4().to_string();
        let mut context = Map::new();
        context.insert("source".into(), json!("options"));
        context.insert("entry_id".into(), json!(entry_id));
        let request = ConfigFlowRequest {
            flow_id,
            domain: entry.domain.clone(),
            step_id: "init".into(),
            source: "options".into(),
            entry_id: Some(entry_id.to_owned()),
            unique_id: entry.unique_id,
            user_input: user_input.clone(),
            context,
        };
        let lease = self.reserve_initial_async_flow(
            request.clone(),
            FlowKind::Options,
            Arc::clone(&driver),
            user_input,
            None,
        )?;
        self.run_owned_flow_task(request, driver, lease, None, admission)
            .await
    }

    /// Begin a supported native subentry flow. New flows use `source = "user"`
    /// with no subentry ID; reconfigure flows require the existing exact child
    /// ID and a type that advertises `supports_reconfigure`.
    pub fn init_subentry_flow(
        &self,
        entry_id: &str,
        subentry_type: &str,
        source: &str,
        subentry_id: Option<&str>,
        context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        let (request, driver) = self.prepare_subentry_flow_request(
            entry_id,
            subentry_type,
            source,
            subentry_id,
            context,
            user_input,
        )?;
        self.run_subentry_flow_step(request, driver)
    }

    /// Start a subentry flow with manager-owned callback work.
    pub async fn init_subentry_flow_async(
        &self,
        entry_id: &str,
        subentry_type: &str,
        source: &str,
        subentry_id: Option<&str>,
        context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
        let admission = self.begin_flow_admission()?;
        let (request, driver) = self.prepare_subentry_flow_request(
            entry_id,
            subentry_type,
            source,
            subentry_id,
            context,
            user_input,
        )?;
        let lease =
            self.reserve_initial_async_subentry_flow(request.clone(), Arc::clone(&driver))?;
        self.run_owned_subentry_flow_task(request, driver, lease, None, admission)
            .await
    }

    fn prepare_subentry_flow_request(
        &self,
        entry_id: &str,
        subentry_type: &str,
        source: &str,
        subentry_id: Option<&str>,
        mut context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<(ConfigSubentryFlowRequest, Arc<dyn ConfigEntryDriver>)> {
        validate_source(source)?;
        match (source, subentry_id) {
            ("user", None) | ("reconfigure", Some(_)) => {}
            ("user", Some(_)) => {
                return Err(Error::InvalidInput(
                    "new subentry flows cannot specify subentry_id".into(),
                ));
            }
            ("reconfigure", None) => {
                return Err(Error::InvalidInput(
                    "subentry reconfigure flow requires subentry_id".into(),
                ));
            }
            _ => {
                return Err(Error::InvalidInput(
                    "subentry flows support user and reconfigure sources".into(),
                ));
            }
        }
        let entry = self.require_entry(entry_id)?;
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        })?;
        let supported = driver.supported_subentry_types(&entry);
        let type_info = supported.get(subentry_type).ok_or_else(|| {
            Error::NotFound(format!(
                "config entry {} does not support subentry type {subentry_type}",
                entry.domain
            ))
        })?;
        if source == "reconfigure" {
            if !type_info.supports_reconfigure {
                return Err(Error::InvalidInput(format!(
                    "subentry type {subentry_type} does not support reconfigure"
                )));
            }
            let existing = self
                .get_subentry(entry_id, subentry_id.expect("source validated"))
                .ok_or_else(|| Error::NotFound("Config subentry not found".into()))?;
            if existing.subentry_type != subentry_type {
                return Err(Error::InvalidInput(
                    "subentry type does not match the requested reconfigure flow".into(),
                ));
            }
        }
        context.insert("source".into(), json!(source));
        context.insert("entry_id".into(), json!(entry_id));
        context.insert("subentry_type".into(), json!(subentry_type));
        if let Some(subentry_id) = subentry_id {
            context.insert("subentry_id".into(), json!(subentry_id));
        } else {
            context.remove("subentry_id");
        }
        let request = ConfigSubentryFlowRequest {
            flow_id: Uuid::new_v4().to_string(),
            domain: entry.domain,
            entry_id: entry_id.to_owned(),
            subentry_type: subentry_type.to_owned(),
            subentry_id: subentry_id.map(str::to_owned),
            step_id: source.to_owned(),
            source: source.to_owned(),
            user_input,
            context,
        };
        Ok((request, driver))
    }

    /// Submit user input to an in-progress config/options flow.
    pub fn submit_flow(&self, flow_id: &str, user_input: Map<String, Value>) -> Result<Value> {
        let (entry_session, subentry_session) = {
            let state = self.lock_state();
            (
                state.flows.get(flow_id).cloned(),
                state.subentry_flows.get(flow_id).cloned(),
            )
        };
        if let Some(session) = subentry_session {
            let driver = self.driver(&session.request.domain).ok_or_else(|| {
                Error::NotFound(format!(
                    "no native integration driver registered for {}",
                    session.request.domain
                ))
            })?;
            let mut request = session.request;
            request.user_input = Some(user_input);
            return self.run_subentry_flow_step(request, driver);
        }
        let session = entry_session
            .ok_or_else(|| Error::NotFound(format!("unknown config flow {flow_id}")))?;
        if session.phase != FlowPhase::Ready {
            return Err(Error::InvalidInput(
                "external flow transitions cannot be submitted as user input".into(),
            ));
        }
        let (driver, driver_generation) = self
            .driver_registration(&session.request.domain)
            .ok_or_else(|| Error::NotFound("config flow driver is no longer registered".into()))?;
        let initial_data = session.initial_data;
        let data_kind = session.data_kind;
        let kind = session.kind;
        let (request, lease, backup) = self.mutate(|state| {
            let flow = state
                .flows
                .get_mut(flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            if flow.phase != FlowPhase::Ready
                || flow.revision != session.revision
                || flow.driver_generation != driver_generation
                || flow
                    .driver
                    .upgrade()
                    .is_none_or(|registered| !Arc::ptr_eq(&registered, &driver))
            {
                return Err(Error::InvalidInput(
                    "config flow changed before submit".into(),
                ));
            }
            let backup = flow.clone();
            flow.phase = FlowPhase::Running;
            flow.revision = flow
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
            flow.request.user_input = Some(user_input);
            Ok((flow.request.clone(), external_lease(flow), backup))
        })?;
        self.run_flow_step_with_lease(
            request,
            kind,
            driver,
            initial_data,
            data_kind,
            FlowStepGuard {
                lease: Some(lease),
                backup: Some(backup),
            },
        )
    }

    /// Submit flow input using the optional asynchronous config-flow callback.
    /// A single task owns each transition; concurrent callers await the same
    /// outcome, and dropping one caller does not cancel the owned work.
    pub async fn submit_flow_async(
        &self,
        flow_id: &str,
        user_input: Map<String, Value>,
    ) -> Result<Value> {
        let (session, subentry_session) = {
            let state = self.lock_state();
            (
                state.flows.get(flow_id).cloned(),
                state.subentry_flows.get(flow_id).cloned(),
            )
        };
        if let Some(session) = subentry_session {
            if session.phase == FlowPhase::Running {
                let task = self
                    .flow_task(flow_id, session.revision)
                    .ok_or_else(|| Error::Service("subentry flow is already advancing".into()))?;
                return self.wait_for_flow_task(flow_id, task).await;
            }
            if session.phase != FlowPhase::Ready {
                return Err(Error::InvalidInput("subentry flow is not ready".into()));
            }
            if let Some(task) = self.flow_task_any(flow_id) {
                let _ = self.wait_for_flow_task(flow_id, task).await;
            }
            let admission = self.begin_flow_admission()?;
            let (driver, driver_generation) = self
                .driver_registration(&session.request.domain)
                .ok_or_else(|| {
                    Error::NotFound("subentry flow driver is no longer registered".into())
                })?;
            let (request, lease, backup) =
                {
                    let control = self
                        .flow_tasks
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if control.closed {
                        return Err(Error::Service(
                            "config flow manager is shutting down".into(),
                        ));
                    }
                    self.mutate(|state| {
                        let flow = state.subentry_flows.get_mut(flow_id).ok_or_else(|| {
                            Error::NotFound("unknown config subentry flow".into())
                        })?;
                        if flow.phase != FlowPhase::Ready
                            || flow.revision != session.revision
                            || flow.driver_generation != driver_generation
                            || flow
                                .driver
                                .upgrade()
                                .is_none_or(|registered| !Arc::ptr_eq(&registered, &driver))
                        {
                            return Err(Error::InvalidInput(
                                "config subentry flow changed before submit".into(),
                            ));
                        }
                        let backup = flow.clone();
                        flow.phase = FlowPhase::Running;
                        flow.revision = flow.revision.checked_add(1).ok_or_else(|| {
                            Error::Service("subentry flow revision exhausted".into())
                        })?;
                        flow.request.user_input = Some(user_input);
                        Ok((flow.request.clone(), subentry_flow_lease(flow), backup))
                    })?
                };
            return self
                .run_owned_subentry_flow_task(request, driver, lease, Some(backup), admission)
                .await;
        }
        let session =
            session.ok_or_else(|| Error::NotFound(format!("unknown config flow {flow_id}")))?;
        if session.phase == FlowPhase::Running {
            let task = self
                .flow_task(flow_id, session.revision)
                .ok_or_else(|| Error::Service("config flow is already advancing".into()))?;
            return self.wait_for_flow_task(flow_id, task).await;
        }
        if session.phase != FlowPhase::Ready {
            return Err(Error::InvalidInput(
                "external flow transitions cannot be submitted as user input".into(),
            ));
        }
        if let Some(task) = self
            .flow_task_any(flow_id)
            .filter(|task| task.revision != session.revision)
        {
            let _ = self.wait_for_flow_task(flow_id, task).await;
        }
        let admission = self.begin_flow_admission()?;
        let (driver, driver_generation) = self
            .driver_registration(&session.request.domain)
            .ok_or_else(|| Error::NotFound("config flow driver is no longer registered".into()))?;
        let (request, lease, backup) = {
            let control = self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if control.closed {
                return Err(Error::Service(
                    "config flow manager is shutting down".into(),
                ));
            }
            self.mutate(|state| {
                let flow = state
                    .flows
                    .get_mut(flow_id)
                    .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
                if flow.phase != FlowPhase::Ready
                    || flow.revision != session.revision
                    || flow.driver_generation != driver_generation
                    || flow
                        .driver
                        .upgrade()
                        .is_none_or(|registered| !Arc::ptr_eq(&registered, &driver))
                {
                    return Err(Error::InvalidInput(
                        "config flow changed before submit".into(),
                    ));
                }
                let backup = flow.clone();
                flow.phase = FlowPhase::Running;
                flow.revision = flow
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
                flow.request.user_input = Some(user_input);
                Ok((flow.request.clone(), external_lease(flow), backup))
            })?
        };
        self.run_owned_flow_task(request, driver, lease, Some(backup), admission)
            .await
    }

    /// Refresh the current cached flow result. External-flow hosts claim and
    /// advance `external_done` through the explicit lease methods first.
    pub async fn refresh_flow_async(&self, flow_id: &str) -> Result<Value> {
        let receiver = {
            let state = self.lock_state();
            state
                .flows
                .get(flow_id)
                .filter(|flow| flow.phase == FlowPhase::Running)
                .map(|flow| flow.revision)
                .or_else(|| {
                    state
                        .subentry_flows
                        .get(flow_id)
                        .filter(|flow| flow.phase == FlowPhase::Running)
                        .map(|flow| flow.revision)
                })
        };
        if let Some(revision) = receiver
            && let Some(task) = self.flow_task(flow_id, revision)
        {
            return self.wait_for_flow_task(flow_id, task).await;
        }
        self.flow_result(flow_id)
    }

    /// Abort a flow and cancel any manager-owned async callback task.
    pub async fn abort_flow_async(&self, flow_id: &str) -> Result<()> {
        let task = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(flow_id)
            .cloned();
        // Even when removing the flow cannot persist, its callback must stop
        // and its blocking work must be joined before the abort error returns.
        self.cancel_flow_task(flow_id);
        let abort_result = self.abort_flow(flow_id);
        if let Some(task) = task {
            let _ = self.wait_for_flow_task(flow_id, task).await;
        }
        abort_result
    }

    /// Request cancellation for all in-memory flows without waiting for their
    /// callbacks. Hosts should call `shutdown_flow_tasks_async` to drain them.
    pub fn shutdown_flow_tasks(&self) -> usize {
        {
            let mut task_state = self
                .flow_tasks
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            task_state.closed = true;
            for task in task_state.tasks.values() {
                let _ = task.cancel.send(true);
            }
        }
        let flow_ids = {
            let _transition = self
                .flow_transitions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut state = self.lock_state();
            let mut ids = state.flows.keys().cloned().collect::<Vec<_>>();
            ids.extend(state.subentry_flows.keys().cloned());
            state.flows.clear();
            state.subentry_flows.clear();
            ids
        };
        for flow_id in &flow_ids {
            let _ = self.notify_flow_progress(flow_id);
        }
        flow_ids.len()
    }

    /// Cancel all in-memory flows and join their owned callbacks during host
    /// shutdown. Synchronous legacy callbacks already running on the blocking
    /// pool are allowed to finish, then their stale flow leases are rejected.
    pub async fn shutdown_flow_tasks_async(&self) -> Result<usize> {
        Self::wait_for_flow_task_shutdown(self.begin_flow_task_shutdown()).await
    }

    /// Return the persisted core configuration.
    pub fn core_config(&self) -> CoreConfig {
        self.lock_state().core_config.clone()
    }

    /// Validate, persist, and replace the core configuration.
    pub fn update_core_config(&self, config: CoreConfig) -> Result<CoreConfig> {
        let _operation = self.lock_operation();
        let event_data = config
            .to_json()
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Service("core configuration is not an object".into()))?;
        self.replace_core_config(config, event_data)
    }

    fn apply_runtime_core_config(&self, config: &CoreConfig) -> Result<()> {
        self.runtime.set_home_context(HomeContext {
            latitude: config.latitude,
            longitude: config.longitude,
            elevation: config.elevation,
            time_zone: config.time_zone.clone(),
        })
    }

    fn replace_core_config(
        &self,
        config: CoreConfig,
        event_data: Map<String, Value>,
    ) -> Result<CoreConfig> {
        config.validate()?;
        self.mutate(|state| {
            state.core_config = config.clone();
            state.core_config_initialized = true;
            Ok(())
        })?;
        self.apply_runtime_core_config(&config)?;
        self.runtime
            .fire_event("core_config_updated", event_data, Context::default())?;
        Ok(config)
    }

    /// Seed core settings from the YAML config once. A saved UI update takes
    /// precedence on later process starts.
    pub fn initialize_core_config(&self, yaml_config: &Value) -> Result<CoreConfig> {
        if self.lock_state().core_config_initialized {
            return Ok(self.core_config());
        }
        let source = yaml_config
            .get("homeassistant")
            .filter(|value| value.is_object())
            .unwrap_or(yaml_config);
        let object = source
            .as_object()
            .ok_or_else(|| Error::InvalidInput("homeassistant config must be an object".into()))?;
        let mut config = self.core_config();
        if let Some(value) = object.get("name") {
            config.location_name = value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("homeassistant.name must be a string".into()))?
                .to_owned();
        }
        if let Some(value) = object.get("location_name") {
            config.location_name = value
                .as_str()
                .ok_or_else(|| {
                    Error::InvalidInput("homeassistant.location_name must be a string".into())
                })?
                .to_owned();
        }
        if let Some(value) = object.get("latitude") {
            config.latitude = value.as_f64().ok_or_else(|| {
                Error::InvalidInput("homeassistant.latitude must be a number".into())
            })?;
        }
        if let Some(value) = object.get("longitude") {
            config.longitude = value.as_f64().ok_or_else(|| {
                Error::InvalidInput("homeassistant.longitude must be a number".into())
            })?;
        }
        if let Some(value) = object.get("elevation") {
            config.elevation = value.as_f64().ok_or_else(|| {
                Error::InvalidInput("homeassistant.elevation must be a number".into())
            })?;
        }
        if let Some(value) = object.get("time_zone") {
            config.time_zone = value
                .as_str()
                .ok_or_else(|| {
                    Error::InvalidInput("homeassistant.time_zone must be a string".into())
                })?
                .to_owned();
        }
        if let Some(value) = object.get("unit_system") {
            let name = value.as_str().ok_or_else(|| {
                Error::InvalidInput("homeassistant.unit_system must be metric or imperial".into())
            })?;
            config.unit_system = match name {
                "metric" => UnitSystem::metric(),
                "imperial" => UnitSystem::imperial(),
                _ => {
                    return Err(Error::InvalidInput(
                        "homeassistant.unit_system must be metric or imperial".into(),
                    ));
                }
            };
        }
        if let Some(value) = object.get("external_url") {
            config.external_url = optional_url_value("external_url", value)?;
        }
        if let Some(value) = object.get("internal_url") {
            config.internal_url = optional_url_value("internal_url", value)?;
        }
        config.validate()?;
        self.mutate(|state| {
            if !state.core_config_initialized {
                state.core_config = config.clone();
                state.core_config_initialized = true;
            }
            Ok(())
        })?;
        let initialized = self.core_config();
        self.apply_runtime_core_config(&initialized)?;
        Ok(initialized)
    }

    /// Handle a config-entry/config-core WebSocket payload and return its inner
    /// result value (the outer `id` and WebSocket response envelope are owned
    /// by the transport adapter).
    pub fn handle_ws(&self, command: &Value) -> Result<Value> {
        let object = command
            .as_object()
            .ok_or_else(|| Error::InvalidInput("config command must be an object".into()))?;
        let command_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidInput("config command type must be a string".into()))?;
        match command_type {
            "config/core/get" => Ok(self.core_config().to_json()),
            "config/core/update" => self.handle_core_update(object),
            "config_entries/get" => {
                let domain = object.get("domain").and_then(Value::as_str);
                let mut entries = self.list_entries(domain);
                if let Some(filter) = object.get("type_filter") {
                    let filters = match filter {
                        Value::String(filter) => vec![filter.as_str()],
                        Value::Array(values) => values
                            .iter()
                            .map(|value| {
                                value.as_str().ok_or_else(|| {
                                    Error::InvalidInput("type_filter must contain strings".into())
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                        _ => {
                            return Err(Error::InvalidInput(
                                "type_filter must be a string or an array of strings".into(),
                            ));
                        }
                    };
                    if !filters.is_empty() {
                        entries.retain(|entry| filters.contains(&entry.integration_type.as_str()));
                    }
                }
                Ok(Value::Array(
                    entries.iter().map(ConfigEntry::public_json).collect(),
                ))
            }
            "config_entries/get_single" => {
                let entry_id = required_string(object, "entry_id")?;
                let entry = self.require_entry(entry_id)?;
                Ok(json!({ "config_entry": entry.public_json() }))
            }
            "config_entries/update" => {
                let entry_id = required_string(object, "entry_id")?;
                let title = optional_string(object, "title")?;
                let pref_disable_new_entities = optional_bool(object, "pref_disable_new_entities")?;
                let pref_disable_polling = optional_bool(object, "pref_disable_polling")?;
                let entry = self.update_entry_preferences(
                    entry_id,
                    title,
                    pref_disable_new_entities,
                    pref_disable_polling,
                )?;
                Ok(
                    json!({ "config_entry": entry.public_json(), "require_restart": entry.require_reload }),
                )
            }
            "config_entries/disable" => {
                let entry_id = required_string(object, "entry_id")?;
                let disabled_by = if object.get("disabled_by").is_some_and(Value::is_null) {
                    None
                } else {
                    Some(required_string(object, "disabled_by")?)
                };
                let success = self.disable_entry(entry_id, disabled_by)?;
                let entry = self.require_entry(entry_id)?;
                let require_restart = !success
                    && !matches!(
                        entry.state,
                        ConfigEntryState::SetupRetry | ConfigEntryState::SetupError
                    );
                Ok(json!({ "require_restart": require_restart }))
            }
            "config_entries/subscribe" | "config_entries/flow/subscribe" => Ok(Value::Null),
            "config_entries/flow/progress" => Ok(Value::Array(self.flow_progress())),
            "config_entries/flow/init" => {
                let domain = required_string(object, "handler")?;
                let source = optional_string(object, "source")?.unwrap_or_else(|| "user".into());
                let context = object
                    .get("context")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let user_input = object.get("data").and_then(Value::as_object).cloned();
                self.init_config_flow_with_input(domain, &source, context, user_input)
            }
            "config_entries/flow/step" => {
                let flow_id = required_string(object, "flow_id")?;
                let user_input = object
                    .get("user_input")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or_else(|| Error::InvalidInput("user_input must be an object".into()))?;
                self.submit_flow(flow_id, user_input)
            }
            "config_entries/options/flow/init" => {
                let user_input = object.get("data").and_then(Value::as_object).cloned();
                self.init_options_flow_with_input(required_string(object, "entry_id")?, user_input)
            }
            _ => Err(Error::InvalidInput(format!(
                "unsupported config command: {command_type}"
            ))),
        }
    }

    fn handle_core_update(&self, object: &Map<String, Value>) -> Result<Value> {
        let _operation = self.lock_operation();
        if let Some(field) = object.keys().find(|field| {
            !matches!(
                field.as_str(),
                "id" | "type"
                    | "location_name"
                    | "latitude"
                    | "longitude"
                    | "elevation"
                    | "time_zone"
                    | "unit_system"
                    | "external_url"
                    | "internal_url"
            )
        }) {
            return Err(Error::InvalidInput(format!(
                "unsupported core configuration field {field}"
            )));
        }
        let mut config = self.core_config();
        if let Some(value) = object.get("location_name") {
            config.location_name = value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("location_name must be a string".into()))?
                .to_owned();
        }
        if let Some(value) = object.get("latitude") {
            config.latitude = value
                .as_f64()
                .ok_or_else(|| Error::InvalidInput("latitude must be a number".into()))?;
        }
        if let Some(value) = object.get("longitude") {
            config.longitude = value
                .as_f64()
                .ok_or_else(|| Error::InvalidInput("longitude must be a number".into()))?;
        }
        if let Some(value) = object.get("elevation") {
            config.elevation = value
                .as_f64()
                .ok_or_else(|| Error::InvalidInput("elevation must be a number".into()))?;
        }
        if let Some(value) = object.get("time_zone") {
            config.time_zone = value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("time_zone must be a string".into()))?
                .to_owned();
        }
        if let Some(value) = object.get("unit_system") {
            if let Some(name) = value.as_str() {
                config.unit_system = match name {
                    "metric" => UnitSystem::metric(),
                    "imperial" => UnitSystem::imperial(),
                    _ => {
                        return Err(Error::InvalidInput(
                            "unit_system must be metric or imperial".into(),
                        ));
                    }
                };
            } else {
                config.unit_system = serde_json::from_value(value.clone()).map_err(|error| {
                    Error::InvalidInput(format!("invalid unit_system: {error}"))
                })?;
            }
        }
        if let Some(value) = object.get("external_url") {
            config.external_url = optional_url_value("external_url", value)?;
        }
        if let Some(value) = object.get("internal_url") {
            config.internal_url = optional_url_value("internal_url", value)?;
        }
        let event_data = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self.replace_core_config(config.clone(), event_data)?;
        Ok(config.to_json())
    }

    fn associate_flow_discovery_key(&self, request: &ConfigFlowRequest) -> Result<()> {
        let Some(key) = discovery_key_from_context(&request.context) else {
            return Ok(());
        };
        let Some(unique_id) = request.unique_id.as_deref() else {
            return Ok(());
        };
        let entry_id = self.mutate(|state| {
            let entry_id = state
                .entries
                .values()
                .find(|entry| {
                    entry.domain == request.domain && entry.unique_id.as_deref() == Some(unique_id)
                })
                .map(|entry| entry.entry_id.clone());
            let Some(entry_id) = entry_id else {
                return Ok(None);
            };
            let entry = state
                .entries
                .get_mut(&entry_id)
                .expect("discovery key target was found in the same mutation");
            if !record_discovery_key(entry, key) {
                return Ok(None);
            }
            entry.modified_at = Utc::now();
            Ok(Some(entry_id))
        })?;
        if let Some(entry_id) = entry_id {
            self.emit_current_entry_change("updated", &entry_id)?;
        }
        Ok(())
    }

    fn reserve_initial_async_flow(
        &self,
        request: ConfigFlowRequest,
        kind: FlowKind,
        driver: Arc<dyn ConfigEntryDriver>,
        initial_data: Option<Map<String, Value>>,
        data_kind: Option<DiscoveryDataKind>,
    ) -> Result<ExternalFlowLease> {
        let task_control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if task_control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        let registration = registrations
            .drivers
            .get(&request.domain)
            .filter(|registration| Arc::ptr_eq(&registration.driver, &driver))
            .ok_or_else(|| Error::NotFound("config flow driver registration changed".into()))?;
        let lease = self.mutate(|state| {
            if state.flows.contains_key(&request.flow_id)
                || state.subentry_flows.contains_key(&request.flow_id)
            {
                return Err(Error::InvalidInput("config flow ID collision".into()));
            }
            let flow = FlowSession {
                request,
                kind,
                initial_data,
                data_kind,
                result: Value::Null,
                phase: FlowPhase::Running,
                revision: 1,
                driver_generation: registration.generation,
                driver: Arc::downgrade(&driver),
            };
            let lease = external_lease(&flow);
            state.flows.insert(flow.request.flow_id.clone(), flow);
            Ok(lease)
        })?;
        drop(registrations);
        drop(task_control);
        Ok(lease)
    }

    async fn run_owned_flow_task(
        &self,
        request: ConfigFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
        lease: ExternalFlowLease,
        backup: Option<FlowSession>,
        admission: FlowAdmissionGuard,
    ) -> Result<Value> {
        let flow_id = lease.flow_id.clone();
        let task = match self.ensure_owned_flow_task(request, driver, lease.clone(), backup.clone())
        {
            Ok(task) => task,
            Err(error) => {
                self.restore_async_flow_after_error(&lease, backup);
                drop(admission);
                return Err(error);
            }
        };
        drop(admission);
        self.wait_for_flow_task(&flow_id, task).await
    }

    fn ensure_owned_flow_task(
        &self,
        request: ConfigFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
        lease: ExternalFlowLease,
        backup: Option<FlowSession>,
    ) -> Result<AsyncFlowTask> {
        let (sender, receiver) = watch::channel(None);
        let (start_sender, start_receiver) = tokio::sync::oneshot::channel();
        let (cancel_sender, mut cancel_receiver) = watch::channel(false);
        let mut control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        if let Some(existing) = control.tasks.get(&lease.flow_id)
            && existing.revision == lease.revision
        {
            return Ok(existing.clone());
        }
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        ensure_lease_driver(&registrations, &lease)?;
        let kind = {
            let state = self.lock_state();
            let flow = state
                .flows
                .get(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
            flow.kind
        };
        let manager = self.clone();
        let callback_lease = lease.clone();
        let task_driver = Arc::clone(&driver);
        let task_request = request.clone();
        let handle = tokio::spawn(async move {
            if start_receiver.await.is_err() {
                return;
            }
            let callback_result = if *cancel_receiver.borrow() {
                Err(Error::Service("config flow was aborted".into()))
            } else if let Some(callback) = match kind {
                FlowKind::Config => task_driver.config_flow_async(task_request.clone()),
                FlowKind::Options => None,
            } {
                let mut callback = AbortOnDropJoin::new(tokio::spawn(callback));
                tokio::select! {
                    result = &mut callback => match result {
                        Ok(result) => result,
                        Err(error) => Err(Error::Service(format!(
                            "async config-flow driver failed: {error}"
                        ))),
                    },
                    changed = cancel_receiver.changed() => {
                        let _ = changed;
                        callback.0.abort();
                        let _ = callback.await;
                        Err(Error::Service("config flow was aborted".into()))
                    }
                }
            } else {
                let sync_driver = Arc::clone(&task_driver);
                let sync_request = task_request.clone();
                let worker = tokio::task::spawn_blocking(move || match kind {
                    FlowKind::Config => sync_driver.config_flow(&sync_request),
                    FlowKind::Options => sync_driver.options_flow(&sync_request),
                });
                let result = match worker.await {
                    Ok(result) => result,
                    Err(error) => Err(Error::Service(format!(
                        "config-flow worker failed: {error}"
                    ))),
                };
                if *cancel_receiver.borrow() {
                    Err(Error::Service("config flow was aborted".into()))
                } else {
                    result
                }
            };
            // Capability/setup/continuation callbacks are synchronous too.
            // Keep their real completion owned without blocking a Tokio worker.
            let outcome = tokio::task::spawn_blocking(move || match callback_result {
                Ok(result) => manager.commit_async_flow_result(
                    callback_lease.clone(),
                    task_request.clone(),
                    kind,
                    result,
                ),
                Err(error) => {
                    manager.restore_async_flow_after_error(&callback_lease, backup);
                    Err(error)
                }
            })
            .await
            .unwrap_or_else(|error| {
                Err(Error::Service(format!(
                    "config flow completion failed: {error}"
                )))
            });
            let _ = sender.send(Some(outcome));
        });
        let task = AsyncFlowTask {
            revision: lease.revision,
            result: receiver.clone(),
            cancel: cancel_sender,
            join: Arc::new(OwnedFlowTaskJoin::new(handle)),
        };
        control.tasks.insert(lease.flow_id.clone(), task.clone());
        drop(registrations);
        drop(control);
        let _ = start_sender.send(());
        Ok(task)
    }

    fn reserve_initial_async_subentry_flow(
        &self,
        request: ConfigSubentryFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
    ) -> Result<SubentryFlowLease> {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        let (registered, driver_generation) =
            self.driver_registration(&request.domain).ok_or_else(|| {
                Error::NotFound("subentry flow driver is no longer registered".into())
            })?;
        if !Arc::ptr_eq(&registered, &driver) {
            return Err(Error::NotFound(
                "subentry flow driver registration changed".into(),
            ));
        }
        let lease = self.mutate(|state| {
            if state.flows.contains_key(&request.flow_id)
                || state.subentry_flows.contains_key(&request.flow_id)
            {
                return Err(Error::InvalidInput("config flow ID collision".into()));
            }
            let flow = ConfigSubentryFlowSession {
                request,
                result: Value::Null,
                phase: FlowPhase::Running,
                revision: 1,
                driver_generation,
                driver: Arc::downgrade(&driver),
            };
            let lease = subentry_flow_lease(&flow);
            state
                .subentry_flows
                .insert(flow.request.flow_id.clone(), flow);
            Ok(lease)
        })?;
        drop(control);
        Ok(lease)
    }

    async fn run_owned_subentry_flow_task(
        &self,
        request: ConfigSubentryFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
        lease: SubentryFlowLease,
        backup: Option<ConfigSubentryFlowSession>,
        admission: FlowAdmissionGuard,
    ) -> Result<Value> {
        let flow_id = lease.flow_id.clone();
        let task = match self.ensure_owned_subentry_flow_task(
            request,
            driver,
            lease.clone(),
            backup.clone(),
        ) {
            Ok(task) => task,
            Err(error) => {
                self.restore_async_subentry_flow_after_error(&lease, backup);
                drop(admission);
                return Err(error);
            }
        };
        drop(admission);
        self.wait_for_flow_task(&flow_id, task).await
    }

    fn ensure_owned_subentry_flow_task(
        &self,
        request: ConfigSubentryFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
        lease: SubentryFlowLease,
        backup: Option<ConfigSubentryFlowSession>,
    ) -> Result<AsyncFlowTask> {
        let (sender, receiver) = watch::channel(None);
        let (start_sender, start_receiver) = tokio::sync::oneshot::channel();
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let mut control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        if let Some(existing) = control.tasks.get(&lease.flow_id)
            && existing.revision == lease.revision
        {
            return Ok(existing.clone());
        }
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        let registration = registrations
            .drivers
            .get(&lease.domain)
            .ok_or_else(|| Error::NotFound("subentry flow driver is not registered".into()))?;
        let same_driver = lease
            .driver
            .upgrade()
            .is_some_and(|registered| Arc::ptr_eq(&registered, &registration.driver));
        if registration.generation != lease.driver_generation || !same_driver {
            return Err(Error::NotFound(
                "subentry flow driver registration changed".into(),
            ));
        }
        {
            let state = self.lock_state();
            let flow = state
                .subentry_flows
                .get(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
            ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
        }
        let manager = self.clone();
        let callback_lease = lease.clone();
        let task_driver = Arc::clone(&driver);
        let task_request = request.clone();
        let handle = tokio::spawn(async move {
            if start_receiver.await.is_err() {
                return;
            }
            let callback_result = if *cancel_receiver.borrow() {
                Err(Error::Service("config subentry flow was aborted".into()))
            } else {
                let callback_driver = Arc::clone(&task_driver);
                let callback_request = task_request.clone();
                match tokio::task::spawn_blocking(move || {
                    callback_driver.subentry_flow(&callback_request)
                })
                .await
                {
                    Ok(result) if !*cancel_receiver.borrow() => result,
                    Ok(_) => Err(Error::Service("config subentry flow was aborted".into())),
                    Err(error) => Err(Error::Service(format!(
                        "subentry config-flow worker failed: {error}"
                    ))),
                }
            };
            let outcome = tokio::task::spawn_blocking(move || match callback_result {
                Ok(result) => manager.commit_async_subentry_flow_result(
                    callback_lease.clone(),
                    task_request.clone(),
                    result,
                ),
                Err(error) => {
                    manager.restore_async_subentry_flow_after_error(&callback_lease, backup);
                    Err(error)
                }
            })
            .await
            .unwrap_or_else(|error| {
                Err(Error::Service(format!(
                    "subentry flow completion failed: {error}"
                )))
            });
            let _ = sender.send(Some(outcome));
        });
        let task = AsyncFlowTask {
            revision: lease.revision,
            result: receiver,
            cancel: cancel_sender,
            join: Arc::new(OwnedFlowTaskJoin::new(handle)),
        };
        control.tasks.insert(lease.flow_id.clone(), task.clone());
        drop(registrations);
        drop(control);
        let _ = start_sender.send(());
        Ok(task)
    }

    fn restore_async_subentry_flow_after_error(
        &self,
        lease: &SubentryFlowLease,
        backup: Option<ConfigSubentryFlowSession>,
    ) {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return;
        }
        let _ = self.mutate(|state| {
            let Some(current) = state.subentry_flows.get(&lease.flow_id) else {
                return Ok(());
            };
            if ensure_subentry_flow_matches_lease(current, lease, FlowPhase::Running).is_err() {
                return Ok(());
            }
            if let Some(mut backup) = backup {
                backup.phase = FlowPhase::Ready;
                backup.revision = current.revision.saturating_add(1);
                backup.request.user_input = None;
                state.subentry_flows.insert(lease.flow_id.clone(), backup);
            } else {
                state.subentry_flows.remove(&lease.flow_id);
            }
            Ok(())
        });
    }

    fn commit_async_subentry_flow_result(
        &self,
        lease: SubentryFlowLease,
        request: ConfigSubentryFlowRequest,
        result: ConfigSubentryFlowResult,
    ) -> Result<Value> {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        match result {
            result @ ConfigSubentryFlowResult::Form { .. } => {
                let ConfigSubentryFlowResult::Form { step_id, .. } = &result else {
                    unreachable!("form result matched above")
                };
                let mut stored_request = request.clone();
                stored_request.step_id = step_id.clone();
                stored_request.user_input = None;
                let result_json = subentry_flow_result_json(&request, &result, None);
                let _transition = self
                    .flow_transitions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                self.mutate(|state| {
                    let flow = state
                        .subentry_flows
                        .get_mut(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                    flow.request = stored_request;
                    flow.result = result_json.clone();
                    flow.phase = FlowPhase::Ready;
                    flow.revision = flow
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Service("subentry flow revision exhausted".into()))?;
                    Ok(())
                })?;
                drop(_transition);
                drop(control);
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            result @ ConfigSubentryFlowResult::Abort { .. } => {
                let result_json = subentry_flow_result_json(&request, &result, None);
                let _transition = self
                    .flow_transitions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                self.mutate(|state| {
                    let flow = state
                        .subentry_flows
                        .get(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                    state.subentry_flows.remove(&lease.flow_id);
                    Ok(())
                })?;
                drop(_transition);
                drop(control);
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            result @ ConfigSubentryFlowResult::CreateEntry { .. } => {
                let _transition = self
                    .flow_transitions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                {
                    let state = self.lock_state();
                    let flow = state
                        .subentry_flows
                        .get(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                }
                let result_json = match self.finish_subentry_flow(&request, &result) {
                    Ok(subentry) => subentry_flow_result_json(&request, &result, Some(subentry)),
                    Err(Error::InvalidInput(message)) if message == SUBENTRY_ALREADY_CONFIGURED => {
                        let duplicate = ConfigSubentryFlowResult::Abort {
                            reason: "already_configured".into(),
                        };
                        subentry_flow_result_json(&request, &duplicate, None)
                    }
                    Err(error) => return Err(error),
                };
                self.mutate(|state| {
                    let flow = state
                        .subentry_flows
                        .get(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                    state.subentry_flows.remove(&lease.flow_id);
                    Ok(())
                })?;
                drop(_transition);
                drop(control);
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            result @ ConfigSubentryFlowResult::UpdateAndAbort { .. } => {
                let _transition = self
                    .flow_transitions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                {
                    let state = self.lock_state();
                    let flow = state
                        .subentry_flows
                        .get(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                }
                let (reason, reload) = match self
                    .finish_subentry_reconfigure_without_reload(&request, &result)
                {
                    Ok(value) => value,
                    Err(Error::InvalidInput(message)) if message == SUBENTRY_ALREADY_CONFIGURED => {
                        ("already_configured".into(), false)
                    }
                    Err(error) => return Err(error),
                };
                let abort = ConfigSubentryFlowResult::Abort { reason };
                let result_json = subentry_flow_result_json(&request, &abort, None);
                self.mutate(|state| {
                    let flow = state
                        .subentry_flows
                        .get(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config subentry flow".into()))?;
                    ensure_subentry_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                    state.subentry_flows.remove(&lease.flow_id);
                    Ok(())
                })?;
                drop(_transition);
                drop(control);
                if reload {
                    let _ = self.reload_entry(&request.entry_id);
                }
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
        }
    }

    fn restore_async_flow_after_error(
        &self,
        lease: &ExternalFlowLease,
        backup: Option<FlowSession>,
    ) {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return;
        }
        let _ = self.mutate(|state| {
            let Some(current) = state.flows.get(&lease.flow_id) else {
                return Ok(());
            };
            if ensure_flow_matches_lease(current, lease, FlowPhase::Running).is_err() {
                return Ok(());
            }
            if let Some(mut backup) = backup {
                backup.phase = FlowPhase::Ready;
                backup.revision = current.revision.saturating_add(1);
                backup.request.user_input = None;
                state.flows.insert(lease.flow_id.clone(), backup);
            } else {
                state.flows.remove(&lease.flow_id);
            }
            Ok(())
        });
    }

    fn commit_async_flow_result(
        &self,
        lease: ExternalFlowLease,
        request: ConfigFlowRequest,
        kind: FlowKind,
        result: ConfigFlowResult,
    ) -> Result<Value> {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        match result {
            result @ (ConfigFlowResult::Form { .. }
            | ConfigFlowResult::ExternalStep { .. }
            | ConfigFlowResult::ExternalDone { .. }) => {
                let (step_id, phase) = match &result {
                    ConfigFlowResult::Form { step_id, .. } => (step_id.clone(), FlowPhase::Ready),
                    ConfigFlowResult::ExternalStep { step_id, .. } => {
                        (step_id.clone(), FlowPhase::AwaitingExternalCallback)
                    }
                    ConfigFlowResult::ExternalDone { step_id } => {
                        (step_id.clone(), FlowPhase::ExternalDone)
                    }
                    _ => unreachable!(),
                };
                validate_flow_step(&step_id)?;
                let mut stored_request = request.clone();
                stored_request.step_id = step_id;
                stored_request.user_input = None;
                let result_json = flow_result_json(&request, kind, &result, None, None);
                let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
                ensure_lease_driver(&registrations, &lease)?;
                let discovery_entry = self.mutate(|state| {
                    let flow = state
                        .flows
                        .get_mut(&lease.flow_id)
                        .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
                    ensure_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
                    if flow.kind != kind {
                        return Err(Error::InvalidInput(
                            "async config flow type mismatch".into(),
                        ));
                    }
                    flow.request = stored_request;
                    flow.result = result_json.clone();
                    flow.phase = phase;
                    flow.revision = flow
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Service("config flow revision exhausted".into()))?;
                    if kind != FlowKind::Config {
                        return Ok(None);
                    }
                    let Some(key) = discovery_key_from_context(&request.context) else {
                        return Ok(None);
                    };
                    let Some(unique_id) = request.unique_id.as_deref() else {
                        return Ok(None);
                    };
                    let Some(entry_id) = state
                        .entries
                        .values()
                        .find(|entry| {
                            entry.domain == request.domain
                                && entry.unique_id.as_deref() == Some(unique_id)
                        })
                        .map(|entry| entry.entry_id.clone())
                    else {
                        return Ok(None);
                    };
                    let entry = state
                        .entries
                        .get_mut(&entry_id)
                        .expect("discovery entry was found in the same mutation");
                    if record_discovery_key(entry, key) {
                        entry.modified_at = Utc::now();
                        Ok(Some(entry.clone()))
                    } else {
                        Ok(None)
                    }
                })?;
                drop(control);
                drop(registrations);
                if let Some(entry) = discovery_entry {
                    self.emit_public_entry_change("updated", &entry)?;
                }
                self.notify_flow_progress(&lease.flow_id)?;
                Ok(result_json)
            }
            ConfigFlowResult::Abort { .. } | ConfigFlowResult::CreateEntry { .. } => {
                drop(control);
                let plan = self.commit_running_flow_completion(lease, result)?;
                self.finish_external_flow(plan)
            }
        }
    }

    fn commit_running_flow_completion(
        &self,
        lease: ExternalFlowLease,
        result: ConfigFlowResult,
    ) -> Result<DeferredExternalFlowCompletion> {
        let control = self
            .flow_tasks
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if control.closed {
            return Err(Error::Service(
                "config flow manager is shutting down".into(),
            ));
        }
        let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
        ensure_lease_driver(&registrations, &lease)?;
        let registration = registrations
            .drivers
            .get(&lease.domain)
            .ok_or_else(|| Error::NotFound("config flow driver is not registered".into()))?;
        let driver = Arc::clone(&registration.driver);
        let (request, kind, entry, action) = self.mutate(|state| {
            let flow = state
                .flows
                .get(&lease.flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
            ensure_flow_matches_lease(flow, &lease, FlowPhase::Running)?;
            let kind = flow.kind;
            let request = flow.request.clone();
            let (entry, action) = match &result {
                ConfigFlowResult::Abort { .. } => (None, DeferredFlowAction::Abort),
                ConfigFlowResult::CreateEntry {
                    title,
                    data,
                    options,
                    unique_id,
                    version,
                    minor_version,
                } if kind == FlowKind::Options => {
                    let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                        Error::InvalidInput("options flow is missing entry_id".into())
                    })?;
                    let current = state
                        .entries
                        .get_mut(entry_id)
                        .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
                    if current.domain != request.domain {
                        return Err(Error::InvalidInput("options flow domain mismatch".into()));
                    }
                    let _ = (title, data, unique_id, version, minor_version);
                    current.options = options.clone();
                    current.modified_at = Utc::now();
                    (Some(current.clone()), DeferredFlowAction::Options)
                }
                ConfigFlowResult::CreateEntry {
                    title,
                    data,
                    options,
                    unique_id,
                    version,
                    minor_version,
                } => match request.source.as_str() {
                    "reauth" | "reconfigure" => {
                        let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                            Error::InvalidInput(format!(
                                "{} flow is missing entry_id",
                                request.source
                            ))
                        })?;
                        let snapshot = state
                            .entries
                            .get(entry_id)
                            .cloned()
                            .ok_or_else(|| Error::NotFound("Config entry not found".into()))?;
                        if snapshot.domain != request.domain {
                            return Err(Error::InvalidInput("config flow domain mismatch".into()));
                        }
                        let next_unique = unique_id.as_deref().or(snapshot.unique_id.as_deref());
                        ensure_unique_id(
                            &state.entries,
                            &snapshot.domain,
                            next_unique,
                            Some(entry_id),
                        )?;
                        let current = state
                            .entries
                            .get_mut(entry_id)
                            .expect("the existing config entry was read from this mutation");
                        current.title = title.trim().to_owned();
                        current.data = data.clone();
                        current.options = options.clone();
                        current.unique_id = next_unique.map(str::to_owned);
                        current.version = *version;
                        current.minor_version = *minor_version;
                        current.needs_reauth = false;
                        current.modified_at = Utc::now();
                        let committed = current.clone();
                        (
                            Some(committed),
                            if request.source == "reauth" {
                                DeferredFlowAction::Reauth
                            } else {
                                DeferredFlowAction::Reconfigure
                            },
                        )
                    }
                    _ => {
                        let mut seed = NewConfigEntry::new(&request.domain, title, data.clone());
                        seed.source = request.source.clone();
                        seed.unique_id = unique_id.clone().or_else(|| request.unique_id.clone());
                        seed.version = *version;
                        seed.minor_version = *minor_version;
                        seed.options = options.clone();
                        validate_new_entry(&seed)?;
                        let entry = new_entry_from_seed(
                            seed,
                            Uuid::new_v4().to_string(),
                            &driver,
                            discovery_key_from_context(&request.context),
                        );
                        ensure_unique_id(
                            &state.entries,
                            &entry.domain,
                            entry.unique_id.as_deref(),
                            None,
                        )?;
                        state.entries.insert(entry.entry_id.clone(), entry.clone());
                        (Some(entry), DeferredFlowAction::Create)
                    }
                },
                _ => {
                    return Err(Error::InvalidInput(
                        "async flow completion accepts only CreateEntry or Abort".into(),
                    ));
                }
            };
            state.flows.remove(&lease.flow_id);
            Ok((request, kind, entry, action))
        })?;
        drop(control);
        drop(registrations);
        Ok(DeferredExternalFlowCompletion {
            request,
            kind,
            result,
            entry,
            driver,
            action,
        })
    }

    fn run_flow_step(
        &self,
        request: ConfigFlowRequest,
        kind: FlowKind,
        driver: Arc<dyn ConfigEntryDriver>,
        initial_data: Option<Map<String, Value>>,
        data_kind: Option<DiscoveryDataKind>,
    ) -> Result<Value> {
        self.run_flow_step_with_lease(
            request,
            kind,
            driver,
            initial_data,
            data_kind,
            FlowStepGuard {
                lease: None,
                backup: None,
            },
        )
    }

    fn run_flow_step_with_lease(
        &self,
        request: ConfigFlowRequest,
        kind: FlowKind,
        driver: Arc<dyn ConfigEntryDriver>,
        initial_data: Option<Map<String, Value>>,
        data_kind: Option<DiscoveryDataKind>,
        guard: FlowStepGuard,
    ) -> Result<Value> {
        let _admission = self.begin_flow_admission()?;
        let FlowStepGuard { lease, backup } = guard;
        let (registered_driver, driver_generation) = self
            .driver_registration(&request.domain)
            .ok_or_else(|| Error::NotFound("config flow driver is no longer registered".into()))?;
        if !Arc::ptr_eq(&registered_driver, &driver) {
            return Err(Error::NotFound(
                "config flow driver registration changed".into(),
            ));
        }
        let result = match kind {
            FlowKind::Config => driver.config_flow(&request),
            FlowKind::Options => driver.options_flow(&request),
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(lease) = lease.as_ref() {
                    self.restore_async_flow_after_error(lease, backup);
                }
                return Err(error);
            }
        };
        if lease.is_none()
            && kind == FlowKind::Config
            && !matches!(&result, ConfigFlowResult::CreateEntry { .. })
        {
            self.associate_flow_discovery_key(&request)?;
        }
        match &result {
            ConfigFlowResult::Form { step_id, .. }
            | ConfigFlowResult::ExternalStep { step_id, .. }
            | ConfigFlowResult::ExternalDone { step_id } => {
                let mut stored_request = request.clone();
                stored_request.step_id = step_id.clone();
                stored_request.user_input = None;
                let flow_id = request.flow_id.clone();
                let result_json = flow_result_json(&request, kind, &result, None, None);
                let phase = match result {
                    ConfigFlowResult::ExternalStep { .. } => FlowPhase::AwaitingExternalCallback,
                    ConfigFlowResult::ExternalDone { .. } => FlowPhase::ExternalDone,
                    _ => FlowPhase::Ready,
                };
                let _registered = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
                let current = _registered.drivers.get(&request.domain).ok_or_else(|| {
                    Error::NotFound("config flow driver registration changed".into())
                })?;
                if current.generation != driver_generation || !Arc::ptr_eq(&current.driver, &driver)
                {
                    return Err(Error::NotFound(
                        "config flow driver registration changed".into(),
                    ));
                }
                self.mutate(|state| {
                    if let Some(lease) = lease.as_ref() {
                        let flow = state
                            .flows
                            .get_mut(&lease.flow_id)
                            .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
                        ensure_flow_matches_lease(flow, lease, FlowPhase::Running)?;
                        flow.request = stored_request;
                        flow.result = result_json.clone();
                        flow.phase = phase;
                        flow.revision = flow.revision.checked_add(1).ok_or_else(|| {
                            Error::Service("config flow revision exhausted".into())
                        })?;
                    } else {
                        state.flows.insert(
                            flow_id.clone(),
                            FlowSession {
                                request: stored_request,
                                kind,
                                initial_data: initial_data.clone(),
                                data_kind,
                                result: result_json.clone(),
                                phase,
                                revision: 1,
                                driver_generation,
                                driver: Arc::downgrade(&driver),
                            },
                        );
                    }
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            ConfigFlowResult::Abort { .. } => {
                if let Some(lease) = lease {
                    let plan = self.commit_running_flow_completion(lease, result.clone())?;
                    return self.finish_external_flow(plan);
                }
                self.mutate(|state| {
                    state.flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(&request, kind, &result, None, None))
            }
            ConfigFlowResult::CreateEntry { .. } => {
                if let Some(lease) = lease {
                    let plan = self.commit_running_flow_completion(lease, result.clone())?;
                    return self.finish_external_flow(plan);
                }
                let entry = self.finish_flow(&request, kind, &result)?;
                let next_flow = if kind == FlowKind::Config
                    && !matches!(request.source.as_str(), "reauth" | "reconfigure")
                {
                    let next_flow = driver.on_create_entry(self, &request, &entry, &result)?;
                    if let Some(next_flow) = next_flow.as_ref() {
                        self.validate_flow_continuation(next_flow)?;
                    }
                    next_flow
                } else {
                    None
                };
                self.mutate(|state| {
                    state.flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(
                    &request,
                    kind,
                    &result,
                    Some(entry),
                    next_flow,
                ))
            }
        }
    }

    fn validate_flow_continuation(&self, continuation: &ConfigFlowContinuation) -> Result<()> {
        let state = self.lock_state();
        let valid = match continuation.flow_type {
            ConfigFlowType::Config => state
                .flows
                .get(&continuation.flow_id)
                .is_some_and(|flow| flow.kind == FlowKind::Config),
            ConfigFlowType::Options => state
                .flows
                .get(&continuation.flow_id)
                .is_some_and(|flow| flow.kind == FlowKind::Options),
            ConfigFlowType::ConfigSubentries => {
                state.subentry_flows.contains_key(&continuation.flow_id)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidInput(format!(
                "on_create_entry returned an unknown {:?} flow {}",
                continuation.flow_type, continuation.flow_id
            )))
        }
    }

    fn run_subentry_flow_step(
        &self,
        request: ConfigSubentryFlowRequest,
        driver: Arc<dyn ConfigEntryDriver>,
    ) -> Result<Value> {
        let _admission = self.begin_flow_admission()?;
        let (registered, driver_generation) =
            self.driver_registration(&request.domain).ok_or_else(|| {
                Error::NotFound("subentry flow driver is no longer registered".into())
            })?;
        if !Arc::ptr_eq(&registered, &driver) {
            return Err(Error::NotFound(
                "subentry flow driver registration changed".into(),
            ));
        }
        let result = driver.subentry_flow(&request)?;
        match &result {
            ConfigSubentryFlowResult::Form { step_id, .. } => {
                let mut stored_request = request.clone();
                stored_request.step_id = step_id.clone();
                stored_request.user_input = None;
                let flow_id = request.flow_id.clone();
                let result_json = subentry_flow_result_json(&request, &result, None);
                let registrations = self.drivers.read().unwrap_or_else(PoisonError::into_inner);
                let current_registration =
                    registrations.drivers.get(&request.domain).ok_or_else(|| {
                        Error::NotFound("subentry flow driver is no longer registered".into())
                    })?;
                if current_registration.generation != driver_generation
                    || !Arc::ptr_eq(&current_registration.driver, &driver)
                {
                    return Err(Error::NotFound(
                        "subentry flow driver registration changed".into(),
                    ));
                }
                self.mutate(|state| {
                    if state.flows.contains_key(&flow_id) {
                        return Err(Error::InvalidInput("config flow ID collision".into()));
                    }
                    let revision = state
                        .subentry_flows
                        .get(&flow_id)
                        .map(|flow| flow.revision)
                        .unwrap_or(0)
                        .checked_add(1)
                        .ok_or_else(|| Error::Service("subentry flow revision exhausted".into()))?;
                    state.subentry_flows.insert(
                        flow_id.clone(),
                        ConfigSubentryFlowSession {
                            request: stored_request,
                            result: result_json.clone(),
                            phase: FlowPhase::Ready,
                            revision,
                            driver_generation,
                            driver: Arc::downgrade(&driver),
                        },
                    );
                    Ok(())
                })?;
                drop(registrations);
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            ConfigSubentryFlowResult::Abort { .. } => {
                let result_json = subentry_flow_result_json(&request, &result, None);
                self.mutate(|state| {
                    state.subentry_flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            ConfigSubentryFlowResult::CreateEntry { .. } => {
                let subentry = match self.finish_subentry_flow(&request, &result) {
                    Ok(subentry) => subentry,
                    Err(Error::InvalidInput(message)) if message == SUBENTRY_ALREADY_CONFIGURED => {
                        let duplicate = ConfigSubentryFlowResult::Abort {
                            reason: "already_configured".into(),
                        };
                        let result_json = subentry_flow_result_json(&request, &duplicate, None);
                        self.mutate(|state| {
                            state.subentry_flows.remove(&request.flow_id);
                            Ok(())
                        })?;
                        self.notify_flow_progress(&request.flow_id)?;
                        return Ok(result_json);
                    }
                    Err(error) => return Err(error),
                };
                let result_json = subentry_flow_result_json(&request, &result, Some(subentry));
                self.mutate(|state| {
                    state.subentry_flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            ConfigSubentryFlowResult::UpdateAndAbort { .. } => {
                let reason = match self.finish_subentry_reconfigure(&request, &result) {
                    Ok(reason) => reason,
                    Err(Error::InvalidInput(message)) if message == SUBENTRY_ALREADY_CONFIGURED => {
                        "already_configured".into()
                    }
                    Err(error) => return Err(error),
                };
                let abort = ConfigSubentryFlowResult::Abort { reason };
                let result_json = subentry_flow_result_json(&request, &abort, None);
                self.mutate(|state| {
                    state.subentry_flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
        }
    }

    fn finish_subentry_flow(
        &self,
        request: &ConfigSubentryFlowRequest,
        result: &ConfigSubentryFlowResult,
    ) -> Result<ConfigSubentry> {
        let ConfigSubentryFlowResult::CreateEntry {
            title,
            data,
            unique_id,
        } = result
        else {
            return Err(Error::InvalidInput(
                "subentry flow did not create a subentry".into(),
            ));
        };
        if request.source != "user" || request.subentry_id.is_some() {
            return Err(Error::InvalidInput(
                "subentry creation requires a new user flow".into(),
            ));
        }
        let mut seed = NewConfigSubentry::new(&request.subentry_type, title, data.clone());
        seed.unique_id = unique_id.clone();
        self.create_subentry(&request.entry_id, seed)
    }

    fn finish_subentry_reconfigure(
        &self,
        request: &ConfigSubentryFlowRequest,
        result: &ConfigSubentryFlowResult,
    ) -> Result<String> {
        let (reason, reload) = self.finish_subentry_reconfigure_without_reload(request, result)?;
        if reload {
            // The pinned flow schedules the parent reload independently of its
            // abort result. This synchronous manager completes that reload and
            // leaves any setup failure visible on the parent entry.
            let _ = self.reload_entry(&request.entry_id);
        }
        Ok(reason)
    }

    fn finish_subentry_reconfigure_without_reload(
        &self,
        request: &ConfigSubentryFlowRequest,
        result: &ConfigSubentryFlowResult,
    ) -> Result<(String, bool)> {
        let ConfigSubentryFlowResult::UpdateAndAbort {
            update,
            data_updates,
            reason,
            reload,
        } = result
        else {
            return Err(Error::InvalidInput(
                "subentry reconfigure flow did not update a subentry".into(),
            ));
        };
        if request.source != "reconfigure" {
            return Err(Error::InvalidInput(
                "subentry update requires a reconfigure flow".into(),
            ));
        }
        let subentry_id = request.subentry_id.as_deref().ok_or_else(|| {
            Error::InvalidInput("subentry reconfigure flow is missing subentry_id".into())
        })?;
        let mut update = update.clone();
        if let Some(data_updates) = data_updates {
            if update.data.is_some() {
                return Err(Error::InvalidInput(
                    "cannot set both replacement data and data_updates".into(),
                ));
            }
            let mut data = self
                .get_subentry(&request.entry_id, subentry_id)
                .ok_or_else(|| Error::NotFound("Config subentry not found".into()))?
                .data;
            data.extend(data_updates.clone());
            update.data = Some(data);
        }
        self.update_subentry(&request.entry_id, subentry_id, update)?;
        Ok((
            reason
                .clone()
                .unwrap_or_else(|| "reconfigure_successful".into()),
            *reload,
        ))
    }

    fn finish_flow(
        &self,
        request: &ConfigFlowRequest,
        kind: FlowKind,
        result: &ConfigFlowResult,
    ) -> Result<ConfigEntry> {
        let ConfigFlowResult::CreateEntry {
            title,
            data,
            options,
            unique_id,
            version,
            minor_version,
        } = result
        else {
            return Err(Error::InvalidInput("flow did not create an entry".into()));
        };
        if title.trim().is_empty() || title.chars().count() > 255 || *version == 0 {
            return Err(Error::InvalidInput(
                "flow title and version are invalid".into(),
            ));
        }
        if let Some(unique_id) = unique_id.as_deref() {
            validate_unique_id(unique_id)?;
        }
        match kind {
            FlowKind::Options => {
                let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                    Error::InvalidInput("options flow is missing entry_id".into())
                })?;
                let entry = self.require_entry(entry_id)?;
                if entry.domain != request.domain {
                    return Err(Error::InvalidInput("options flow domain mismatch".into()));
                }
                self.update_options(entry_id, options.clone())
            }
            FlowKind::Config if matches!(request.source.as_str(), "reauth" | "reconfigure") => {
                let entry_id = request.entry_id.as_deref().ok_or_else(|| {
                    Error::InvalidInput(format!("{} flow is missing entry_id", request.source))
                })?;
                let original = self.require_entry(entry_id)?;
                let new_unique = unique_id.as_deref().or(original.unique_id.as_deref());
                ensure_unique_id(
                    &self.lock_state().entries,
                    &original.domain,
                    new_unique,
                    Some(entry_id),
                )?;
                self.update_entry_metadata(entry_id, |entry| {
                    entry.title = title.trim().to_owned();
                    entry.data = data.clone();
                    entry.options = options.clone();
                    entry.unique_id = new_unique.map(str::to_owned);
                    entry.version = *version;
                    entry.minor_version = *minor_version;
                    entry.needs_reauth = false;
                    entry.modified_at = Utc::now();
                })?;
                self.reload_entry(entry_id)?;
                self.require_entry(entry_id)
            }
            FlowKind::Config => {
                let mut seed = NewConfigEntry::new(&request.domain, title, data.clone());
                seed.source = request.source.clone();
                seed.unique_id = unique_id.clone().or_else(|| request.unique_id.clone());
                seed.version = *version;
                seed.minor_version = *minor_version;
                seed.options = options.clone();
                let discovery_key = discovery_key_from_context(&request.context);
                let entry = self.create_entry_with_discovery_key(seed, discovery_key)?;
                // Flow success is independent of setup availability; the entry
                // remains visible with its honest setup_error/setup_retry state.
                let _ = self.setup_entry(&entry.entry_id);
                self.require_entry(&entry.entry_id)
            }
        }
    }

    fn flow_progress(&self) -> Vec<Value> {
        self.lock_state()
            .flows
            .values()
            .filter(|flow| !matches!(flow.request.source.as_str(), "user" | "reconfigure"))
            .map(|flow| {
                let context = if matches!(
                    flow.phase,
                    FlowPhase::AwaitingExternalCallback
                        | FlowPhase::ResolvingExternalCallback
                        | FlowPhase::ExternalDone
                        | FlowPhase::ResolvingExternalDone
                ) {
                    ["entry_id", "source"]
                        .into_iter()
                        .filter_map(|key| {
                            flow.request
                                .context
                                .get(key)
                                .cloned()
                                .map(|value| (key.to_owned(), value))
                        })
                        .collect::<Map<String, Value>>()
                } else {
                    public_flow_context(&flow.request.context)
                };
                json!({
                    "flow_id": flow.request.flow_id,
                    "handler": flow.request.domain,
                    "context": context,
                    "step_id": flow.request.step_id,
                    "type": match flow.kind { FlowKind::Config => "config_flow", FlowKind::Options => "options_flow" },
                })
            })
            .collect()
    }

    fn migrate_entry_locked(
        &self,
        entry_id: &str,
        driver: &Arc<dyn ConfigEntryDriver>,
        target_version: u32,
        target_minor_version: u32,
    ) -> Result<()> {
        let entry = self.require_entry(entry_id)?;
        let current = (entry.version, entry.minor_version);
        let target = (target_version, target_minor_version);
        if target < current {
            let error = Error::InvalidInput(format!(
                "integration {} cannot downgrade entry {} from {}.{} to {}.{}",
                entry.domain, entry.entry_id, current.0, current.1, target.0, target.1
            ));
            self.update_entry_state(
                entry_id,
                ConfigEntryState::MigrationError,
                Some(error.to_string()),
                None,
            )?;
            return Err(error);
        }
        if target == current {
            return Ok(());
        }
        match driver.migrate(&entry, target_version, target_minor_version) {
            Ok(data) => self.update_entry_metadata(entry_id, |current| {
                current.data = data;
                current.version = target_version;
                current.minor_version = target_minor_version;
                current.reason = None;
                current.modified_at = Utc::now();
            }),
            Err(error) => {
                self.update_entry_state(
                    entry_id,
                    ConfigEntryState::MigrationError,
                    Some(error.to_string()),
                    None,
                )?;
                Err(error)
            }
        }
    }

    fn reload_entry_locked(&self, entry_id: &str) -> Result<()> {
        let mut notification_error = None;
        let result = self.reload_entry_with_notifications(entry_id, &mut notification_error);
        finish_lifecycle_notifications(result, notification_error).map(|_| ())
    }

    fn setup_entry_locked(&self, entry_id: &str) -> Result<ConfigEntry> {
        let mut notification_error = None;
        let result = self.setup_entry_with_notifications(entry_id, &mut notification_error);
        finish_lifecycle_notifications(result, notification_error)
    }

    fn setup_entry_with_notifications(
        &self,
        entry_id: &str,
        notification_error: &mut Option<Error>,
    ) -> Result<ConfigEntry> {
        // `setup_entry` owns the same operation lock, so its implementation is
        // mirrored here for callers already holding the lifecycle lock.
        let entry = self.require_entry(entry_id)?;
        if entry.state.is_loaded() {
            return Ok(entry);
        }
        if entry.disabled_by.is_some() {
            return Err(Error::InvalidInput(format!(
                "config entry {entry_id} is disabled"
            )));
        }
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        });
        let driver = match driver {
            Ok(driver) => driver,
            Err(error) => {
                self.update_lifecycle_state(
                    notification_error,
                    entry_id,
                    ConfigEntryState::SetupError,
                    Some(error.to_string()),
                    None,
                )?;
                return Err(error);
            }
        };
        let (version, minor) = driver.version();
        if (version, minor) != (entry.version, entry.minor_version) {
            self.migrate_entry_locked(entry_id, &driver, version, minor)?;
        }
        self.update_lifecycle_state(
            notification_error,
            entry_id,
            ConfigEntryState::SetupInProgress,
            None,
            None,
        )?;
        let entry = self.require_entry(entry_id)?;
        match driver.setup(&self.runtime, &entry) {
            Ok(SetupOutcome::Ready) => {
                self.update_lifecycle_state(
                    notification_error,
                    entry_id,
                    ConfigEntryState::Loaded,
                    None,
                    None,
                )?;
                self.update_lifecycle_metadata(notification_error, entry_id, |current| {
                    current.retry_attempts = 0;
                    current.retry_at = None;
                    current.require_reload = false;
                })?;
                self.require_entry(entry_id)
            }
            Ok(SetupOutcome::RetryAfter { seconds, reason }) => {
                if seconds == 0 || seconds > MAX_RETRY_DELAY_SECS {
                    let error = Error::InvalidInput(format!(
                        "retry delay must be between 1 and {MAX_RETRY_DELAY_SECS} seconds"
                    ));
                    self.update_lifecycle_state(
                        notification_error,
                        entry_id,
                        ConfigEntryState::SetupError,
                        Some(error.to_string()),
                        None,
                    )?;
                    return Err(error);
                }
                let retry_at = Utc::now() + Duration::seconds(seconds as i64);
                self.update_lifecycle_metadata(notification_error, entry_id, |current| {
                    current.state = ConfigEntryState::SetupRetry;
                    current.retry_attempts = current.retry_attempts.saturating_add(1);
                    current.retry_at = Some(retry_at);
                    current.reason = reason;
                    current.modified_at = Utc::now();
                })?;
                self.require_entry(entry_id)
            }
            Err(error) => {
                self.update_lifecycle_state(
                    notification_error,
                    entry_id,
                    ConfigEntryState::SetupError,
                    Some(error.to_string()),
                    None,
                )?;
                Err(error)
            }
        }
    }

    fn unload_entry_locked(&self, entry_id: &str) -> Result<()> {
        let mut notification_error = None;
        let result = self.unload_entry_with_notifications(entry_id, &mut notification_error);
        finish_lifecycle_notifications(result, notification_error)
    }

    fn unload_entry_with_notifications(
        &self,
        entry_id: &str,
        notification_error: &mut Option<Error>,
    ) -> Result<()> {
        let entry = self.require_entry(entry_id)?;
        if !entry.state.is_loaded() {
            return Ok(());
        }
        let driver = self.driver(&entry.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                entry.domain
            ))
        });
        let driver = match driver {
            Ok(driver) => driver,
            Err(error) => {
                self.update_lifecycle_state(
                    notification_error,
                    entry_id,
                    ConfigEntryState::FailedUnload,
                    Some(error.to_string()),
                    None,
                )?;
                return Err(error);
            }
        };
        self.update_lifecycle_state(
            notification_error,
            entry_id,
            ConfigEntryState::UnloadInProgress,
            None,
            None,
        )?;
        let entry = self.require_entry(entry_id)?;
        match driver.unload(&self.runtime, &entry) {
            Ok(()) => self.update_lifecycle_state(
                notification_error,
                entry_id,
                ConfigEntryState::NotLoaded,
                None,
                None,
            ),
            Err(error) => {
                self.update_lifecycle_state(
                    notification_error,
                    entry_id,
                    ConfigEntryState::FailedUnload,
                    Some(error.to_string()),
                    None,
                )?;
                Err(error)
            }
        }
    }

    fn update_entry_state(
        &self,
        entry_id: &str,
        state_value: ConfigEntryState,
        reason: Option<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.update_entry_metadata(entry_id, |entry| {
            entry.state = state_value;
            if state_value == ConfigEntryState::FailedUnload {
                entry.require_reload = true;
            }
            entry.reason = reason;
            entry.retry_at = retry_at;
            entry.modified_at = Utc::now();
        })
    }

    fn update_entry_metadata(
        &self,
        entry_id: &str,
        update: impl FnOnce(&mut ConfigEntry),
    ) -> Result<()> {
        let mut emitted = None;
        self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            update(entry);
            emitted = Some(entry.clone());
            Ok(())
        })?;
        if let Some(entry) = emitted {
            self.emit_entry_change("updated", &entry)?;
        }
        Ok(())
    }

    fn update_lifecycle_state(
        &self,
        notification_error: &mut Option<Error>,
        entry_id: &str,
        state_value: ConfigEntryState,
        reason: Option<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.update_lifecycle_metadata(notification_error, entry_id, |entry| {
            entry.state = state_value;
            if state_value == ConfigEntryState::FailedUnload {
                entry.require_reload = true;
            }
            entry.reason = reason;
            entry.retry_at = retry_at;
            entry.modified_at = Utc::now();
        })
    }

    // Persistence failures stop the lifecycle. Event-delivery failures are
    // reported after the driver and its final persisted state have completed.
    fn update_lifecycle_metadata(
        &self,
        notification_error: &mut Option<Error>,
        entry_id: &str,
        update: impl FnOnce(&mut ConfigEntry),
    ) -> Result<()> {
        let entry = self.mutate(|state| {
            let entry = state
                .entries
                .get_mut(entry_id)
                .ok_or_else(|| Error::NotFound(format!("unknown config entry {entry_id}")))?;
            update(entry);
            Ok(entry.clone())
        })?;
        if let Err(error) = self.emit_entry_change("updated", &entry) {
            notification_error.get_or_insert(error);
        }
        Ok(())
    }

    fn require_entry(&self, entry_id: &str) -> Result<ConfigEntry> {
        self.get_entry(entry_id)
            .ok_or_else(|| Error::NotFound("Config entry not found".into()))
    }

    fn mutate<T>(&self, operation: impl FnOnce(&mut PersistedState) -> Result<T>) -> Result<T> {
        let mut current = self.lock_state();
        let mut next = current.clone();
        let result = operation(&mut next)?;
        self.persist(&next)?;
        *current = next;
        Ok(result)
    }

    fn dispatch_direct_cleanup(&self, pending: &PendingCleanup) -> Result<()> {
        match self.dispatch_pending_cleanup(pending)? {
            CleanupDispatch::InFlight => Err(Error::Service(
                "config cleanup is already being dispatched".into(),
            )),
            CleanupDispatch::Acknowledged | CleanupDispatch::Pending => Ok(()),
        }
    }

    fn dispatch_pending_cleanup(&self, pending: &PendingCleanup) -> Result<CleanupDispatch> {
        // Reserve only while the record is still current. The state and
        // in-flight locks are both released before any observer is invoked.
        let state = self.lock_state();
        if !state
            .pending_cleanup
            .iter()
            .any(|candidate| candidate.id == pending.id)
        {
            return Ok(CleanupDispatch::Pending);
        }
        let mut in_flight = self
            .cleanup_in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !in_flight.insert(pending.id) {
            return Ok(CleanupDispatch::InFlight);
        }
        drop(in_flight);
        drop(state);
        let _flight_guard = CleanupFlightGuard {
            manager: self,
            id: pending.id,
        };

        let delivered =
            self.notify_lifecycle_observers_with_presence(&pending.lifecycle_event())?;
        if !delivered {
            return Ok(CleanupDispatch::Pending);
        }
        if self.acknowledge_pending_cleanup(pending.id)? {
            Ok(CleanupDispatch::Acknowledged)
        } else {
            // A parent record may have superseded a child while the callback
            // was running. Its cleanup record remains authoritative.
            Ok(CleanupDispatch::Pending)
        }
    }

    fn acknowledge_pending_cleanup(&self, id: Uuid) -> Result<bool> {
        let mut current = self.lock_state();
        if !current
            .pending_cleanup
            .iter()
            .any(|pending| pending.id == id)
        {
            return Ok(false);
        }
        let mut next = current.clone();
        next.pending_cleanup.retain(|pending| pending.id != id);
        self.persist(&next).map_err(|error| {
            Error::Storage(format!(
                "config cleanup succeeded but its journal acknowledgement failed: {error}"
            ))
        })?;
        *current = next;
        Ok(true)
    }

    fn persist(&self, state: &PersistedState) -> Result<()> {
        let document = PersistedDocument {
            document_version: DOCUMENT_VERSION,
            entries: state.entries.values().cloned().collect(),
            core_config: state.core_config.clone(),
            core_config_initialized: state.core_config_initialized,
            pending_cleanup: state.pending_cleanup.clone(),
        };
        let value = serde_json::to_value(document).map_err(|error| {
            Error::InvalidInput(format!("cannot encode config metadata: {error}"))
        })?;
        self.runtime.set_data(METADATA_KEY, &value)
    }

    fn emit_current_entry_change(&self, change: &str, entry_id: &str) -> Result<()> {
        let snapshot = { self.lock_state().entries.get(entry_id).cloned() };
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        // Driver capability callbacks remain outside the state lock. Recheck
        // the raw snapshot before sending so parent removal cannot be followed
        // by a stale child-update announcement, including concurrent removal.
        let entry = self.with_supported_subentry_types(snapshot.clone());
        let mut data = Map::new();
        data.insert("type".into(), json!(change));
        data.insert("entry".into(), entry.public_json());
        let current = self.lock_state();
        if current.entries.get(entry_id) != Some(&snapshot) {
            return Ok(());
        }
        self.runtime
            .fire_event("config_entry_changed", data, Context::default())?;
        Ok(())
    }

    fn emit_entry_change(&self, change: &str, entry: &ConfigEntry) -> Result<()> {
        let entry = self.with_supported_subentry_types(entry.clone());
        self.emit_public_entry_change(change, &entry)
    }

    fn emit_public_entry_change(&self, change: &str, entry: &ConfigEntry) -> Result<()> {
        let mut data = Map::new();
        data.insert("type".into(), json!(change));
        data.insert("entry".into(), entry.public_json());
        self.runtime
            .fire_event("config_entry_changed", data, Context::default())?;
        Ok(())
    }

    fn notify_discovery_key_removal(&self, entry: &ConfigEntry) {
        let keys = entry
            .discovery_keys
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return;
        }
        let event = DiscoveryKeyRemoval {
            entry_id: entry.entry_id.clone(),
            keys,
        };
        let observers = self
            .discovery_key_removal_observers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for observer in observers {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.discovery_keys_removed(&event)));
        }
    }

    fn notify_lifecycle_observers(&self, event: &ConfigLifecycleEvent) -> Result<()> {
        self.notify_lifecycle_observers_with_presence(event)
            .map(|_| ())
    }

    fn notify_lifecycle_observers_with_presence(
        &self,
        event: &ConfigLifecycleEvent,
    ) -> Result<bool> {
        let observers = self
            .lifecycle_observers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let had_observers = !observers.is_empty();
        let mut first_error = None;
        for observer in observers {
            if let Err(error) = observer.config_lifecycle_changed(event)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(Error::Service(format!(
                "config lifecycle committed but observer failed: {error}"
            ))),
            None => Ok(had_observers),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PersistedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_operation(&self) -> std::sync::MutexGuard<'_, ()> {
        self.operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn finish_lifecycle_notifications<T>(
    result: Result<T>,
    notification_error: Option<Error>,
) -> Result<T> {
    let value = result?;
    match notification_error {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

fn public_flow_context(context: &Map<String, Value>) -> Map<String, Value> {
    context
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.to_ascii_lowercase().as_str(),
                "state"
                    | "state_proof"
                    | "oauth_state"
                    | "code"
                    | "authorization_code"
                    | "code_verifier"
                    | "pkce"
                    | "pkce_verifier"
                    | "client_secret"
                    | "token"
                    | "token_data"
                    | "access_token"
                    | "refresh_token"
            )
        })
        .map(|(key, value)| (key.clone(), public_flow_context_value(value)))
        .collect()
}

fn public_flow_context_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(public_flow_context(object)),
        Value::Array(values) => {
            Value::Array(values.iter().map(public_flow_context_value).collect())
        }
        value => value.clone(),
    }
}

fn flow_result_json(
    request: &ConfigFlowRequest,
    kind: FlowKind,
    result: &ConfigFlowResult,
    entry: Option<ConfigEntry>,
    next_flow: Option<ConfigFlowContinuation>,
) -> Value {
    let handler = match kind {
        FlowKind::Config => json!(request.domain),
        FlowKind::Options => json!(request.entry_id.as_deref().unwrap_or(&request.domain)),
    };
    let mut value = match result {
        ConfigFlowResult::Form {
            step_id,
            data_schema,
            errors,
            description_placeholders,
        } => json!({
            "flow_id": request.flow_id,
            "type": "form",
            "handler": handler,
            "context": request.context,
            "step_id": step_id,
            "data_schema": data_schema,
            "description_placeholders": description_placeholders,
            "errors": errors,
            "last_step": null,
            "preview": null,
        }),
        ConfigFlowResult::ExternalStep {
            step_id,
            url,
            description_placeholders,
        } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "external",
            "url": url,
            "description_placeholders": description_placeholders,
            "step_id": step_id,
        }),
        ConfigFlowResult::ExternalDone { step_id } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "external_done",
            "step_id": step_id,
        }),
        ConfigFlowResult::CreateEntry {
            title,
            options,
            version,
            minor_version,
            ..
        } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "create_entry",
            "title": title,
            "version": version,
            "minor_version": minor_version,
            "result": entry.map(|entry| entry.public_json()),
            "description": null,
            "description_placeholders": null,
            "options": options,
            "subentries": [],
        }),
        ConfigFlowResult::Abort { reason } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "abort",
            "context": request.context,
            "reason": reason,
        }),
    };
    if let Some(next_flow) = next_flow {
        value["next_flow"] = json!([next_flow.flow_type.as_str(), next_flow.flow_id]);
    }
    value
}

fn subentry_flow_result_json(
    request: &ConfigSubentryFlowRequest,
    result: &ConfigSubentryFlowResult,
    subentry: Option<ConfigSubentry>,
) -> Value {
    let handler = json!([request.entry_id, request.subentry_type]);
    match result {
        ConfigSubentryFlowResult::Form {
            step_id,
            data_schema,
            errors,
            description_placeholders,
        } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "form",
            "context": request.context,
            "step_id": step_id,
            "data_schema": data_schema,
            "description_placeholders": description_placeholders,
            "errors": errors,
            "last_step": null,
            "preview": null,
        }),
        ConfigSubentryFlowResult::CreateEntry { title, .. } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "create_entry",
            "title": title,
            "result": subentry.map(|subentry| subentry.public_json()),
            "description": null,
            "description_placeholders": null,
        }),
        ConfigSubentryFlowResult::UpdateAndAbort { reason, .. } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "abort",
            "context": request.context,
            "reason": reason.as_deref().unwrap_or("reconfigure_successful"),
        }),
        ConfigSubentryFlowResult::Abort { reason } => json!({
            "flow_id": request.flow_id,
            "handler": handler,
            "type": "abort",
            "context": request.context,
            "reason": reason,
        }),
    }
}

const SUBENTRY_ALREADY_CONFIGURED: &str = "subentry unique ID already configured";

fn validate_new_entry(seed: &NewConfigEntry) -> Result<()> {
    validate_domain(&seed.domain)?;
    validate_source(&seed.source)?;
    if seed.title.trim().is_empty() || seed.title.chars().count() > 255 {
        return Err(Error::InvalidInput(
            "entry title must contain between 1 and 255 characters".into(),
        ));
    }
    if seed.version == 0 {
        return Err(Error::InvalidInput(
            "entry version must be at least 1".into(),
        ));
    }
    if let Some(unique_id) = seed.unique_id.as_deref() {
        validate_unique_id(unique_id)?;
    }
    Ok(())
}

fn validate_subentry(subentry: &ConfigSubentry) -> Result<()> {
    if subentry.subentry_id.trim().is_empty() {
        return Err(Error::InvalidInput(
            "stored subentry has an empty ID".into(),
        ));
    }
    Ok(())
}

fn ensure_subentry_unique_id(
    subentries: &[ConfigSubentry],
    unique_id: Option<&str>,
    except_subentry: Option<&str>,
) -> Result<()> {
    let Some(unique_id) = unique_id else {
        return Ok(());
    };
    if subentries.iter().any(|existing| {
        existing.unique_id.as_deref() == Some(unique_id)
            && Some(existing.subentry_id.as_str()) != except_subentry
    }) {
        return Err(Error::InvalidInput(SUBENTRY_ALREADY_CONFIGURED.into()));
    }
    Ok(())
}

fn supported_subentry_types_json(
    supported: BTreeMap<String, ConfigSubentryTypeInfo>,
) -> Map<String, Value> {
    supported
        .into_iter()
        .map(|(subentry_type, info)| {
            (
                subentry_type,
                json!({"supports_reconfigure": info.supports_reconfigure}),
            )
        })
        .collect()
}

fn record_discovery_key(entry: &mut ConfigEntry, key: DiscoveryKey) -> bool {
    let keys = entry.discovery_keys.entry(key.domain.clone()).or_default();
    if keys.contains(&key) {
        return false;
    }
    keys.push(key);
    let excess = keys.len().saturating_sub(MAX_DISCOVERY_KEYS_PER_DOMAIN);
    if excess > 0 {
        keys.drain(..excess);
    }
    true
}

fn discovery_key_from_context(context: &Map<String, Value>) -> Option<DiscoveryKey> {
    let key = serde_json::from_value::<DiscoveryKey>(context.get("discovery_key")?.clone()).ok()?;
    validate_domain(&key.domain).ok()?;
    Some(key)
}

fn validate_entry(entry: &ConfigEntry) -> Result<()> {
    validate_domain(&entry.domain)?;
    validate_source(&entry.source)?;
    if entry.entry_id.is_empty() || entry.title.trim().is_empty() || entry.version == 0 {
        return Err(Error::InvalidInput(
            "stored config entry has invalid identity fields".into(),
        ));
    }
    if let Some(unique_id) = entry.unique_id.as_deref() {
        validate_unique_id(unique_id)?;
    }
    for (domain, keys) in &entry.discovery_keys {
        validate_domain(domain)?;
        if keys.len() > MAX_DISCOVERY_KEYS_PER_DOMAIN {
            return Err(Error::InvalidInput(
                "stored config entry has too many discovery keys for one domain".into(),
            ));
        }
        let mut unique_keys = HashSet::new();
        for key in keys {
            validate_domain(&key.domain)?;
            if key.domain != *domain || !unique_keys.insert(key) {
                return Err(Error::InvalidInput(
                    "stored config entry has invalid or duplicate discovery keys".into(),
                ));
            }
        }
    }
    if let Some(disabled_by) = entry.disabled_by.as_deref()
        && disabled_by != "user"
    {
        return Err(Error::InvalidInput(
            "stored disabled_by must be user or null".into(),
        ));
    }
    let mut subentry_ids = std::collections::HashSet::new();
    for subentry in &entry.subentries {
        validate_subentry(subentry)?;
        if !subentry_ids.insert(subentry.subentry_id.as_str()) {
            return Err(Error::InvalidInput(
                "duplicate subentry ID in stored config entry".into(),
            ));
        }
    }
    let mut unique_ids = std::collections::HashSet::new();
    for subentry in &entry.subentries {
        if let Some(unique_id) = subentry.unique_id.as_deref()
            && !unique_ids.insert(unique_id)
        {
            return Err(Error::InvalidInput(
                "duplicate subentry unique ID in stored config entry".into(),
            ));
        }
    }
    Ok(())
}

fn validate_unique_ids<'a>(entries: impl IntoIterator<Item = &'a ConfigEntry>) -> Result<()> {
    let mut identities = HashMap::new();
    for entry in entries {
        if let Some(unique_id) = entry.unique_id.as_deref() {
            let key = (entry.domain.as_str(), unique_id);
            if identities.insert(key, entry.entry_id.as_str()).is_some() {
                return Err(Error::InvalidInput(format!(
                    "duplicate unique id for integration {}",
                    entry.domain
                )));
            }
        }
    }
    Ok(())
}

fn ensure_unique_id(
    entries: &BTreeMap<String, ConfigEntry>,
    domain: &str,
    unique_id: Option<&str>,
    except_entry: Option<&str>,
) -> Result<()> {
    let Some(unique_id) = unique_id else {
        return Ok(());
    };
    validate_unique_id(unique_id)?;
    if entries.values().any(|entry| {
        entry.domain == domain
            && entry.unique_id.as_deref() == Some(unique_id)
            && Some(entry.entry_id.as_str()) != except_entry
    }) {
        return Err(Error::InvalidInput(format!(
            "config entry unique id collision for {domain}"
        )));
    }
    Ok(())
}

fn validate_domain(domain: &str) -> Result<()> {
    if domain.is_empty()
        || domain.len() > 128
        || !domain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || !domain.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(Error::InvalidInput(format!(
            "invalid integration domain `{domain}`"
        )));
    }
    Ok(())
}

fn validate_source(source: &str) -> Result<()> {
    const SOURCES: &[&str] = &[
        "user",
        "import",
        "discovery",
        "integration_discovery",
        "dhcp",
        "ssdp",
        "usb",
        "zeroconf",
        "mqtt",
        "bluetooth",
        "hardware",
        "hassio",
        "homekit",
        "system",
        "ignore",
        "reconfigure",
        "reauth",
    ];
    if SOURCES.contains(&source) {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "unsupported config entry source `{source}`"
        )))
    }
}

fn validate_unique_id(unique_id: &str) -> Result<()> {
    if unique_id.trim().is_empty()
        || unique_id.chars().count() > 255
        || unique_id.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(
            "unique_id must contain 1 to 255 non-control characters".into(),
        ));
    }
    Ok(())
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidInput(format!("{key} must be a string")))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::InvalidInput(format!("{key} must be a string"))),
    }
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(Error::InvalidInput(format!("{key} must be a boolean"))),
    }
}

fn optional_url_value(field: &str, value: &Value) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(Error::InvalidInput(format!(
            "{field} must be a string or null"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    use hass_core::{
        Event, EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, Runtime,
        State, Store,
    };

    use super::*;

    struct FixtureDriver {
        setup_calls: AtomicUsize,
        unload_calls: AtomicUsize,
        remove_calls: AtomicUsize,
        on_create_calls: AtomicUsize,
        on_create_saw_loaded: AtomicBool,
        fail_first_setup: bool,
        fail_unload: bool,
        retry_second_setup: bool,
        launch_subentry_flow_after_create: bool,
        version: (u32, u32),
    }

    impl FixtureDriver {
        fn new() -> Self {
            Self {
                setup_calls: AtomicUsize::new(0),
                unload_calls: AtomicUsize::new(0),
                remove_calls: AtomicUsize::new(0),
                on_create_calls: AtomicUsize::new(0),
                on_create_saw_loaded: AtomicBool::new(false),
                fail_first_setup: false,
                fail_unload: false,
                retry_second_setup: false,
                launch_subentry_flow_after_create: false,
                version: (1, 1),
            }
        }
    }

    impl ConfigEntryDriver for FixtureDriver {
        fn domain(&self) -> &str {
            "fixture"
        }

        fn integration_type(&self) -> &str {
            "service"
        }

        fn supports_options(&self) -> bool {
            true
        }

        fn version(&self) -> (u32, u32) {
            self.version
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            let call = self.setup_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_setup && call == 0 {
                return Err(Error::Service("fixture failed first setup".into()));
            }
            if self.retry_second_setup && call == 1 {
                return Ok(SetupOutcome::RetryAfter {
                    seconds: 30,
                    reason: Some("fixture dependency not ready".into()),
                });
            }
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            self.unload_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_unload {
                Err(Error::Service("fixture unload failed".into()))
            } else {
                Ok(())
            }
        }

        fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            if request.user_input.is_none() {
                return Ok(ConfigFlowResult::Form {
                    step_id: "credentials".into(),
                    data_schema: Some(json!([{"name":"token","type":"string"}])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            let input = request.user_input.as_ref().expect("input was checked");
            let mut data = Map::new();
            data.insert(
                "token".into(),
                input
                    .get("token")
                    .cloned()
                    .unwrap_or_else(|| json!("fixture-secret")),
            );
            let mut result = ConfigFlowResult::create_entry(
                "Fixture account",
                data,
                Some("fixture-device-1".into()),
            );
            if let ConfigFlowResult::CreateEntry { options, .. } = &mut result {
                options.insert("polling".into(), json!(60));
            }
            Ok(result)
        }

        fn on_create_entry(
            &self,
            manager: &ConfigEntries,
            _request: &ConfigFlowRequest,
            entry: &ConfigEntry,
            _result: &ConfigFlowResult,
        ) -> Result<Option<ConfigFlowContinuation>> {
            self.on_create_calls.fetch_add(1, Ordering::SeqCst);
            self.on_create_saw_loaded.store(
                manager
                    .get_entry(&entry.entry_id)
                    .is_some_and(|entry| entry.state == ConfigEntryState::Loaded),
                Ordering::SeqCst,
            );
            if !self.launch_subentry_flow_after_create {
                return Ok(None);
            }
            let flow = manager.init_subentry_flow(
                &entry.entry_id,
                "fixture_child",
                "user",
                None,
                Map::new(),
                None,
            )?;
            let flow_id = flow
                .get("flow_id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidInput("subentry flow has no flow_id".into()))?;
            Ok(Some(ConfigFlowContinuation::config_subentries_flow(
                flow_id,
            )))
        }

        fn supported_subentry_types(
            &self,
            _entry: &ConfigEntry,
        ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
            BTreeMap::from([(
                "fixture_child".into(),
                ConfigSubentryTypeInfo {
                    supports_reconfigure: true,
                },
            )])
        }

        fn subentry_flow(
            &self,
            request: &ConfigSubentryFlowRequest,
        ) -> Result<ConfigSubentryFlowResult> {
            let Some(user_input) = request.user_input.as_ref() else {
                return Ok(ConfigSubentryFlowResult::Form {
                    step_id: if request.source == "reconfigure" {
                        "reconfigure".into()
                    } else {
                        "user".into()
                    },
                    data_schema: Some(json!([{"name":"token","type":"string"}])),
                    errors: None,
                    description_placeholders: None,
                });
            };
            if request.source == "reconfigure" {
                Ok(ConfigSubentryFlowResult::UpdateAndAbort {
                    update: ConfigSubentryUpdate {
                        title: Some("Fixture child updated".into()),
                        unique_id: Some(Some("fixture-child-updated".into())),
                        data: None,
                    },
                    data_updates: Some(user_input.clone()),
                    reason: None,
                    reload: false,
                })
            } else {
                Ok(ConfigSubentryFlowResult::CreateEntry {
                    title: "Fixture child".into(),
                    data: user_input.clone(),
                    unique_id: Some("fixture-child-1".into()),
                })
            }
        }

        fn options_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            if request.user_input.is_none() {
                return Ok(ConfigFlowResult::form(
                    "options",
                    Some(json!([{"name":"polling","type":"integer"}])),
                ));
            }
            let input = request.user_input.as_ref().expect("input was checked");
            Ok(ConfigFlowResult::CreateEntry {
                title: "Fixture options".into(),
                data: Map::new(),
                options: input.clone(),
                unique_id: request.unique_id.clone(),
                version: self.version.0,
                minor_version: self.version.1,
            })
        }

        fn migrate(
            &self,
            entry: &ConfigEntry,
            _target_version: u32,
            _target_minor_version: u32,
        ) -> Result<Map<String, Value>> {
            let mut data = entry.data.clone();
            data.insert("migrated".into(), json!(true));
            Ok(data)
        }

        fn remove(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct DiscoveryFlowDriver;

    impl ConfigEntryDriver for DiscoveryFlowDriver {
        fn domain(&self) -> &str {
            "discovery_fixture"
        }

        fn supports_options(&self) -> bool {
            true
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            Ok(())
        }

        fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            let complete = request
                .user_input
                .as_ref()
                .and_then(|input| input.get("complete"))
                .and_then(Value::as_bool)
                == Some(true);
            if !complete {
                return Ok(ConfigFlowResult::form("discovery", None));
            }
            let mut data = Map::new();
            data.insert("token".into(), json!("discovery-flow-token"));
            Ok(ConfigFlowResult::create_entry(
                "Discovered fixture",
                data,
                Some("flow-created-device".into()),
            ))
        }

        fn options_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form("options", None))
        }

        fn supported_subentry_types(
            &self,
            _entry: &ConfigEntry,
        ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
            BTreeMap::from([(
                "fixture_child".into(),
                ConfigSubentryTypeInfo {
                    supports_reconfigure: false,
                },
            )])
        }

        fn subentry_flow(
            &self,
            _request: &ConfigSubentryFlowRequest,
        ) -> Result<ConfigSubentryFlowResult> {
            Ok(ConfigSubentryFlowResult::Form {
                step_id: "user".into(),
                data_schema: None,
                errors: None,
                description_placeholders: None,
            })
        }
    }

    fn discovery_key(key: &str) -> DiscoveryKey {
        DiscoveryKey {
            domain: "ssdp".into(),
            key: DiscoveryKeyValue::String(key.into()),
            version: 1,
        }
    }

    fn discovery_info(st: &str, location: &str) -> Map<String, Value> {
        Map::from_iter([
            ("ssdp_st".into(), json!(st)),
            ("ssdp_location".into(), json!(location)),
        ])
    }

    fn discovery_context(key: &DiscoveryKey, unique_id: Option<&str>) -> Map<String, Value> {
        let mut context = Map::new();
        context.insert("discovery_key".into(), serde_json::to_value(key).unwrap());
        if let Some(unique_id) = unique_id {
            context.insert("unique_id".into(), json!(unique_id));
        }
        context
    }

    struct CleanupSeenObserver(Arc<AtomicBool>);

    impl ConfigLifecycleObserver for CleanupSeenObserver {
        fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()> {
            if matches!(event, ConfigLifecycleEvent::EntryRemoved { .. }) {
                self.0.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    struct DiscoveryRemovalProbe {
        manager: Mutex<Option<ConfigEntries>>,
        cleanup_seen: Arc<AtomicBool>,
        cleanup_preceded: AtomicBool,
        entry_was_gone: AtomicBool,
        events: Mutex<Vec<DiscoveryKeyRemoval>>,
    }

    impl DiscoveryKeyRemovalObserver for DiscoveryRemovalProbe {
        fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> Result<()> {
            self.cleanup_preceded
                .store(self.cleanup_seen.load(Ordering::SeqCst), Ordering::SeqCst);
            let manager = self
                .manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let entry_was_gone = manager.is_some_and(|manager| {
                let (sender, receiver) = std::sync::mpsc::channel();
                let entry_id = event.entry_id.clone();
                std::thread::spawn(move || {
                    let _ = sender.send(manager.get_entry(&entry_id).is_none());
                });
                receiver.recv_timeout(StdDuration::from_secs(1)) == Ok(true)
            });
            self.entry_was_gone.store(entry_was_gone, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
            Ok(())
        }
    }

    struct PanickingDiscoveryRemovalObserver;

    impl DiscoveryKeyRemovalObserver for PanickingDiscoveryRemovalObserver {
        fn discovery_keys_removed(&self, _event: &DiscoveryKeyRemoval) -> Result<()> {
            panic!("injected discovery-key-removal observer panic");
        }
    }

    #[derive(Default)]
    struct CollectDiscoveryRemovalObserver(Mutex<Vec<DiscoveryKeyRemoval>>);

    impl DiscoveryKeyRemovalObserver for CollectDiscoveryRemovalObserver {
        fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> Result<()> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
            Ok(())
        }
    }

    fn runtime(store: Arc<MemoryStore>) -> Runtime {
        Runtime::new(store).expect("runtime initializes")
    }

    fn register(manager: &ConfigEntries, driver: Arc<FixtureDriver>) {
        manager
            .register_driver(driver)
            .expect("fixture driver registers");
    }

    fn seed() -> NewConfigEntry {
        let mut data = Map::new();
        data.insert("token".into(), json!("secret-value"));
        let mut seed = NewConfigEntry::new("fixture", "Fixture account", data);
        seed.unique_id = Some("fixture-device-1".into());
        seed
    }

    #[derive(Default)]
    struct LifecycleObserver {
        events: std::sync::Mutex<Vec<ConfigLifecycleEvent>>,
        manager: std::sync::Mutex<Option<ConfigEntries>>,
        runtime_events: std::sync::Mutex<Option<tokio::sync::broadcast::Receiver<Event>>>,
        check_subentry_cleanup_order: AtomicBool,
        check_disable_notification_order: AtomicBool,
        check_parent_remove_notification_order: AtomicBool,
    }

    impl ConfigLifecycleObserver for LifecycleObserver {
        fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()> {
            if let Some(manager) = self
                .manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                match event {
                    ConfigLifecycleEvent::EntryRemoved { entry_id, .. } => {
                        let (sender, receiver) = std::sync::mpsc::channel();
                        let entry_id = entry_id.clone();
                        std::thread::spawn(move || {
                            let result = manager.unload_entry(&entry_id).is_err();
                            let _ = sender.send(result);
                        });
                        if receiver.recv_timeout(StdDuration::from_secs(1)) != Ok(true) {
                            return Err(Error::Service(
                                "entry-removal observer could not reenter the manager".into(),
                            ));
                        }
                        if self
                            .check_parent_remove_notification_order
                            .load(Ordering::SeqCst)
                        {
                            let mut receiver = self.runtime_events.lock().unwrap();
                            let receiver = receiver.as_mut().unwrap();
                            while let Ok(event) = receiver.try_recv() {
                                if event.event_type == "config_entry_changed"
                                    && event.data.get("type").and_then(Value::as_str)
                                        == Some("removed")
                                {
                                    return Err(Error::Service(
                                        "parent removal event preceded cleanup observer".into(),
                                    ));
                                }
                            }
                        }
                    }
                    ConfigLifecycleEvent::EntryDisabledByChanged {
                        entry_id,
                        disabled_by,
                    } => {
                        if disabled_by.is_some() {
                            let (sender, receiver) = std::sync::mpsc::channel();
                            let entry_id = entry_id.clone();
                            std::thread::spawn(move || {
                                let result = manager.unload_entry(&entry_id).is_ok();
                                let _ = sender.send(result);
                            });
                            if receiver.recv_timeout(StdDuration::from_secs(1)) != Ok(true) {
                                return Err(Error::Service(
                                    "disable observer could not reenter the manager".into(),
                                ));
                            }
                        }
                        if self.check_disable_notification_order.load(Ordering::SeqCst) {
                            self.expect_entry_change("updated")?;
                        }
                    }
                    ConfigLifecycleEvent::SubentryRemoved {
                        entry_id,
                        subentry_id,
                    } if manager.subentry_exists(entry_id, subentry_id) => {
                        return Err(Error::Service(
                            "subentry observer ran before persisted removal".into(),
                        ));
                    }
                    ConfigLifecycleEvent::SubentryRemoved { .. }
                        if self.check_subentry_cleanup_order.load(Ordering::SeqCst) =>
                    {
                        if let Some(receiver) = self
                            .runtime_events
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_mut()
                            && !matches!(
                                receiver.try_recv(),
                                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                            )
                        {
                            return Err(Error::Service(
                                "subentry removal event preceded cleanup observer".into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
            Ok(())
        }
    }

    impl LifecycleObserver {
        fn expect_entry_change(&self, expected_change: &str) -> Result<()> {
            let mut receiver = self
                .runtime_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let receiver = receiver
                .as_mut()
                .ok_or_else(|| Error::Service("runtime event receiver is missing".into()))?;
            loop {
                match receiver.try_recv() {
                    Ok(event)
                        if event.event_type == "config_entry_changed"
                            && event.data.get("type").and_then(Value::as_str)
                                == Some(expected_change) =>
                    {
                        return Ok(());
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        return Err(Error::Service(format!(
                            "expected config entry {expected_change} event before lifecycle callback"
                        )));
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        return Err(Error::Service("runtime event receiver closed".into()));
                    }
                }
            }
        }
    }

    #[derive(Default)]
    struct CleanupObserver {
        events: std::sync::Mutex<Vec<ConfigLifecycleEvent>>,
        manager: std::sync::Mutex<Option<ConfigEntries>>,
        observer: std::sync::Mutex<Option<std::sync::Weak<dyn ConfigLifecycleObserver>>>,
        fail_once: AtomicBool,
        replay_in_callback: AtomicBool,
        nested_replay_counts: std::sync::Mutex<Vec<usize>>,
        remove_other_entry: std::sync::Mutex<Option<String>>,
        unregister_self: AtomicBool,
        fail_store_after_success: std::sync::Mutex<Option<Arc<FailingMetadataStore>>>,
    }

    impl ConfigLifecycleObserver for CleanupObserver {
        fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());

            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(Error::Service("injected cleanup failure".into()));
            }

            let manager = self
                .manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(manager) = manager {
                if self.replay_in_callback.swap(false, Ordering::SeqCst) {
                    let count = manager.replay_pending_cleanup()?;
                    self.nested_replay_counts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(count);
                }

                let other_entry = self
                    .remove_other_entry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(entry_id) = other_entry {
                    manager.remove_entry(&entry_id)?;
                }

                if self.unregister_self.swap(false, Ordering::SeqCst)
                    && let Some(observer) = self
                        .observer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade)
                {
                    manager.unregister_lifecycle_observer(&observer);
                }
            }

            if let Some(store) = self
                .fail_store_after_success
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                store.fail_metadata.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    fn register_cleanup_observer(
        manager: &ConfigEntries,
        observer: &Arc<CleanupObserver>,
    ) -> Arc<dyn ConfigLifecycleObserver> {
        let observer_trait: Arc<dyn ConfigLifecycleObserver> = observer.clone();
        manager
            .register_lifecycle_observer(observer_trait.clone())
            .expect("cleanup observer registers");
        *observer
            .manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(manager.clone());
        *observer
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::downgrade(&observer_trait));
        observer_trait
    }

    struct BlockingCleanupObserver {
        events: std::sync::Mutex<Vec<ConfigLifecycleEvent>>,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl ConfigLifecycleObserver for BlockingCleanupObserver {
        fn config_lifecycle_changed(&self, event: &ConfigLifecycleEvent) -> Result<()> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
            self.entered
                .send(())
                .map_err(|error| Error::Service(error.to_string()))?;
            self.release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv()
                .map_err(|error| Error::Service(error.to_string()))?;
            Ok(())
        }
    }

    fn store_has_pending_cleanup(manager: &ConfigEntries) -> bool {
        !manager.lock_state().pending_cleanup.is_empty()
    }

    #[derive(Default)]
    struct FailingMetadataStore {
        inner: MemoryStore,
        fail_metadata: AtomicBool,
        fail_events: AtomicBool,
    }

    impl Store for FailingMetadataStore {
        fn load_states(&self) -> Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> Result<()> {
            if self.fail_events.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected event failure".into()));
            }
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            if self.fail_metadata.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected metadata failure".into()));
            }
            self.inner.set_data(key, value)
        }

        fn delete_data(&self, key: &str) -> Result<bool> {
            self.inner.delete_data(key)
        }

        fn history(
            &self,
            entity_id: &str,
            start: DateTime<Utc>,
            end: DateTime<Utc>,
        ) -> Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }

        fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
            self.inner.history_query(query)
        }

        fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
            self.inner.event_history_query(query)
        }

        fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn repack(&self) -> Result<()> {
            self.inner.repack()
        }
    }

    #[test]
    fn driver_cleanup_matches_instance_and_preserves_replacements() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let first: Arc<dyn ConfigEntryDriver> = Arc::new(FixtureDriver::new());
        let replacement: Arc<dyn ConfigEntryDriver> = Arc::new(FixtureDriver::new());
        manager.register_driver(first.clone()).unwrap();
        assert!(!manager.unregister_driver(&replacement));
        assert_eq!(manager.flow_handlers(), ["fixture"]);
        assert!(manager.unregister_driver(&first));
        assert!(manager.driver("fixture").is_none());
        manager.register_driver(replacement.clone()).unwrap();
        assert!(!manager.unregister_driver(&first));
        assert!(Arc::ptr_eq(
            &manager.driver("fixture").unwrap(),
            &replacement
        ));
        assert!(manager.unregister_driver(&replacement));
        assert!(!manager.unregister_driver(&replacement));
    }

    #[test]
    fn config_and_options_flows_execute_driver_and_redact_secrets() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let manager = ConfigEntries::new(runtime).unwrap();
        let driver = Arc::new(FixtureDriver::new());
        register(&manager, driver.clone());

        let started = manager
            .init_config_flow("fixture", "user", Map::new())
            .unwrap();
        assert_eq!(started["type"], "form");
        assert_eq!(started["step_id"], "credentials");
        assert_eq!(started["context"]["source"], "user");
        let flow_id = started["flow_id"].as_str().unwrap();
        let active = manager
            .active_flow(ConfigFlowType::Config, flow_id)
            .unwrap();
        assert_eq!(active.flow_type, ConfigFlowType::Config);
        assert_eq!(active.domain, "fixture");
        assert_eq!(active.result, started);
        assert!(
            manager
                .active_flow(ConfigFlowType::Options, flow_id)
                .is_err()
        );
        let mut input = Map::new();
        input.insert("token".into(), json!("top-secret-flow-token"));
        let finished = manager.submit_flow(flow_id, input).unwrap();
        assert_eq!(finished["type"], "create_entry");
        assert_eq!(finished["minor_version"], 1);
        assert_eq!(finished["options"]["polling"], 60);
        assert!(finished["description"].is_null());
        assert_eq!(finished["subentries"], json!([]));
        assert!(!finished.to_string().contains("top-secret-flow-token"));
        let entry_id = finished["result"]["entry_id"].as_str().unwrap().to_owned();
        let entry = manager.get_entry(&entry_id).unwrap();
        assert_eq!(entry.state, ConfigEntryState::Loaded);
        assert_eq!(entry.data["token"], "top-secret-flow-token");
        assert!(
            manager.list_entries(None)[0]
                .public_json()
                .get("data")
                .is_none()
        );
        let listed = manager
            .handle_ws(&json!({
                "type": "config_entries/get",
                "type_filter": "service"
            }))
            .unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert!(listed[0].get("data").is_none());
        assert_eq!(listed[0]["state"], "loaded");
        assert_eq!(listed[0]["supports_options"], true);
        assert!(listed[0].get("error_reason_translation_key").is_some());

        let options_start = manager.init_options_flow(&entry_id).unwrap();
        assert_eq!(options_start["type"], "form");
        assert_eq!(options_start["handler"], entry_id);
        assert_eq!(options_start["context"]["entry_id"], entry_id);
        let options_flow_id = options_start["flow_id"].as_str().unwrap();
        let active_options = manager
            .active_flow(ConfigFlowType::Options, options_flow_id)
            .unwrap();
        assert_eq!(active_options.result, options_start);
        let mut options = Map::new();
        options.insert("polling".into(), json!(15));
        let options_result = manager.submit_flow(options_flow_id, options).unwrap();
        assert_eq!(options_result["type"], "create_entry");
        assert!(
            manager
                .active_flow(ConfigFlowType::Options, options_flow_id)
                .is_err()
        );
        assert_eq!(manager.get_entry(&entry_id).unwrap().options["polling"], 15);
        assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 1);
        assert_eq!(driver.setup_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn setup_error_retry_and_retry_state_are_persisted() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let manager = ConfigEntries::new(runtime).unwrap();
        let driver = Arc::new(FixtureDriver {
            fail_first_setup: true,
            retry_second_setup: true,
            ..FixtureDriver::new()
        });
        register(&manager, driver.clone());
        let entry = manager.create_entry(seed()).unwrap();

        assert!(manager.setup_entry(&entry.entry_id).is_err());
        let failed = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(failed.state, ConfigEntryState::SetupError);
        assert!(failed.reason.as_deref().unwrap().contains("fixture failed"));

        let retry = manager.retry_entry(&entry.entry_id).unwrap();
        assert_eq!(retry.state, ConfigEntryState::SetupRetry);
        assert_eq!(retry.retry_attempts, 1);
        assert!(retry.retry_at.is_some());

        assert!(manager.retry_due_entries(Utc::now()).is_empty());
        let due = manager.retry_due_entries(Utc::now() + Duration::seconds(31));
        assert_eq!(due.len(), 1);
        let loaded = due.into_iter().next().unwrap().unwrap();
        assert_eq!(loaded.state, ConfigEntryState::Loaded);
        assert_eq!(loaded.retry_attempts, 0);
        assert_eq!(driver.setup_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn missing_driver_fails_setup_and_never_claims_success() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let entry = manager.create_entry(seed()).unwrap();
        let error = manager.setup_entry(&entry.entry_id).unwrap_err();
        assert!(error.to_string().contains("no native integration driver"));
        let stored = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(stored.state, ConfigEntryState::SetupError);
        assert_ne!(stored.state, ConfigEntryState::Loaded);
    }

    #[test]
    fn lifecycle_calls_real_unload_before_remove() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver::new());
        register(&manager, driver.clone());
        let entry = manager.create_entry(seed()).unwrap();
        manager.setup_entry(&entry.entry_id).unwrap();
        manager.unload_entry(&entry.entry_id).unwrap();
        assert_eq!(
            manager.get_entry(&entry.entry_id).unwrap().state,
            ConfigEntryState::NotLoaded
        );
        manager.setup_entry(&entry.entry_id).unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();
        assert!(manager.get_entry(&entry.entry_id).is_none());
        assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 2);
        assert_eq!(driver.remove_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reauth_flow_updates_existing_entry_and_clears_reauth_state() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver::new());
        register(&manager, driver.clone());
        let entry = manager.create_entry(seed()).unwrap();
        manager.setup_entry(&entry.entry_id).unwrap();

        let started = manager.begin_reauth_flow(&entry.entry_id).unwrap();
        assert!(manager.get_entry(&entry.entry_id).unwrap().needs_reauth);
        let mut input = Map::new();
        input.insert("token".into(), json!("renewed-token"));
        let finished = manager
            .submit_flow(started["flow_id"].as_str().unwrap(), input)
            .unwrap();
        assert_eq!(finished["type"], "create_entry");
        let updated = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(updated.data["token"], "renewed-token");
        assert_eq!(updated.unique_id.as_deref(), Some("fixture-device-1"));
        assert!(!updated.needs_reauth);
        assert_eq!(updated.state, ConfigEntryState::Loaded);
        assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_unload_survives_disable_and_requests_restart() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver {
            fail_unload: true,
            ..FixtureDriver::new()
        });
        register(&manager, driver);
        let entry = manager.create_entry(seed()).unwrap();
        manager.setup_entry(&entry.entry_id).unwrap();
        assert!(
            !manager
                .disable_entry(&entry.entry_id, Some("user"))
                .unwrap()
        );
        let failed = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(failed.state, ConfigEntryState::FailedUnload);
        assert_eq!(failed.disabled_by.as_deref(), Some("user"));
        assert!(failed.require_reload);
        assert!(!manager.disable_entry(&entry.entry_id, None).unwrap());
        assert_eq!(
            manager.get_entry(&entry.entry_id).unwrap().disabled_by,
            None
        );
    }

    #[test]
    fn version_upgrade_uses_registered_migration_and_rejects_unique_collision() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver {
            version: (2, 1),
            ..FixtureDriver::new()
        });
        register(&manager, driver);
        let entry = manager.create_entry(seed()).unwrap();
        let loaded = manager.setup_entry(&entry.entry_id).unwrap();
        assert_eq!((loaded.version, loaded.minor_version), (2, 1));
        assert_eq!(loaded.data["migrated"], true);
        assert!(manager.create_entry(seed()).is_err());
    }

    #[test]
    fn entries_options_disabled_state_and_core_config_survive_restart() {
        let store = Arc::new(MemoryStore::default());
        let entry_id;
        {
            let runtime = runtime(store.clone());
            let manager = ConfigEntries::new(runtime).unwrap();
            register(&manager, Arc::new(FixtureDriver::new()));
            manager
                .initialize_core_config(&json!({
                    "name":"Cabin", "latitude":42.0, "longitude":-71.0,
                    "time_zone":"America/New_York", "unit_system":"imperial"
                }))
                .unwrap();
            assert_eq!(manager.runtime.time_zone(), "America/New_York");
            let entry = manager.create_entry(seed()).unwrap();
            entry_id = entry.entry_id;
            manager.setup_entry(&entry_id).unwrap();
            let mut options = Map::new();
            options.insert("polling".into(), json!(42));
            manager.update_options(&entry_id, options).unwrap();
            manager.disable_entry(&entry_id, Some("user")).unwrap();
            let mut changed = manager.core_config();
            changed.location_name = "Renamed in UI".into();
            changed.latitude = 43.0;
            changed.longitude = -72.0;
            changed.elevation = -20.0;
            manager.update_core_config(changed).unwrap();
            assert_eq!(
                manager.runtime.home_context(),
                HomeContext {
                    latitude: 43.0,
                    longitude: -72.0,
                    elevation: -20.0,
                    time_zone: "America/New_York".into()
                }
            );
        }
        {
            let runtime = runtime(store);
            let manager = ConfigEntries::new(runtime).unwrap();
            assert_eq!(manager.runtime.time_zone(), "America/New_York");
            let restored = manager.get_entry(&entry_id).unwrap();
            assert_eq!(
                manager.runtime.home_context(),
                HomeContext {
                    latitude: 43.0,
                    longitude: -72.0,
                    elevation: -20.0,
                    time_zone: "America/New_York".into()
                }
            );
            assert_eq!(restored.state, ConfigEntryState::NotLoaded);
            assert_eq!(restored.disabled_by.as_deref(), Some("user"));
            assert_eq!(restored.options["polling"], 42);
            assert_eq!(restored.data["token"], "secret-value");
            assert_eq!(manager.core_config().location_name, "Renamed in UI");
            manager
                .initialize_core_config(&json!({"name":"older YAML value"}))
                .unwrap();
            assert_eq!(manager.core_config().location_name, "Renamed in UI");
        }
    }

    #[test]
    fn core_config_websocket_updates_validate_values_and_persist() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let mut events = manager.runtime.subscribe();
        let updated = manager
            .handle_ws(&json!({
                "type":"config/core/update",
                "location_name":"Apartment",
                "unit_system":"imperial",
                "external_url":"https://example.test"
            }))
            .unwrap();
        assert_eq!(updated["location_name"], "Apartment");
        assert_eq!(updated["unit_system"]["name"], "imperial");
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, "core_config_updated");
        assert_eq!(
            event.data,
            json!({
                "location_name":"Apartment",
                "unit_system":"imperial",
                "external_url":"https://example.test"
            })
            .as_object()
            .unwrap()
            .clone()
        );
        assert!(
            manager
                .handle_ws(&json!({"type":"config/core/update", "latitude":100.0}))
                .is_err()
        );
        assert_eq!(manager.core_config().location_name, "Apartment");
        assert!(
            manager
                .handle_ws(&json!({"type":"config/core/update", "unexpected":true}))
                .is_err()
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn subentry_crud_preserves_identity_uniqueness_and_secret_redaction() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver::new());
        register(&manager, driver);
        let entry = manager.create_entry(seed()).unwrap();
        assert_eq!(entry.public_json()["num_subentries"], 0);
        assert_eq!(
            entry.public_json()["supported_subentry_types"]["fixture_child"]["supports_reconfigure"],
            true
        );

        let mut data = Map::new();
        data.insert("token".into(), json!("private-child-secret"));
        let mut child_seed = NewConfigSubentry::new("fixture_child", "Fixture child", data);
        child_seed.unique_id = Some("child-unique-1".into());
        let child = manager
            .create_subentry(&entry.entry_id, child_seed.clone())
            .unwrap();
        assert_ne!(child.subentry_id, entry.entry_id);
        assert!(Uuid::parse_str(&child.subentry_id).is_ok());
        assert_eq!(
            manager.get_subentry(&entry.entry_id, &child.subentry_id),
            Some(child.clone())
        );
        let listed = manager.list_subentries(&entry.entry_id).unwrap();
        assert_eq!(listed.as_slice(), std::slice::from_ref(&child));
        assert!(manager.subentry_exists(&entry.entry_id, &child.subentry_id));
        assert_eq!(
            manager.get_entry(&entry.entry_id).unwrap().public_json()["num_subentries"],
            1
        );
        assert!(child.public_json().get("data").is_none());
        assert!(!format!("{child:?}").contains("private-child-secret"));
        assert!(
            !child
                .public_json()
                .to_string()
                .contains("private-child-secret")
        );

        assert!(
            manager
                .create_subentry(&entry.entry_id, child_seed)
                .is_err()
        );
        assert_eq!(manager.list_subentries(&entry.entry_id).unwrap().len(), 1);

        let mut replacement_data = Map::new();
        replacement_data.insert("region".into(), json!("north"));
        assert!(
            manager
                .update_subentry(
                    &entry.entry_id,
                    &child.subentry_id,
                    ConfigSubentryUpdate {
                        title: Some("Renamed child".into()),
                        unique_id: Some(Some("child-unique-2".into())),
                        data: Some(replacement_data.clone()),
                    },
                )
                .unwrap()
        );
        assert!(
            !manager
                .update_subentry(
                    &entry.entry_id,
                    &child.subentry_id,
                    ConfigSubentryUpdate {
                        title: Some("Renamed child".into()),
                        unique_id: Some(Some("child-unique-2".into())),
                        data: Some(replacement_data.clone()),
                    },
                )
                .unwrap()
        );

        let mut sibling_seed = NewConfigSubentry::new("fixture_child", "Sibling", Map::new());
        sibling_seed.unique_id = Some("sibling-unique".into());
        let sibling = manager
            .create_subentry(&entry.entry_id, sibling_seed)
            .unwrap();
        assert!(
            manager
                .update_subentry(
                    &entry.entry_id,
                    &child.subentry_id,
                    ConfigSubentryUpdate {
                        unique_id: Some(Some("sibling-unique".into())),
                        ..ConfigSubentryUpdate::default()
                    },
                )
                .is_err()
        );
        assert_eq!(
            manager
                .get_subentry(&entry.entry_id, &child.subentry_id)
                .unwrap()
                .unique_id
                .as_deref(),
            Some("child-unique-2")
        );
        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .unwrap()
        );
        assert!(!manager.subentry_exists(&entry.entry_id, &child.subentry_id));
        assert!(
            manager
                .get_subentry(&entry.entry_id, &sibling.subentry_id)
                .is_some()
        );
        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .is_err()
        );
    }

    #[test]
    fn registered_native_subentry_flow_creates_reconfigures_and_rejects_duplicates() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();

        let started = manager
            .init_subentry_flow(
                &entry.entry_id,
                "fixture_child",
                "user",
                None,
                Map::new(),
                None,
            )
            .unwrap();
        assert_eq!(started["type"], "form");
        assert_eq!(started["handler"], json!([entry.entry_id, "fixture_child"]));
        assert!(
            manager
                .flow_result(started["flow_id"].as_str().unwrap())
                .is_ok()
        );
        let active = manager
            .active_flow(
                ConfigFlowType::ConfigSubentries,
                started["flow_id"].as_str().unwrap(),
            )
            .unwrap();
        assert_eq!(active.result, started);
        assert!(
            manager
                .active_flow(ConfigFlowType::Config, &active.flow_id)
                .is_err()
        );
        let mut input = Map::new();
        input.insert("token".into(), json!("private-flow-secret"));
        let created = manager
            .submit_flow(started["flow_id"].as_str().unwrap(), input)
            .unwrap();
        assert_eq!(created["type"], "create_entry");
        assert_eq!(created["result"]["subentry_type"], "fixture_child");
        assert!(created["result"].get("data").is_none());
        assert!(!created.to_string().contains("private-flow-secret"));
        let subentry_id = created["result"]["subentry_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            manager
                .get_subentry(&entry.entry_id, &subentry_id)
                .unwrap()
                .data["token"],
            "private-flow-secret"
        );

        let duplicate = manager
            .init_subentry_flow(
                &entry.entry_id,
                "fixture_child",
                "user",
                None,
                Map::new(),
                None,
            )
            .unwrap();
        assert_eq!(
            manager
                .submit_flow(
                    duplicate["flow_id"].as_str().unwrap(),
                    Map::from_iter([("token".into(), json!("duplicate"))]),
                )
                .unwrap()["reason"],
            "already_configured"
        );
        assert_eq!(manager.list_subentries(&entry.entry_id).unwrap().len(), 1);

        let reconfigure = manager
            .init_subentry_flow(
                &entry.entry_id,
                "fixture_child",
                "reconfigure",
                Some(&subentry_id),
                Map::new(),
                None,
            )
            .unwrap();
        let mut updates = Map::new();
        updates.insert("region".into(), json!("west"));
        let finished = manager
            .submit_flow(reconfigure["flow_id"].as_str().unwrap(), updates)
            .unwrap();
        assert_eq!(finished["type"], "abort");
        assert_eq!(finished["reason"], "reconfigure_successful");
        let updated = manager.get_subentry(&entry.entry_id, &subentry_id).unwrap();
        assert_eq!(updated.title, "Fixture child updated");
        assert_eq!(updated.unique_id.as_deref(), Some("fixture-child-updated"));
        assert_eq!(updated.data["region"], "west");
        assert_eq!(updated.data["token"], "private-flow-secret");

        assert!(
            manager
                .init_subentry_flow(
                    &entry.entry_id,
                    "unsupported",
                    "user",
                    None,
                    Map::new(),
                    None,
                )
                .is_err()
        );
    }

    #[test]
    fn parent_create_callback_can_launch_a_registered_subentry_flow() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let driver = Arc::new(FixtureDriver {
            launch_subentry_flow_after_create: true,
            ..FixtureDriver::new()
        });
        register(&manager, driver.clone());

        let started = manager
            .init_config_flow("fixture", "user", Map::new())
            .unwrap();
        let mut input = Map::new();
        input.insert("token".into(), json!("parent-token"));
        let parent_result = manager
            .submit_flow(started["flow_id"].as_str().unwrap(), input)
            .unwrap();
        assert_eq!(parent_result["type"], "create_entry");
        assert_eq!(
            parent_result["next_flow"][0],
            ConfigFlowType::ConfigSubentries.as_str()
        );
        assert!(driver.on_create_saw_loaded.load(Ordering::SeqCst));
        assert_eq!(driver.on_create_calls.load(Ordering::SeqCst), 1);

        let child_flow_id = parent_result["next_flow"][1].as_str().unwrap();
        let child_form = manager.flow_result(child_flow_id).unwrap();
        assert_eq!(child_form["type"], "form");
        assert_eq!(
            child_form["handler"],
            json!([parent_result["result"]["entry_id"], "fixture_child"])
        );
        let mut child_input = Map::new();
        child_input.insert("token".into(), json!("child-token"));
        let child_result = manager.submit_flow(child_flow_id, child_input).unwrap();
        assert_eq!(child_result["type"], "create_entry");
        assert_eq!(
            manager
                .get_entry(parent_result["result"]["entry_id"].as_str().unwrap())
                .unwrap()
                .public_json()["num_subentries"],
            1
        );
    }

    #[test]
    fn lifecycle_observers_receive_committed_events_outside_manager_locks() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();
        let child = manager
            .create_subentry(
                &entry.entry_id,
                NewConfigSubentry::new("fixture_child", "Child", Map::new()),
            )
            .unwrap();

        let observer = Arc::new(LifecycleObserver::default());
        let observer_trait: Arc<dyn ConfigLifecycleObserver> = observer.clone();
        let replacement: Arc<dyn ConfigLifecycleObserver> = Arc::new(LifecycleObserver::default());
        manager
            .register_lifecycle_observer(observer_trait.clone())
            .unwrap();
        observer.manager.lock().unwrap().replace(manager.clone());
        assert!(
            manager
                .register_lifecycle_observer(observer_trait.clone())
                .is_err()
        );
        assert!(!manager.unregister_lifecycle_observer(&replacement));

        let mut runtime_events = manager.runtime.subscribe();
        while runtime_events.try_recv().is_ok() {}
        observer
            .runtime_events
            .lock()
            .unwrap()
            .replace(runtime_events);
        observer
            .check_subentry_cleanup_order
            .store(true, Ordering::SeqCst);
        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .unwrap()
        );
        observer
            .check_subentry_cleanup_order
            .store(false, Ordering::SeqCst);
        assert_eq!(
            observer
                .runtime_events
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_recv()
                .unwrap()
                .event_type,
            "config_entry_changed"
        );

        observer
            .check_disable_notification_order
            .store(true, Ordering::SeqCst);
        assert!(
            manager
                .disable_entry(&entry.entry_id, Some("user"))
                .unwrap()
        );
        assert!(manager.disable_entry(&entry.entry_id, None).unwrap());
        observer
            .check_disable_notification_order
            .store(false, Ordering::SeqCst);
        while observer
            .runtime_events
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_recv()
            .is_ok()
        {}
        observer
            .check_parent_remove_notification_order
            .store(true, Ordering::SeqCst);
        manager.remove_entry(&entry.entry_id).unwrap();
        observer.expect_entry_change("removed").unwrap();
        observer
            .check_parent_remove_notification_order
            .store(false, Ordering::SeqCst);
        assert_eq!(
            *observer.events.lock().unwrap(),
            vec![
                ConfigLifecycleEvent::SubentryRemoved {
                    entry_id: entry.entry_id.clone(),
                    subentry_id: child.subentry_id.clone(),
                },
                ConfigLifecycleEvent::EntryDisabledByChanged {
                    entry_id: entry.entry_id.clone(),
                    disabled_by: Some("user".into()),
                },
                ConfigLifecycleEvent::EntryRegistryDisabledByChanged {
                    entry_id: entry.entry_id.clone(),
                },
                ConfigLifecycleEvent::EntryDisabledByChanged {
                    entry_id: entry.entry_id.clone(),
                    disabled_by: None,
                },
                ConfigLifecycleEvent::EntryRegistryDisabledByChanged {
                    entry_id: entry.entry_id.clone(),
                },
                ConfigLifecycleEvent::EntryRemoved {
                    entry_id: entry.entry_id.clone(),
                    domain: entry.domain.clone(),
                },
            ]
        );
        assert!(manager.unregister_lifecycle_observer(&observer_trait));
        assert!(!manager.unregister_lifecycle_observer(&observer_trait));
    }

    #[test]
    fn observer_guard_unregisters_only_its_owned_instance() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let entry = manager.create_entry(seed()).unwrap();
        let guarded = Arc::new(LifecycleObserver::default());
        let guarded_trait: Arc<dyn ConfigLifecycleObserver> = guarded.clone();
        let guard = manager.observe_lifecycle(guarded_trait.clone()).unwrap();
        let retained = Arc::new(LifecycleObserver::default());
        let retained_trait: Arc<dyn ConfigLifecycleObserver> = retained.clone();
        manager
            .register_lifecycle_observer(retained_trait.clone())
            .unwrap();

        drop(guard);
        manager
            .disable_entry(&entry.entry_id, Some("user"))
            .unwrap();
        assert!(guarded.events.lock().unwrap().is_empty());
        assert_eq!(retained.events.lock().unwrap().len(), 2);
        assert!(manager.unregister_lifecycle_observer(&retained_trait));
    }

    #[test]
    fn failed_parent_cleanup_survives_reopen_and_replays_with_domain() {
        let store = Arc::new(FailingMetadataStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let manager = ConfigEntries::new(runtime.clone()).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let mut data = Map::new();
        data.insert("credential".into(), json!("parent-private-value"));
        let entry = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", data))
            .unwrap();

        let failed = Arc::new(CleanupObserver::default());
        failed.fail_once.store(true, Ordering::SeqCst);
        let _failed_trait = register_cleanup_observer(&manager, &failed);
        assert!(manager.remove_entry(&entry.entry_id).is_err());
        assert!(manager.get_entry(&entry.entry_id).is_none());

        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        let pending = persisted["pending_cleanup"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["event"]["type"], "entry_removed");
        assert_eq!(pending[0]["event"]["entry_id"], entry.entry_id);
        assert_eq!(pending[0]["event"]["domain"], "fixture");
        assert!(!persisted.to_string().contains("parent-private-value"));

        // Construction and observer registration deliberately do not replay.
        let reopened = ConfigEntries::new(runtime).unwrap();
        let mut runtime_events = reopened.runtime.subscribe();
        let replay = Arc::new(CleanupObserver::default());
        let replay_trait = register_cleanup_observer(&reopened, &replay);
        assert!(replay.events.lock().unwrap().is_empty());
        assert_eq!(reopened.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(
            *replay.events.lock().unwrap(),
            vec![ConfigLifecycleEvent::EntryRemoved {
                entry_id: entry.entry_id.clone(),
                domain: "fixture".into(),
            }]
        );
        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        assert!(
            !persisted
                .as_object()
                .unwrap()
                .contains_key("pending_cleanup")
        );
        assert!(matches!(
            runtime_events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(reopened.unregister_lifecycle_observer(&replay_trait));
    }

    #[test]
    fn child_cleanup_journal_keeps_exact_identity_and_sibling_unchanged() {
        let store = Arc::new(MemoryStore::default());
        let manager = ConfigEntries::new(runtime(store.clone())).unwrap();
        let entry = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
            .unwrap();
        let mut private_data = Map::new();
        private_data.insert("credential".into(), json!("removed-child-private"));
        let child = manager
            .create_subentry(
                &entry.entry_id,
                NewConfigSubentry::new("fixture_child", "Removed child", private_data),
            )
            .unwrap();
        let mut sibling_data = Map::new();
        sibling_data.insert("setting".into(), json!("sibling-retained"));
        let sibling = manager
            .create_subentry(
                &entry.entry_id,
                NewConfigSubentry::new("fixture_child", "Sibling", sibling_data),
            )
            .unwrap();

        let observer = Arc::new(CleanupObserver::default());
        observer.fail_once.store(true, Ordering::SeqCst);
        let _observer_trait = register_cleanup_observer(&manager, &observer);
        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .is_err()
        );
        assert!(!manager.subentry_exists(&entry.entry_id, &child.subentry_id));
        assert!(manager.subentry_exists(&entry.entry_id, &sibling.subentry_id));
        let sibling_after = manager
            .get_subentry(&entry.entry_id, &sibling.subentry_id)
            .unwrap();
        assert_eq!(sibling_after.title, sibling.title);
        assert_eq!(sibling_after.data, sibling.data);

        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        let pending = persisted["pending_cleanup"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        let record = pending[0].as_object().unwrap();
        assert_eq!(
            record.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["event", "id"]
        );
        let event = record["event"].as_object().unwrap();
        assert_eq!(
            event.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["entry_id", "subentry_id", "type"]
        );
        assert_eq!(event["type"], "subentry_removed");
        assert_eq!(event["entry_id"], entry.entry_id);
        assert_eq!(event["subentry_id"], child.subentry_id);
        assert!(!persisted.to_string().contains("removed-child-private"));

        assert_eq!(manager.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(
            observer.events.lock().unwrap().last(),
            Some(&ConfigLifecycleEvent::SubentryRemoved {
                entry_id: entry.entry_id.clone(),
                subentry_id: child.subentry_id.clone(),
            })
        );
        assert!(
            store
                .get_data(METADATA_KEY)
                .unwrap()
                .unwrap()
                .as_object()
                .unwrap()
                .get("pending_cleanup")
                .is_none()
        );
    }

    #[test]
    fn parent_cleanup_supersedes_pending_child_records() {
        let store = Arc::new(MemoryStore::default());
        let manager = ConfigEntries::new(runtime(store.clone())).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
            .unwrap();
        let child = manager
            .create_subentry(
                &entry.entry_id,
                NewConfigSubentry::new("fixture_child", "Child", Map::new()),
            )
            .unwrap();
        let observer = Arc::new(CleanupObserver::default());
        observer.fail_once.store(true, Ordering::SeqCst);
        let _observer_trait = register_cleanup_observer(&manager, &observer);

        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .is_err()
        );
        observer.fail_once.store(true, Ordering::SeqCst);
        assert!(manager.remove_entry(&entry.entry_id).is_err());

        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        let pending = persisted["pending_cleanup"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["event"]["type"], "entry_removed");
        assert_eq!(pending[0]["event"]["entry_id"], entry.entry_id);

        assert_eq!(manager.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(observer.events.lock().unwrap().len(), 3);
        assert_eq!(
            observer.events.lock().unwrap().last(),
            Some(&ConfigLifecycleEvent::EntryRemoved {
                entry_id: entry.entry_id,
                domain: "fixture".into(),
            })
        );
        assert!(!store_has_pending_cleanup(&manager));
    }

    #[test]
    fn failed_cleanup_acknowledgement_remains_retriable() {
        let store = Arc::new(FailingMetadataStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let manager = ConfigEntries::new(runtime).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
            .unwrap();
        let observer = Arc::new(CleanupObserver::default());
        *observer
            .fail_store_after_success
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store.clone());
        let _observer_trait = register_cleanup_observer(&manager, &observer);

        assert!(manager.remove_entry(&entry.entry_id).is_err());
        assert!(manager.get_entry(&entry.entry_id).is_none());
        assert_eq!(
            store.get_data(METADATA_KEY).unwrap().unwrap()["pending_cleanup"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        store.fail_metadata.store(false, Ordering::SeqCst);
        assert_eq!(manager.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(observer.events.lock().unwrap().len(), 2);
        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        assert!(
            !persisted
                .as_object()
                .unwrap()
                .contains_key("pending_cleanup")
        );
    }

    #[test]
    fn replay_is_reentrant_and_allows_removal_and_self_unregistration() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let first = manager
            .create_entry(NewConfigEntry::new("fixture", "First", Map::new()))
            .unwrap();
        let second = manager
            .create_entry(NewConfigEntry::new("fixture", "Second", Map::new()))
            .unwrap();

        // Leave only the first cleanup pending until the observer is installed.
        manager.remove_entry(&first.entry_id).unwrap();
        let observer = Arc::new(CleanupObserver::default());
        observer.replay_in_callback.store(true, Ordering::SeqCst);
        observer.unregister_self.store(true, Ordering::SeqCst);
        *observer
            .remove_other_entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(second.entry_id.clone());
        let observer_trait = register_cleanup_observer(&manager, &observer);

        assert_eq!(manager.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(*observer.nested_replay_counts.lock().unwrap(), vec![0]);
        assert_eq!(observer.events.lock().unwrap().len(), 2);
        assert!(manager.get_entry(&second.entry_id).is_none());
        assert!(!manager.unregister_lifecycle_observer(&observer_trait));
        assert!(!store_has_pending_cleanup(&manager));
    }

    #[test]
    fn child_cleanup_that_removes_parent_does_not_publish_a_stale_update() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let manager = ConfigEntries::new(runtime.clone()).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let parent = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
            .unwrap();
        let child = manager
            .create_subentry(
                &parent.entry_id,
                NewConfigSubentry::new("child", "Child", Map::new()),
            )
            .unwrap();
        let observer = Arc::new(CleanupObserver::default());
        *observer.remove_other_entry.lock().unwrap() = Some(parent.entry_id.clone());
        observer.unregister_self.store(true, Ordering::SeqCst);
        let _observer_trait = register_cleanup_observer(&manager, &observer);
        let mut events = runtime.subscribe();
        manager
            .remove_subentry(&parent.entry_id, &child.subentry_id)
            .unwrap();
        assert!(manager.get_entry(&parent.entry_id).is_none());
        let changes: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.event_type == "config_entry_changed")
            .filter(|event| event.data["entry"]["entry_id"] == parent.entry_id)
            .map(|event| event.data["type"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            changes.last().map(String::as_str),
            Some("removed"),
            "{changes:?}"
        );
        assert!(!store_has_pending_cleanup(&manager));
    }

    #[test]
    fn overlapping_replay_dispatches_a_record_once_and_preserves_it_until_ack() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager
            .create_entry(NewConfigEntry::new("fixture", "Parent", Map::new()))
            .unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();

        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let observer = Arc::new(BlockingCleanupObserver {
            events: std::sync::Mutex::new(Vec::new()),
            entered: entered_sender,
            release: std::sync::Mutex::new(release_receiver),
        });
        let observer_trait: Arc<dyn ConfigLifecycleObserver> = observer.clone();
        manager.register_lifecycle_observer(observer_trait).unwrap();
        let replay_manager = manager.clone();
        let replay = std::thread::spawn(move || replay_manager.replay_pending_cleanup());

        entered_receiver
            .recv_timeout(StdDuration::from_secs(2))
            .expect("first replay enters observer");
        assert_eq!(manager.replay_pending_cleanup().unwrap(), 0);
        assert_eq!(observer.events.lock().unwrap().len(), 1);
        release_sender.send(()).unwrap();
        assert_eq!(replay.join().unwrap().unwrap(), 1);
        assert_eq!(observer.events.lock().unwrap().len(), 1);
        assert!(!store_has_pending_cleanup(&manager));
    }

    #[test]
    fn failed_persistence_never_notifies_lifecycle_observers() {
        let store = Arc::new(FailingMetadataStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let manager = ConfigEntries::new(runtime).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();
        let child = manager
            .create_subentry(
                &entry.entry_id,
                NewConfigSubentry::new("fixture_child", "Child", Map::new()),
            )
            .unwrap();
        let observer = Arc::new(LifecycleObserver::default());
        let observer_trait: Arc<dyn ConfigLifecycleObserver> = observer.clone();
        manager
            .register_lifecycle_observer(observer_trait.clone())
            .unwrap();
        observer.manager.lock().unwrap().replace(manager.clone());

        store.fail_metadata.store(true, Ordering::SeqCst);
        assert!(
            manager
                .remove_subentry(&entry.entry_id, &child.subentry_id)
                .is_err()
        );
        assert!(
            manager
                .disable_entry(&entry.entry_id, Some("user"))
                .is_err()
        );
        assert!(manager.remove_entry(&entry.entry_id).is_err());
        assert!(manager.subentry_exists(&entry.entry_id, &child.subentry_id));
        assert_eq!(
            manager.get_entry(&entry.entry_id).unwrap().disabled_by,
            None
        );
        assert!(observer.events.lock().unwrap().is_empty());
        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        assert!(
            !persisted
                .as_object()
                .unwrap()
                .contains_key("pending_cleanup")
        );

        store.fail_metadata.store(false, Ordering::SeqCst);
        manager
            .remove_subentry(&entry.entry_id, &child.subentry_id)
            .unwrap();
        manager
            .disable_entry(&entry.entry_id, Some("user"))
            .unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();
        assert_eq!(observer.events.lock().unwrap().len(), 4);
        manager.unregister_lifecycle_observer(&observer_trait);
    }

    #[test]
    fn old_config_store_migrates_absent_and_legacy_empty_subentry_fields() {
        let store = Arc::new(MemoryStore::default());
        let manager = ConfigEntries::new(runtime(store.clone())).unwrap();
        let entry = manager.create_entry(seed()).unwrap();
        let mut document = store.get_data(METADATA_KEY).unwrap().unwrap();
        document["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("subentries");
        store.set_data(METADATA_KEY, &document).unwrap();
        drop(manager);

        let restored = ConfigEntries::new(runtime(store.clone())).unwrap();
        assert!(
            restored
                .list_subentries(&entry.entry_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get_data(METADATA_KEY).unwrap().unwrap()["entries"][0]["subentries"],
            json!([])
        );
        drop(restored);

        let mut document = store.get_data(METADATA_KEY).unwrap().unwrap();
        document["entries"][0]["subentries"] = json!({});
        store.set_data(METADATA_KEY, &document).unwrap();
        let restored_legacy_map = ConfigEntries::new(runtime(store)).unwrap();
        assert!(
            restored_legacy_map
                .list_subentries(&entry.entry_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn subentries_survive_sqlite_runtime_restart_with_stable_private_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config-subentries.sqlite");
        let (entry_id, subentry_id) = {
            let store = Arc::new(hass_storage::SqliteStore::open(&path).unwrap());
            let runtime = Runtime::new(store).unwrap();
            let manager = ConfigEntries::new(runtime).unwrap();
            register(&manager, Arc::new(FixtureDriver::new()));
            let entry = manager.create_entry(seed()).unwrap();
            let mut data = Map::new();
            data.insert("token".into(), json!("sqlite-private-secret"));
            let subentry = manager
                .create_subentry(
                    &entry.entry_id,
                    NewConfigSubentry::new("fixture_child", "SQLite child", data),
                )
                .unwrap();
            (entry.entry_id, subentry.subentry_id)
        };

        let store = Arc::new(hass_storage::SqliteStore::open(&path).unwrap());
        let manager = ConfigEntries::new(Runtime::new(store).unwrap()).unwrap();
        let restored = manager.get_subentry(&entry_id, &subentry_id).unwrap();
        assert_eq!(restored.title, "SQLite child");
        assert_eq!(restored.data["token"], "sqlite-private-secret");
        assert!(manager.subentry_exists(&entry_id, &subentry_id));
        assert_eq!(
            manager.get_entry(&entry_id).unwrap().public_json()["num_subentries"],
            1
        );
        assert!(restored.public_json().get("data").is_none());
    }

    #[test]
    fn discovery_keys_are_typed_private_deduplicated_and_persisted() {
        let string_key = discovery_key("uuid:string-device");
        let string_json = serde_json::to_value(&string_key).unwrap();
        assert_eq!(string_json["key"], "uuid:string-device");
        assert_eq!(
            serde_json::from_value::<DiscoveryKey>(string_json).unwrap(),
            string_key
        );

        let tuple_key = DiscoveryKey {
            domain: "zeroconf".into(),
            key: DiscoveryKeyValue::Tuple(vec![
                "_fixture._tcp.local.".into(),
                "fixture.local.".into(),
            ]),
            version: 2,
        };
        let tuple_json = serde_json::to_value(&tuple_key).unwrap();
        assert_eq!(
            tuple_json["key"],
            json!(["_fixture._tcp.local.", "fixture.local."])
        );
        assert_eq!(
            serde_json::from_value::<DiscoveryKey>(tuple_json).unwrap(),
            tuple_key
        );

        let store = Arc::new(MemoryStore::default());
        let manager = ConfigEntries::new(runtime(store.clone())).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();
        let serialized_empty = serde_json::to_value(&entry).unwrap();
        assert!(serialized_empty.get("discovery_keys").is_none());
        let mut legacy_entry = serialized_empty;
        legacy_entry
            .as_object_mut()
            .unwrap()
            .remove("discovery_keys");
        let restored_legacy: ConfigEntry = serde_json::from_value(legacy_entry).unwrap();
        assert!(restored_legacy.discovery_keys.is_empty());

        assert!(
            manager
                .associate_discovery_key(&entry.entry_id, string_key.clone())
                .unwrap()
        );
        assert!(
            !manager
                .associate_discovery_key(&entry.entry_id, string_key.clone())
                .unwrap()
        );
        assert!(
            manager
                .associate_discovery_key(&entry.entry_id, tuple_key.clone())
                .unwrap()
        );
        for index in 1..=10 {
            assert!(
                manager
                    .associate_discovery_key(
                        &entry.entry_id,
                        discovery_key(&format!("uuid:device-{index}")),
                    )
                    .unwrap()
            );
        }

        assert!(manager.entries_for_discovery_key(&string_key).is_empty());
        assert_eq!(
            manager.entries_for_discovery_key(&tuple_key)[0].entry_id,
            entry.entry_id
        );
        let latest_key = discovery_key("uuid:device-10");
        assert_eq!(
            manager.entries_for_discovery_key(&latest_key)[0].entry_id,
            entry.entry_id
        );
        let updated = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(updated.discovery_keys["ssdp"].len(), 10);
        assert!(updated.public_json().get("discovery_keys").is_none());
        assert!(updated.public_json().get("data").is_none());
        let debug = format!("{updated:?}");
        assert!(!debug.contains("uuid:device-10"));
        assert!(!debug.contains("secret-value"));

        let restored = ConfigEntries::new(manager.runtime.clone()).unwrap();
        let restored_entry = restored.get_entry(&entry.entry_id).unwrap();
        assert_eq!(restored_entry.discovery_keys["ssdp"].len(), 10);
        assert_eq!(
            restored.entries_for_discovery_key(&latest_key)[0].entry_id,
            entry.entry_id
        );
    }

    #[test]
    fn discovery_flows_match_original_data_and_dismiss_only_matching_config_source() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        manager
            .register_driver(Arc::new(DiscoveryFlowDriver))
            .unwrap();
        let mut existing_seed =
            NewConfigEntry::new("discovery_fixture", "Existing fixture", Map::new());
        existing_seed.unique_id = Some("known-device".into());
        let existing = manager.create_entry(existing_seed).unwrap();

        let options = manager.init_options_flow(&existing.entry_id).unwrap();
        let options_flow_id = options["flow_id"].as_str().unwrap();
        let subentry = manager
            .init_subentry_flow(
                &existing.entry_id,
                "fixture_child",
                "user",
                None,
                Map::new(),
                None,
            )
            .unwrap();
        let subentry_flow_id = subentry["flow_id"].as_str().unwrap();

        let key = discovery_key("uuid:known-device");
        let initial_data = discovery_info("urn:fixture:device", "http://192.0.2.2/device.xml");
        let context = discovery_context(&key, Some("known-device"));
        let started = manager
            .init_config_flow_with_input(
                "discovery_fixture",
                "ssdp",
                context,
                Some(initial_data.clone()),
            )
            .unwrap();
        assert_eq!(started["type"], "form");
        let flow_id = started["flow_id"].as_str().unwrap();

        let associated = manager.entries_for_discovery_key(&key);
        assert_eq!(associated.len(), 1);
        assert_eq!(associated[0].entry_id, existing.entry_id);
        let match_context =
            Map::from_iter([("discovery_key".into(), serde_json::to_value(&key).unwrap())]);
        assert!(manager.has_matching_discovery_flow(
            "discovery_fixture",
            "ssdp",
            &match_context,
            &initial_data,
        ));
        assert!(!manager.has_matching_discovery_flow(
            "other_fixture",
            "ssdp",
            &match_context,
            &initial_data,
        ));
        assert!(!manager.has_matching_discovery_flow(
            "discovery_fixture",
            "zeroconf",
            &match_context,
            &initial_data,
        ));
        let changed_initial_data =
            discovery_info("urn:fixture:other", "http://192.0.2.2/other.xml");
        assert!(!manager.has_matching_discovery_flow(
            "discovery_fixture",
            "ssdp",
            &match_context,
            &changed_initial_data,
        ));

        let mut later_input = Map::new();
        later_input.insert("token".into(), json!("user-entered-token"));
        assert_eq!(
            manager.submit_flow(flow_id, later_input.clone()).unwrap()["type"],
            "form"
        );
        assert!(manager.has_matching_discovery_flow(
            "discovery_fixture",
            "ssdp",
            &match_context,
            &initial_data,
        ));
        assert!(!manager.has_matching_discovery_flow(
            "discovery_fixture",
            "ssdp",
            &match_context,
            &later_input,
        ));

        let mut protected_context = discovery_context(&key, Some("known-device"));
        protected_context.insert("dismiss_protected".into(), json!(true));
        let protected = manager
            .init_config_flow_with_input(
                "discovery_fixture",
                "ssdp",
                protected_context,
                Some(initial_data.clone()),
            )
            .unwrap();
        let protected_flow_id = protected["flow_id"].as_str().unwrap();

        let other_source = manager
            .init_config_flow_with_input(
                "discovery_fixture",
                "zeroconf",
                discovery_context(&key, Some("known-device")),
                Some(initial_data.clone()),
            )
            .unwrap();
        let other_source_flow_id = other_source["flow_id"].as_str().unwrap();

        let unknown_key = discovery_key("uuid:unknown-device");
        let unknown_info = discovery_info("urn:fixture:unknown", "http://192.0.2.3/device.xml");
        let unknown = manager
            .init_config_flow_with_input(
                "discovery_fixture",
                "ssdp",
                discovery_context(&unknown_key, Some("absent-device")),
                Some(unknown_info.clone()),
            )
            .unwrap();
        let unknown_flow_id = unknown["flow_id"].as_str().unwrap();
        assert!(manager.entries_for_discovery_key(&unknown_key).is_empty());

        let dismissed = manager
            .dismiss_discovery_flows("discovery_fixture", "ssdp", |data| {
                data.get("ssdp_st").and_then(Value::as_str) == Some("urn:fixture:device")
                    && data.get("ssdp_location").and_then(Value::as_str)
                        == Some("http://192.0.2.2/device.xml")
            })
            .unwrap();
        assert_eq!(dismissed, 1);
        assert!(
            manager
                .active_flow(ConfigFlowType::Config, flow_id)
                .is_err()
        );
        assert!(
            manager
                .active_flow(ConfigFlowType::Config, protected_flow_id)
                .is_ok()
        );
        assert!(
            manager
                .active_flow(ConfigFlowType::Config, other_source_flow_id)
                .is_ok()
        );
        assert!(
            manager
                .active_flow(ConfigFlowType::Config, unknown_flow_id)
                .is_ok()
        );
        assert!(
            manager
                .active_flow(ConfigFlowType::Options, options_flow_id)
                .is_ok()
        );
        assert!(
            manager
                .active_flow(ConfigFlowType::ConfigSubentries, subentry_flow_id)
                .is_ok()
        );
    }

    #[test]
    fn removal_notifies_the_keys_present_at_the_deletion_commit() {
        struct AddKeyDuringRemove(std::sync::Weak<ConfigEntries>);
        impl ConfigEntryDriver for AddKeyDuringRemove {
            fn domain(&self) -> &str {
                "fixture"
            }
            fn setup(&self, _: &Runtime, _: &ConfigEntry) -> Result<SetupOutcome> {
                Ok(SetupOutcome::Ready)
            }
            fn unload(&self, _: &Runtime, _: &ConfigEntry) -> Result<()> {
                Ok(())
            }
            fn config_flow(&self, _: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
                Ok(ConfigFlowResult::Abort {
                    reason: "unused".into(),
                })
            }
            fn remove(&self, _: &Runtime, entry: &ConfigEntry) -> Result<()> {
                self.0
                    .upgrade()
                    .unwrap()
                    .associate_discovery_key(&entry.entry_id, discovery_key("uuid:late"))?;
                Ok(())
            }
        }
        struct Events(Arc<Mutex<Vec<DiscoveryKeyRemoval>>>);
        impl DiscoveryKeyRemovalObserver for Events {
            fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> Result<()> {
                self.0.lock().unwrap().push(event.clone());
                Ok(())
            }
        }
        let manager =
            Arc::new(ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap());
        manager
            .register_driver(Arc::new(AddKeyDuringRemove(Arc::downgrade(&manager))))
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let _guard = manager
            .observe_discovery_key_removals(Arc::new(Events(events.clone())))
            .unwrap();
        let entry = manager.create_entry(seed()).unwrap();
        manager
            .associate_discovery_key(&entry.entry_id, discovery_key("uuid:first"))
            .unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();
        assert_eq!(
            events.lock().unwrap()[0].keys,
            vec![discovery_key("uuid:first"), discovery_key("uuid:late")]
        );
    }

    #[test]
    fn committed_removal_notifies_discovery_after_cleanup_failure_without_replay_duplicates() {
        struct Failure;
        impl ConfigLifecycleObserver for Failure {
            fn config_lifecycle_changed(&self, _: &ConfigLifecycleEvent) -> Result<()> {
                Err(Error::Storage("cleanup fixture failure".into()))
            }
        }
        struct Events(Arc<Mutex<Vec<DiscoveryKeyRemoval>>>);
        impl DiscoveryKeyRemovalObserver for Events {
            fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> Result<()> {
                self.0.lock().unwrap().push(event.clone());
                Ok(())
            }
        }
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();
        manager
            .associate_discovery_key(&entry.entry_id, discovery_key("uuid:removed"))
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let _discovery = manager
            .observe_discovery_key_removals(Arc::new(Events(events.clone())))
            .unwrap();
        let failure = manager.observe_lifecycle(Arc::new(Failure)).unwrap();
        assert!(manager.remove_entry(&entry.entry_id).is_err());
        assert!(manager.get_entry(&entry.entry_id).is_none());
        assert_eq!(events.lock().unwrap().len(), 1);
        drop(failure);
        let _success = manager
            .observe_lifecycle(Arc::new(CleanupSeenObserver(Arc::new(AtomicBool::new(
                false,
            )))))
            .unwrap();
        assert_eq!(manager.replay_pending_cleanup().unwrap(), 1);
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[test]
    fn discovery_flow_creation_and_removal_notify_after_cleanup_outside_locks() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        manager
            .register_driver(Arc::new(DiscoveryFlowDriver))
            .unwrap();
        let key = discovery_key("uuid:removal-device");
        let started = manager
            .init_config_flow_with_input(
                "discovery_fixture",
                "ssdp",
                discovery_context(&key, None),
                Some(discovery_info(
                    "urn:fixture:remove",
                    "http://192.0.2.4/device.xml",
                )),
            )
            .unwrap();
        let mut completion = Map::new();
        completion.insert("complete".into(), json!(true));
        let completed = manager
            .submit_flow(started["flow_id"].as_str().unwrap(), completion)
            .unwrap();
        let entry_id = completed["result"]["entry_id"].as_str().unwrap();
        assert!(Uuid::parse_str(entry_id).is_ok());
        let entry = manager.get_entry(entry_id).unwrap();
        assert_eq!(entry.discovery_keys["ssdp"], vec![key.clone()]);
        assert!(entry.public_json().get("discovery_keys").is_none());
        assert!(!format!("{entry:?}").contains("removal-device"));

        let cleanup_seen = Arc::new(AtomicBool::new(false));
        let cleanup_observer: Arc<dyn ConfigLifecycleObserver> =
            Arc::new(CleanupSeenObserver(cleanup_seen.clone()));
        let _cleanup_guard = manager.observe_lifecycle(cleanup_observer).unwrap();
        let probe = Arc::new(DiscoveryRemovalProbe {
            manager: Mutex::new(Some(manager.clone())),
            cleanup_seen: cleanup_seen.clone(),
            cleanup_preceded: AtomicBool::new(false),
            entry_was_gone: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
        });
        let observer: Arc<dyn DiscoveryKeyRemovalObserver> = probe.clone();
        let observer_guard = manager
            .observe_discovery_key_removals(observer.clone())
            .unwrap();
        assert!(
            manager
                .register_discovery_key_removal_observer(observer.clone())
                .is_err()
        );
        let unrelated_observer: Arc<dyn DiscoveryKeyRemovalObserver> =
            Arc::new(CollectDiscoveryRemovalObserver::default());
        assert!(!manager.unregister_discovery_key_removal_observer(&unrelated_observer));
        let _panic_guard = manager
            .observe_discovery_key_removals(Arc::new(PanickingDiscoveryRemovalObserver))
            .unwrap();

        manager.remove_entry(entry_id).unwrap();
        assert!(manager.get_entry(entry_id).is_none());
        assert!(probe.cleanup_preceded.load(Ordering::SeqCst));
        assert!(probe.entry_was_gone.load(Ordering::SeqCst));
        let notifications = probe.events.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].entry_id, entry_id);
        assert_eq!(notifications[0].keys, vec![key]);
        drop(notifications);

        drop(observer_guard);
        let mut second_seed =
            NewConfigEntry::new("discovery_fixture", "Second fixture", Map::new());
        second_seed.unique_id = Some("second-device".into());
        let second_entry = manager.create_entry(second_seed).unwrap();
        manager
            .associate_discovery_key(&second_entry.entry_id, discovery_key("uuid:second-device"))
            .unwrap();
        manager.remove_entry(&second_entry.entry_id).unwrap();
        assert_eq!(probe.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn discovery_removal_replay_keeps_keys_out_of_cleanup_journal() {
        let store = Arc::new(MemoryStore::default());
        let manager = ConfigEntries::new(runtime(store.clone())).unwrap();
        manager
            .register_driver(Arc::new(DiscoveryFlowDriver))
            .unwrap();
        let mut seed = NewConfigEntry::new("discovery_fixture", "Journal fixture", Map::new());
        seed.unique_id = Some("journal-device".into());
        let entry = manager.create_entry(seed).unwrap();
        let key = discovery_key("uuid:journal-device");
        manager
            .associate_discovery_key(&entry.entry_id, key.clone())
            .unwrap();
        let live_observer = Arc::new(CollectDiscoveryRemovalObserver::default());
        let _live_guard = manager
            .observe_discovery_key_removals(live_observer.clone())
            .unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();
        assert_eq!(live_observer.0.lock().unwrap().len(), 1);

        let document = store.get_data(METADATA_KEY).unwrap().unwrap();
        let removal = &document["pending_cleanup"][0]["event"];
        assert_eq!(removal["entry_id"], entry.entry_id);
        assert_eq!(removal["domain"], "discovery_fixture");
        assert!(removal.get("keys").is_none());
        assert!(removal.get("discovery_keys").is_none());

        let restored = ConfigEntries::new(manager.runtime.clone()).unwrap();
        let replay_observer = Arc::new(CollectDiscoveryRemovalObserver::default());
        let _replay_guard = restored
            .observe_discovery_key_removals(replay_observer.clone())
            .unwrap();
        assert_eq!(restored.replay_pending_cleanup().unwrap(), 0);
        assert!(replay_observer.0.lock().unwrap().is_empty());
    }

    struct OAuthFlowTestDriver {
        setup_calls: AtomicUsize,
        on_create_calls: AtomicUsize,
        manager_slot: Arc<Mutex<Option<ConfigEntries>>>,
    }

    impl OAuthFlowTestDriver {
        fn new(manager_slot: Arc<Mutex<Option<ConfigEntries>>>) -> Self {
            Self {
                setup_calls: AtomicUsize::new(0),
                on_create_calls: AtomicUsize::new(0),
                manager_slot,
            }
        }

        fn external_result() -> ConfigFlowResult {
            ConfigFlowResult::ExternalStep {
                step_id: "auth".into(),
                url: "https://auth.example/authorize?client_id=fixture".into(),
                description_placeholders: None,
            }
        }
    }

    impl ConfigEntryDriver for OAuthFlowTestDriver {
        fn domain(&self) -> &str {
            "oauth_fixture"
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            self.setup_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            Ok(Self::external_result())
        }

        fn config_flow_async(
            &self,
            request: ConfigFlowRequest,
        ) -> Option<Pin<Box<dyn Future<Output = Result<ConfigFlowResult>> + Send + 'static>>>
        {
            let manager = self
                .manager_slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let step_id = request.step_id;
            Some(Box::pin(async move {
                assert!(matches!(step_id.as_str(), "user" | "reauth"));
                tokio::task::yield_now().await;
                if let Some(manager) = manager {
                    assert!(
                        manager
                            .flow_handlers()
                            .contains(&"oauth_fixture".to_owned())
                    );
                    let _ = manager.list_entries(None);
                }
                Ok(OAuthFlowTestDriver::external_result())
            }))
        }

        fn on_create_entry(
            &self,
            _manager: &ConfigEntries,
            _request: &ConfigFlowRequest,
            _entry: &ConfigEntry,
            _result: &ConfigFlowResult,
        ) -> Result<Option<ConfigFlowContinuation>> {
            self.on_create_calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    struct BlockingAsyncDriver {
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<tokio::sync::Notify>,
        future_drops: Arc<AtomicUsize>,
    }

    struct AsyncCallbackDrop(Arc<AtomicUsize>);

    impl Drop for AsyncCallbackDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ConfigEntryDriver for BlockingAsyncDriver {
        fn domain(&self) -> &str {
            "blocking_fixture"
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form("sync", None))
        }

        fn config_flow_async(
            &self,
            _request: ConfigFlowRequest,
        ) -> Option<Pin<Box<dyn Future<Output = Result<ConfigFlowResult>> + Send + 'static>>>
        {
            let started = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let release = Arc::clone(&self.release);
            let future_drops = Arc::clone(&self.future_drops);
            Some(Box::pin(async move {
                let _drop_guard = AsyncCallbackDrop(future_drops);
                if let Some(started) = started {
                    let _ = started.send(());
                }
                release.notified().await;
                let mut data = Map::new();
                data.insert("token".into(), json!("late-secret"));
                Ok(ConfigFlowResult::create_entry(
                    "Blocking account",
                    data,
                    None,
                ))
            }))
        }
    }

    #[tokio::test]
    async fn external_flow_shapes_and_single_creation_claim_are_exact() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let manager_slot = Arc::new(Mutex::new(Some(manager.clone())));
        let concrete = Arc::new(OAuthFlowTestDriver::new(manager_slot.clone()));
        let driver: Arc<dyn ConfigEntryDriver> = concrete.clone();
        manager.register_driver(driver.clone()).unwrap();
        let mut context = Map::new();
        context.insert("state_proof".into(), json!("private-proof"));
        let external = manager
            .init_config_flow_with_typed_input_async("oauth_fixture", "reauth", context, None, None)
            .await
            .unwrap();
        assert_eq!(
            external,
            json!({
                "flow_id": external["flow_id"],
                "handler": "oauth_fixture",
                "type": "external",
                "url": "https://auth.example/authorize?client_id=fixture",
                "description_placeholders": null,
                "step_id": "auth",
            })
        );
        assert!(!external.to_string().contains("private-proof"));
        assert!(
            !serde_json::to_string(&manager.flow_progress())
                .unwrap()
                .contains("private-proof")
        );
        let flow_id = external["flow_id"].as_str().unwrap();
        let callback = manager
            .claim_external_callback(flow_id, "oauth_fixture", "auth")
            .unwrap();
        let done = manager.publish_external_done(callback, "creation").unwrap();
        assert_eq!(
            done,
            json!({
                "flow_id": flow_id,
                "handler": "oauth_fixture",
                "type": "external_done",
                "step_id": "creation",
            })
        );
        assert!(!done.to_string().contains("private-proof"));
        let manager_a = manager.clone();
        let manager_b = manager.clone();
        let id_a = flow_id.to_owned();
        let id_b = flow_id.to_owned();
        let (left, right) = tokio::join!(
            tokio::task::spawn_blocking(move || {
                manager_a.claim_external_done(&id_a, "oauth_fixture", "creation")
            }),
            tokio::task::spawn_blocking(move || {
                manager_b.claim_external_done(&id_b, "oauth_fixture", "creation")
            }),
        );
        let left = left.unwrap().unwrap();
        let right = right.unwrap().unwrap();
        assert_ne!(left.is_some(), right.is_some());
        let lease = left.or(right).unwrap();
        assert!(manager.abort_flow(flow_id).is_ok());
        assert!(
            manager
                .commit_external_flow(
                    lease,
                    ConfigFlowResult::create_entry("Stale account", Map::new(), None),
                )
                .is_err()
        );
        assert!(manager.list_entries(Some("oauth_fixture")).is_empty());
        let external = manager
            .init_config_flow_async("oauth_fixture", "user", Map::new())
            .await
            .unwrap();
        let flow_id = external["flow_id"].as_str().unwrap();
        let callback = manager
            .claim_external_callback(flow_id, "oauth_fixture", "auth")
            .unwrap();
        manager.publish_external_done(callback, "creation").unwrap();
        let lease = manager
            .claim_external_done(flow_id, "oauth_fixture", "creation")
            .unwrap()
            .unwrap();
        manager.unregister_driver(&driver);
        manager.register_driver(driver.clone()).unwrap();
        assert!(
            manager
                .commit_external_flow(
                    lease,
                    ConfigFlowResult::create_entry("Stale registration", Map::new(), None),
                )
                .is_err()
        );
        assert!(manager.list_entries(Some("oauth_fixture")).is_empty());
        manager.shutdown_flow_tasks();
        manager.unregister_driver(&driver);
        manager_slot.lock().unwrap().take();
    }

    #[tokio::test]
    async fn external_reauth_merges_private_data_and_defers_callbacks() {
        let store = Arc::new(FailingMetadataStore::default());
        let manager = ConfigEntries::new(Runtime::new(store.clone()).unwrap()).unwrap();
        let manager_slot = Arc::new(Mutex::new(Some(manager.clone())));
        let concrete = Arc::new(OAuthFlowTestDriver::new(manager_slot.clone()));
        let driver: Arc<dyn ConfigEntryDriver> = concrete.clone();
        manager.register_driver(driver.clone()).unwrap();
        let mut data = Map::new();
        data.insert("keep".into(), json!("unrelated"));
        data.insert("auth_implementation".into(), json!("old"));
        data.insert("token".into(), json!({"access_token":"old-secret"}));
        let mut seed = NewConfigEntry::new("oauth_fixture", "OAuth account", data);
        seed.unique_id = Some("provider-account-1".into());
        seed.options.insert("polling".into(), json!(45));
        let original = manager.create_entry(seed).unwrap();

        let external = manager
            .begin_reauth_flow_async(&original.entry_id)
            .await
            .unwrap();
        let flow_id = external["flow_id"].as_str().unwrap();
        let callback = manager
            .claim_external_callback(flow_id, "oauth_fixture", "auth")
            .unwrap();
        manager.publish_external_done(callback, "creation").unwrap();
        let lease = manager
            .claim_external_done(flow_id, "oauth_fixture", "creation")
            .unwrap()
            .unwrap();
        let mut patch = Map::new();
        patch.insert("auth_implementation".into(), json!("new"));
        patch.insert("token".into(), json!({"access_token":"new-secret"}));
        let result = ConfigFlowResult::CreateEntry {
            title: "ignored replacement title".into(),
            data: patch,
            options: Map::new(),
            unique_id: Some("wrong-account".into()),
            version: 9,
            minor_version: 9,
        };
        let retry_lease = lease.clone();
        let events_before_failure = store.inner.events().unwrap().len();
        store.fail_metadata.store(true, Ordering::SeqCst);
        assert!(manager.commit_external_flow(lease, result.clone()).is_err());
        let unchanged = manager.get_entry(&original.entry_id).unwrap();
        assert_eq!(unchanged.data, original.data);
        assert!(unchanged.needs_reauth);
        assert_eq!(
            manager.flow_result(flow_id).unwrap()["type"],
            "external_done"
        );
        assert_eq!(store.inner.events().unwrap().len(), events_before_failure);
        store.fail_metadata.store(false, Ordering::SeqCst);
        let completion = manager.commit_external_flow(retry_lease, result).unwrap();
        let stored = manager.get_entry(&original.entry_id).unwrap();
        assert_eq!(stored.entry_id, original.entry_id);
        assert_eq!(stored.unique_id, original.unique_id);
        assert_eq!(stored.title, original.title);
        assert_eq!(stored.options, original.options);
        assert_eq!(stored.data["keep"], json!("unrelated"));
        assert_eq!(stored.data["auth_implementation"], json!("new"));
        assert_eq!(stored.data["token"]["access_token"], json!("new-secret"));
        assert!(!stored.needs_reauth);
        assert_eq!(concrete.setup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(concrete.on_create_calls.load(Ordering::SeqCst), 0);
        let debug_plan = format!("{completion:?}");
        assert!(!debug_plan.contains("new-secret"));
        assert!(!debug_plan.contains("old-secret"));
        let persisted = store.get_data(METADATA_KEY).unwrap().unwrap();
        assert_eq!(
            persisted["entries"][0]["data"]["token"]["access_token"],
            json!("new-secret")
        );
        assert!(manager.flow_result(flow_id).is_err());

        manager.finish_external_flow(completion).unwrap();
        assert_eq!(concrete.setup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(concrete.on_create_calls.load(Ordering::SeqCst), 0);
        let events = serde_json::to_string(&store.inner.events().unwrap()).unwrap();
        assert!(!events.contains("new-secret"));
        assert!(!events.contains("old-secret"));
        manager.shutdown_flow_tasks();
        manager.unregister_driver(&driver);
        manager_slot.lock().unwrap().take();
    }

    #[test]
    fn private_token_patch_is_atomic_and_events_are_redacted() {
        let failing_store = Arc::new(FailingMetadataStore::default());
        let manager = ConfigEntries::new(Runtime::new(failing_store.clone()).unwrap()).unwrap();
        register(&manager, Arc::new(FixtureDriver::new()));
        let entry = manager.create_entry(seed()).unwrap();
        let before = failing_store.get_data(METADATA_KEY).unwrap().unwrap();
        let events_before = failing_store.inner.events().unwrap().len();
        failing_store.fail_metadata.store(true, Ordering::SeqCst);
        let mut patch = Map::new();
        patch.insert("token".into(), json!("new-token-secret"));
        assert!(
            manager
                .patch_entry_data(&entry.entry_id, "fixture", patch)
                .is_err()
        );
        assert_eq!(manager.get_entry(&entry.entry_id).unwrap().data, entry.data);
        assert_eq!(
            failing_store.get_data(METADATA_KEY).unwrap().unwrap(),
            before
        );
        assert_eq!(failing_store.inner.events().unwrap().len(), events_before);
        failing_store.fail_metadata.store(false, Ordering::SeqCst);
        let mut patch = Map::new();
        patch.insert("token".into(), json!("new-token-secret"));
        let updated = manager
            .patch_entry_data(&entry.entry_id, "fixture", patch)
            .unwrap();
        assert_eq!(updated.data["token"], json!("new-token-secret"));
        let events = serde_json::to_string(&failing_store.inner.events().unwrap()).unwrap();
        assert!(!events.contains("new-token-secret"));
        assert!(
            manager
                .patch_entry_data(&entry.entry_id, "other_fixture", Map::new())
                .is_err()
        );
    }

    #[tokio::test]
    async fn committed_external_activation_runs_despite_persistent_event_failure() {
        let store = Arc::new(FailingMetadataStore::default());
        let manager = ConfigEntries::new(Runtime::new(store.clone()).unwrap()).unwrap();
        let concrete = Arc::new(OAuthFlowTestDriver::new(Arc::new(Mutex::new(None))));
        let driver: Arc<dyn ConfigEntryDriver> = concrete.clone();
        manager.register_driver(driver.clone()).unwrap();
        let start = manager
            .init_config_flow_async("oauth_fixture", "user", Map::new())
            .await
            .unwrap();
        let id = start["flow_id"].as_str().unwrap();
        let callback = manager
            .claim_external_callback(id, "oauth_fixture", "auth")
            .unwrap();
        manager.publish_external_done(callback, "creation").unwrap();
        let lease = manager
            .claim_external_done(id, "oauth_fixture", "creation")
            .unwrap()
            .unwrap();
        let data = Map::from_iter([("token".into(), json!("committed-secret"))]);
        let completion = manager
            .commit_external_flow(lease, ConfigFlowResult::create_entry("Account", data, None))
            .unwrap();
        store.fail_events.store(true, Ordering::SeqCst);
        assert!(matches!(
            manager.finish_external_flow(completion),
            Err(Error::Storage(_))
        ));
        assert_eq!(concrete.setup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(concrete.on_create_calls.load(Ordering::SeqCst), 1);
        let entry = manager.list_entries(Some("oauth_fixture")).pop().unwrap();
        assert_eq!(entry.state, ConfigEntryState::Loaded);
        assert_eq!(entry.data["token"], "committed-secret");

        store.fail_events.store(false, Ordering::SeqCst);
        let reauth = manager
            .begin_reauth_flow_async(&entry.entry_id)
            .await
            .unwrap();
        let id = reauth["flow_id"].as_str().unwrap();
        let callback = manager
            .claim_external_callback(id, "oauth_fixture", "auth")
            .unwrap();
        manager.publish_external_done(callback, "creation").unwrap();
        let lease = manager
            .claim_external_done(id, "oauth_fixture", "creation")
            .unwrap()
            .unwrap();
        let data = Map::from_iter([("token".into(), json!("reauth-secret"))]);
        let completion = manager
            .commit_external_flow(lease, ConfigFlowResult::create_entry("ignored", data, None))
            .unwrap();
        store.fail_events.store(true, Ordering::SeqCst);
        assert!(matches!(
            manager.finish_external_flow(completion),
            Err(Error::Storage(_))
        ));
        assert_eq!(concrete.setup_calls.load(Ordering::SeqCst), 2);
        assert_eq!(concrete.on_create_calls.load(Ordering::SeqCst), 1);
        let current = manager.get_entry(&entry.entry_id).unwrap();
        assert_eq!(current.state, ConfigEntryState::Loaded);
        assert!(!current.needs_reauth);
        assert_eq!(current.data["token"], "reauth-secret");
        let reopened = ConfigEntries::new(manager.runtime.clone()).unwrap();
        assert_eq!(
            reopened.get_entry(&entry.entry_id).unwrap().data,
            current.data
        );
        manager.shutdown_flow_tasks_async().await.unwrap();
        manager.unregister_driver(&driver);
    }

    #[tokio::test]
    async fn running_flow_progress_filters_reserved_context_before_callback_returns() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(BlockingAsyncDriver {
            started: Mutex::new(Some(started_sender)),
            release: release.clone(),
            future_drops: Arc::new(AtomicUsize::new(0)),
        });
        manager.register_driver(driver.clone()).unwrap();
        let context = json!({"state_proof":"private-proof", "client_secret":"private-client", "title_placeholders":{"name":"Fixture", "refresh_token":"private-refresh"}, "metadata":[[{"access_token":"private-array-token"}]], "entry_id":"entry-identity"}).as_object().unwrap().clone();
        let caller_manager = manager.clone();
        let caller = tokio::spawn(async move {
            caller_manager
                .init_config_flow_async("blocking_fixture", "discovery", context)
                .await
        });
        started_receiver.await.unwrap();
        let progress = manager.flow_progress();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0]["context"]["entry_id"], "entry-identity");
        assert_eq!(
            progress[0]["context"]["title_placeholders"]["name"],
            "Fixture"
        );
        assert!(
            !serde_json::to_string(&progress)
                .unwrap()
                .contains("private-")
        );
        release.notify_one();
        caller.await.unwrap().unwrap();
        manager.shutdown_flow_tasks_async().await.unwrap();
        manager.unregister_driver(&driver);
    }

    #[tokio::test]
    async fn async_flow_tasks_survive_caller_cancel_but_abort_blocks_late_create() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let future_drops = Arc::new(AtomicUsize::new(0));
        let driver = Arc::new(BlockingAsyncDriver {
            started: Mutex::new(Some(started_sender)),
            release: Arc::clone(&release),
            future_drops: Arc::clone(&future_drops),
        });
        let driver_trait: Arc<dyn ConfigEntryDriver> = driver;
        manager.register_driver(driver_trait.clone()).unwrap();

        let caller_manager = manager.clone();
        let caller = tokio::spawn(async move {
            caller_manager
                .init_config_flow_async("blocking_fixture", "user", Map::new())
                .await
        });
        started_receiver.await.unwrap();
        caller.abort();
        let _ = caller.await;
        release.notify_one();
        tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                if !manager.list_entries(Some("blocking_fixture")).is_empty() {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.list_entries(Some("blocking_fixture"))[0].data["token"],
            json!("late-secret")
        );
        manager.shutdown_flow_tasks_async().await.unwrap();
        assert_eq!(future_drops.load(Ordering::SeqCst), 1);
        manager.unregister_driver(&driver_trait);

        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let future_drops = Arc::new(AtomicUsize::new(0));
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(BlockingAsyncDriver {
            started: Mutex::new(Some(started_sender)),
            release: Arc::clone(&release),
            future_drops: Arc::clone(&future_drops),
        });
        manager.register_driver(driver.clone()).unwrap();
        let caller_manager = manager.clone();
        let caller = tokio::spawn(async move {
            caller_manager
                .init_config_flow_async("blocking_fixture", "user", Map::new())
                .await
        });
        started_receiver.await.unwrap();
        let flow_id = manager.lock_state().flows.keys().next().cloned().unwrap();
        let abort_manager = manager.clone();
        let abort_flow_id = flow_id.clone();
        let abort_task =
            tokio::spawn(async move { abort_manager.abort_flow_async(&abort_flow_id).await });
        let shutdown_manager = manager.clone();
        let shutdown_task =
            tokio::spawn(async move { shutdown_manager.shutdown_flow_tasks_async().await });
        let (abort_result, shutdown_result) =
            tokio::time::timeout(StdDuration::from_secs(2), async {
                tokio::join!(abort_task, shutdown_task)
            })
            .await
            .unwrap();
        abort_result.unwrap().unwrap();
        shutdown_result.unwrap().unwrap();
        assert!(caller.await.unwrap().is_err());
        assert_eq!(future_drops.load(Ordering::SeqCst), 1);
        assert!(manager.list_entries(Some("blocking_fixture")).is_empty());
        assert!(manager.flow_result(&flow_id).is_err());
        manager.unregister_driver(&driver);
    }

    struct BlockingSyncDriver {
        started: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        completed: Arc<AtomicUsize>,
    }

    impl ConfigEntryDriver for BlockingSyncDriver {
        fn domain(&self) -> &str {
            "blocking_sync_fixture"
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            if let Some(started) = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let _ = started.send(());
            }
            let (released, condition) = &*self.release;
            let mut released = released.lock().unwrap_or_else(PoisonError::into_inner);
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            let mut data = Map::new();
            data.insert("token".into(), json!("blocking-secret"));
            Ok(ConfigFlowResult::create_entry(
                "Blocking sync account",
                data,
                None,
            ))
        }
    }

    #[tokio::test]
    async fn abort_persistence_failure_still_joins_the_running_blocking_callback() {
        let store = Arc::new(FailingMetadataStore::default());
        let manager = ConfigEntries::new(Runtime::new(store.clone()).unwrap()).unwrap();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let completed = Arc::new(AtomicUsize::new(0));
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(BlockingSyncDriver {
            started: Mutex::new(Some(started_sender)),
            release: release.clone(),
            completed: completed.clone(),
        });
        manager.register_driver(driver.clone()).unwrap();
        let caller_manager = manager.clone();
        let caller = tokio::spawn(async move {
            caller_manager
                .init_config_flow_async("blocking_sync_fixture", "user", Map::new())
                .await
        });
        tokio::task::spawn_blocking(move || {
            started_receiver.recv_timeout(StdDuration::from_secs(5))
        })
        .await
        .unwrap()
        .unwrap();
        let id = manager.lock_state().flows.keys().next().unwrap().clone();
        store.fail_metadata.store(true, Ordering::SeqCst);
        let abort_manager = manager.clone();
        let mut abort = tokio::spawn(async move { abort_manager.abort_flow_async(&id).await });
        let returned_early = tokio::time::timeout(StdDuration::from_millis(25), &mut abort)
            .await
            .is_ok();
        {
            let (released, condition) = &*release;
            *released.lock().unwrap() = true;
            condition.notify_all();
        }
        assert!(
            !returned_early,
            "abort returned before the blocking callback exited"
        );
        assert!(matches!(abort.await.unwrap(), Err(Error::Storage(_))));
        assert!(caller.await.unwrap().is_err());
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert!(
            manager
                .list_entries(Some("blocking_sync_fixture"))
                .is_empty()
        );
        manager.shutdown_flow_tasks_async().await.unwrap();
        assert!(manager.lock_state().flows.is_empty());
        manager.unregister_driver(&driver);
    }

    #[tokio::test]
    async fn shutdown_drain_joins_legacy_blocking_callback_before_returning() {
        let manager = ConfigEntries::new(runtime(Arc::new(MemoryStore::default()))).unwrap();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let completed = Arc::new(AtomicUsize::new(0));
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(BlockingSyncDriver {
            started: Mutex::new(Some(started_sender)),
            release: Arc::clone(&release),
            completed: Arc::clone(&completed),
        });
        manager.register_driver(driver.clone()).unwrap();
        let caller_manager = manager.clone();
        let caller = tokio::spawn(async move {
            caller_manager
                .init_config_flow_async("blocking_sync_fixture", "user", Map::new())
                .await
        });
        tokio::task::spawn_blocking(move || started_receiver.recv())
            .await
            .unwrap()
            .unwrap();

        let shutdown_manager = manager.clone();
        let shutdown =
            tokio::spawn(async move { shutdown_manager.shutdown_flow_tasks_async().await });
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        assert!(!shutdown.is_finished());
        {
            let (released, condition) = &*release;
            *released.lock().unwrap() = true;
            condition.notify_all();
        }
        shutdown.await.unwrap().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert!(caller.await.unwrap().is_err());
        assert!(
            manager
                .list_entries(Some("blocking_sync_fixture"))
                .is_empty()
        );
        manager.unregister_driver(&driver);
    }
}
