//! Durable config entries, native integration lifecycles, and config flows.
//!
//! Integration behavior is supplied by registered [`ConfigEntryDriver`]s. An
//! entry without a driver cannot be reported as successfully configured.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::{DateTime, Duration, Utc};
use hass_core::{Context, Error, HomeContext, Result, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

const METADATA_KEY: &str = "hass.config_entries.v1";
const DOCUMENT_VERSION: u32 = 1;
const MAX_RETRY_DELAY_SECS: u64 = 86_400;

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

/// Persisted config entry data. `data` and `options` are available to trusted
/// native drivers, but are intentionally omitted from ordinary API fragments.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigEntry {
    pub entry_id: String,
    pub domain: String,
    pub title: String,
    pub source: String,
    pub unique_id: Option<String>,
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
    result: Value,
}

#[derive(Clone)]
struct ConfigSubentryFlowSession {
    request: ConfigSubentryFlowRequest,
    result: Value,
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
    flows: BTreeMap<String, FlowSession>,
    subentry_flows: BTreeMap<String, ConfigSubentryFlowSession>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedDocument {
    document_version: u32,
    entries: Vec<ConfigEntry>,
    core_config: CoreConfig,
    #[serde(default)]
    core_config_initialized: bool,
}

/// Persistent config-entry, config-flow, and core-config manager.
#[derive(Clone)]
pub struct ConfigEntries {
    runtime: Runtime,
    state: Arc<Mutex<PersistedState>>,
    drivers: Arc<RwLock<HashMap<String, Arc<dyn ConfigEntryDriver>>>>,
    operation_lock: Arc<Mutex<()>>,
    lifecycle_observers: Arc<RwLock<Vec<Arc<dyn ConfigLifecycleObserver>>>>,
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

impl ConfigEntries {
    /// Load the config-entry and core-config document from runtime metadata.
    pub fn new(runtime: Runtime) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut core_config = CoreConfig::default();
        let mut core_config_initialized = false;
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
                flows: BTreeMap::new(),
                subentry_flows: BTreeMap::new(),
            })),
            drivers: Arc::new(RwLock::new(HashMap::new())),
            operation_lock: Arc::new(Mutex::new(())),
            lifecycle_observers: Arc::new(RwLock::new(Vec::new())),
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
        if drivers.contains_key(&domain) {
            return Err(Error::InvalidInput(format!(
                "integration driver already registered for {domain}"
            )));
        }
        drivers.insert(domain.clone(), Arc::clone(&driver));
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
            .get(&domain)
            .is_some_and(|driver| Arc::ptr_eq(driver, expected))
        {
            drivers.remove(&domain);
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

    /// Return a registered integration driver by domain.
    pub fn driver(&self, domain: &str) -> Option<Arc<dyn ConfigEntryDriver>> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(domain)
            .cloned()
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
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        handlers.sort();
        handlers
    }

    /// Return the current public form without executing its driver again.
    pub fn flow_result(&self, flow_id: &str) -> Result<Value> {
        let state = self.lock_state();
        state
            .flows
            .get(flow_id)
            .map(|flow| flow.result.clone())
            .or_else(|| {
                state
                    .subentry_flows
                    .get(flow_id)
                    .map(|flow| flow.result.clone())
            })
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
        self.mutate(|state| {
            let removed_entry_flow = state.flows.remove(flow_id).is_some();
            let removed_subentry_flow = state.subentry_flows.remove(flow_id).is_some();
            if !removed_entry_flow && !removed_subentry_flow {
                return Err(Error::NotFound("unknown config flow".into()));
            }
            Ok(())
        })?;
        self.notify_flow_progress(flow_id)
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
        let removed = self.mutate(|state| {
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
            Ok((entry.clone(), subentry))
        })?;
        let event = ConfigLifecycleEvent::SubentryRemoved {
            entry_id: entry_id.to_owned(),
            subentry_id: removed.1.subentry_id,
        };
        // Home Assistant clears child registry references before notifying
        // update listeners; Repairs cleanup follows the same post-commit order.
        let observer_result = self.notify_lifecycle_observers(&event);
        let entry_change_result = self.emit_entry_change("updated", &removed.0);
        observer_result?;
        entry_change_result?;
        Ok(true)
    }

    /// Create and persist a config entry, checking its identity and source.
    pub fn create_entry(&self, seed: NewConfigEntry) -> Result<ConfigEntry> {
        validate_new_entry(&seed)?;
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
        let entry = self.require_entry(entry_id)?;
        if entry.state.is_loaded() {
            self.unload_entry_locked(entry_id)?;
        }
        self.update_entry_metadata(entry_id, |current| {
            current.require_reload = false;
            current.state = ConfigEntryState::NotLoaded;
            current.reason = None;
            current.modified_at = Utc::now();
        })?;
        self.setup_entry_locked(entry_id)
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
        let success = operation_result?;
        entry_change_result?;
        observer_result?;
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
        self.mutate(|state| {
            state.entries.remove(entry_id);
            state
                .flows
                .retain(|_, flow| flow.request.entry_id.as_deref() != Some(entry_id));
            state
                .subentry_flows
                .retain(|_, flow| flow.request.entry_id != entry_id);
            Ok(())
        })?;
        drop(operation);
        // Source removal dispatch follows registry/issue cleanup. Keep both
        // outside manager locks so observers can reenter the committed state.
        self.notify_lifecycle_observers(&ConfigLifecycleEvent::EntryRemoved {
            entry_id: entry_id.to_owned(),
            domain: entry.domain.clone(),
        })?;
        self.emit_entry_change("removed", &entry)
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
        self.run_flow_step(request, FlowKind::Config, driver)
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
        mut context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
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
        self.run_flow_step(request, FlowKind::Config, driver)
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
        self.run_flow_step(request, FlowKind::Options, driver)
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
        mut context: Map<String, Value>,
        user_input: Option<Map<String, Value>>,
    ) -> Result<Value> {
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
        self.run_subentry_flow_step(request, driver)
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
        let driver = self.driver(&session.request.domain).ok_or_else(|| {
            Error::NotFound(format!(
                "no native integration driver registered for {}",
                session.request.domain
            ))
        })?;
        let mut request = session.request;
        request.user_input = Some(user_input);
        self.run_flow_step(request, session.kind, driver)
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

    fn run_flow_step(
        &self,
        request: ConfigFlowRequest,
        kind: FlowKind,
        driver: Arc<dyn ConfigEntryDriver>,
    ) -> Result<Value> {
        let result = match kind {
            FlowKind::Config => driver.config_flow(&request)?,
            FlowKind::Options => driver.options_flow(&request)?,
        };
        match &result {
            ConfigFlowResult::Form { step_id, .. } => {
                let mut stored_request = request.clone();
                stored_request.step_id = step_id.clone();
                stored_request.user_input = None;
                let flow_id = request.flow_id.clone();
                let result_json = flow_result_json(&request, kind, &result, None, None);
                self.mutate(|state| {
                    state.flows.insert(
                        flow_id.clone(),
                        FlowSession {
                            request: stored_request,
                            kind,
                            result: result_json.clone(),
                        },
                    );
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(result_json)
            }
            ConfigFlowResult::Abort { .. } => {
                self.mutate(|state| {
                    state.flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(&request, kind, &result, None, None))
            }
            ConfigFlowResult::CreateEntry { .. } => {
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
        let result = driver.subentry_flow(&request)?;
        match &result {
            ConfigSubentryFlowResult::Form { step_id, .. } => {
                let mut stored_request = request.clone();
                stored_request.step_id = step_id.clone();
                stored_request.user_input = None;
                let flow_id = request.flow_id.clone();
                let result_json = subentry_flow_result_json(&request, &result, None);
                self.mutate(|state| {
                    if state.flows.contains_key(&flow_id)
                        || state.subentry_flows.contains_key(&flow_id)
                    {
                        return Err(Error::InvalidInput("config flow ID collision".into()));
                    }
                    state.subentry_flows.insert(
                        flow_id.clone(),
                        ConfigSubentryFlowSession {
                            request: stored_request,
                            result: result_json.clone(),
                        },
                    );
                    Ok(())
                })?;
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
        if *reload {
            // The pinned flow schedules the parent reload independently of its
            // abort result. This synchronous manager completes that reload and
            // leaves any setup failure visible on the parent entry.
            let _ = self.reload_entry(&request.entry_id);
        }
        Ok(reason
            .clone()
            .unwrap_or_else(|| "reconfigure_successful".into()))
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
                let entry = self.create_entry(seed)?;
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
                json!({
                    "flow_id": flow.request.flow_id,
                    "handler": flow.request.domain,
                    "context": flow.request.context,
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
        let entry = self.require_entry(entry_id)?;
        if entry.state.is_loaded() {
            self.unload_entry_locked(entry_id)?;
        }
        self.update_entry_metadata(entry_id, |current| {
            current.state = ConfigEntryState::NotLoaded;
            current.reason = None;
            current.modified_at = Utc::now();
        })?;
        self.setup_entry_locked(entry_id).map(|_| ())
    }

    fn setup_entry_locked(&self, entry_id: &str) -> Result<ConfigEntry> {
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
                self.update_entry_state(
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
        self.update_entry_state(entry_id, ConfigEntryState::SetupInProgress, None, None)?;
        let entry = self.require_entry(entry_id)?;
        match driver.setup(&self.runtime, &entry) {
            Ok(SetupOutcome::Ready) => {
                self.update_entry_state(entry_id, ConfigEntryState::Loaded, None, None)?;
                self.update_entry_metadata(entry_id, |current| {
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
                    self.update_entry_state(
                        entry_id,
                        ConfigEntryState::SetupError,
                        Some(error.to_string()),
                        None,
                    )?;
                    return Err(error);
                }
                let retry_at = Utc::now() + Duration::seconds(seconds as i64);
                self.update_entry_metadata(entry_id, |current| {
                    current.state = ConfigEntryState::SetupRetry;
                    current.retry_attempts = current.retry_attempts.saturating_add(1);
                    current.retry_at = Some(retry_at);
                    current.reason = reason;
                    current.modified_at = Utc::now();
                })?;
                self.require_entry(entry_id)
            }
            Err(error) => {
                self.update_entry_state(
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
                self.update_entry_state(
                    entry_id,
                    ConfigEntryState::FailedUnload,
                    Some(error.to_string()),
                    None,
                )?;
                return Err(error);
            }
        };
        self.update_entry_state(entry_id, ConfigEntryState::UnloadInProgress, None, None)?;
        let entry = self.require_entry(entry_id)?;
        match driver.unload(&self.runtime, &entry) {
            Ok(()) => self.update_entry_state(entry_id, ConfigEntryState::NotLoaded, None, None),
            Err(error) => {
                self.update_entry_state(
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

    fn persist(&self, state: &PersistedState) -> Result<()> {
        let document = PersistedDocument {
            document_version: DOCUMENT_VERSION,
            entries: state.entries.values().cloned().collect(),
            core_config: state.core_config.clone(),
            core_config_initialized: state.core_config_initialized,
        };
        let value = serde_json::to_value(document).map_err(|error| {
            Error::InvalidInput(format!("cannot encode config metadata: {error}"))
        })?;
        self.runtime.set_data(METADATA_KEY, &value)
    }

    fn emit_entry_change(&self, change: &str, entry: &ConfigEntry) -> Result<()> {
        let entry = self.with_supported_subentry_types(entry.clone());
        let mut data = Map::new();
        data.insert("type".into(), json!(change));
        data.insert("entry".into(), entry.public_json());
        self.runtime
            .fire_event("config_entry_changed", data, Context::default())?;
        Ok(())
    }

    fn notify_lifecycle_observers(&self, event: &ConfigLifecycleEvent) -> Result<()> {
        let observers = self
            .lifecycle_observers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
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
            None => Ok(()),
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
    struct FailingMetadataStore {
        inner: MemoryStore,
        fail_metadata: AtomicBool,
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
                ConfigLifecycleEvent::EntryDisabledByChanged {
                    entry_id: entry.entry_id.clone(),
                    disabled_by: None,
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
        assert_eq!(retained.events.lock().unwrap().len(), 1);
        assert!(manager.unregister_lifecycle_observer(&retained_trait));
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

        store.fail_metadata.store(false, Ordering::SeqCst);
        manager
            .remove_subentry(&entry.entry_id, &child.subentry_id)
            .unwrap();
        manager
            .disable_entry(&entry.entry_id, Some("user"))
            .unwrap();
        manager.remove_entry(&entry.entry_id).unwrap();
        assert_eq!(observer.events.lock().unwrap().len(), 3);
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
}
