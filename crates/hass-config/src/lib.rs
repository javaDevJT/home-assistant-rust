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
            "num_subentries": 0,
        })
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
            })),
            drivers: Arc::new(RwLock::new(HashMap::new())),
            operation_lock: Arc::new(Mutex::new(())),
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

    /// Return a registered integration driver by domain.
    pub fn driver(&self, domain: &str) -> Option<Arc<dyn ConfigEntryDriver>> {
        self.drivers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(domain)
            .cloned()
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
        self.lock_state()
            .flows
            .get(flow_id)
            .map(|flow| flow.result.clone())
            .ok_or_else(|| Error::NotFound("unknown config flow".into()))
    }

    pub fn abort_flow(&self, flow_id: &str) -> Result<()> {
        self.mutate(|state| {
            state
                .flows
                .remove(flow_id)
                .ok_or_else(|| Error::NotFound("unknown config flow".into()))?;
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
        self.lock_state()
            .entries
            .values()
            .filter(|entry| domain.is_none_or(|domain| entry.domain == domain))
            .cloned()
            .collect()
    }

    /// Fetch one trusted entry, including its private data and options.
    pub fn get_entry(&self, entry_id: &str) -> Option<ConfigEntry> {
        self.lock_state().entries.get(entry_id).cloned()
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
        let entry = ConfigEntry {
            entry_id: Uuid::new_v4().to_string(),
            domain: seed.domain,
            title: seed.title.trim().to_owned(),
            source: seed.source,
            unique_id: seed.unique_id,
            version: seed.version,
            minor_version: seed.minor_version,
            data: seed.data,
            options: seed.options,
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
        let _operation = self.lock_operation();
        let entry = self.require_entry(entry_id)?;
        if entry.disabled_by.as_deref() == disabled_by {
            return Ok(true);
        }
        if disabled_by.is_none() && entry.state == ConfigEntryState::FailedUnload {
            self.update_entry_metadata(entry_id, |current| {
                current.disabled_by = None;
                current.require_reload = true;
                current.modified_at = Utc::now();
            })?;
            return Ok(false);
        }
        if disabled_by.is_some() {
            self.update_entry_metadata(entry_id, |current| {
                current.disabled_by = Some("user".into());
                current.modified_at = Utc::now();
            })?;
            if entry.state.is_loaded() && self.unload_entry_locked(entry_id).is_err() {
                return Ok(false);
            }
            return Ok(true);
        }
        self.update_entry_metadata(entry_id, |current| {
            current.disabled_by = None;
            current.modified_at = Utc::now();
        })?;
        match self.setup_entry_locked(entry_id) {
            Ok(_) => Ok(true),
            Err(error) => {
                let current = self.require_entry(entry_id)?;
                if matches!(
                    current.state,
                    ConfigEntryState::SetupError | ConfigEntryState::SetupRetry
                ) {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Remove an entry after running its unload and cleanup callbacks.
    pub fn remove_entry(&self, entry_id: &str) -> Result<()> {
        let _operation = self.lock_operation();
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
            Ok(())
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

    /// Submit user input to an in-progress config/options flow.
    pub fn submit_flow(&self, flow_id: &str, user_input: Map<String, Value>) -> Result<Value> {
        let session = self
            .lock_state()
            .flows
            .get(flow_id)
            .cloned()
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
                self.mutate(|state| {
                    state.flows.insert(
                        flow_id.clone(),
                        FlowSession {
                            request: stored_request,
                            kind,
                            result: flow_result_json(&request, &result, None),
                        },
                    );
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(&request, &result, None))
            }
            ConfigFlowResult::Abort { .. } => {
                self.mutate(|state| {
                    state.flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(&request, &result, None))
            }
            ConfigFlowResult::CreateEntry { .. } => {
                let entry = self.finish_flow(&request, kind, &result)?;
                self.mutate(|state| {
                    state.flows.remove(&request.flow_id);
                    Ok(())
                })?;
                self.notify_flow_progress(&request.flow_id)?;
                Ok(flow_result_json(&request, &result, Some(entry)))
            }
        }
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
        let mut data = Map::new();
        data.insert("type".into(), json!(change));
        data.insert("entry".into(), entry.public_json());
        self.runtime
            .fire_event("config_entry_changed", data, Context::default())?;
        Ok(())
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
    result: &ConfigFlowResult,
    entry: Option<ConfigEntry>,
) -> Value {
    match result {
        ConfigFlowResult::Form {
            step_id,
            data_schema,
            errors,
            description_placeholders,
        } => json!({
            "flow_id": request.flow_id,
            "type": "form",
            "handler": request.domain,
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
            "handler": request.domain,
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
            "handler": request.domain,
            "type": "abort",
            "reason": reason,
        }),
    }
}

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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hass_core::{MemoryStore, Runtime};

    use super::*;

    struct FixtureDriver {
        setup_calls: AtomicUsize,
        unload_calls: AtomicUsize,
        remove_calls: AtomicUsize,
        fail_first_setup: bool,
        fail_unload: bool,
        retry_second_setup: bool,
        version: (u32, u32),
    }

    impl FixtureDriver {
        fn new() -> Self {
            Self {
                setup_calls: AtomicUsize::new(0),
                unload_calls: AtomicUsize::new(0),
                remove_calls: AtomicUsize::new(0),
                fail_first_setup: false,
                fail_unload: false,
                retry_second_setup: false,
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
        let flow_id = started["flow_id"].as_str().unwrap();
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
        let options_flow_id = options_start["flow_id"].as_str().unwrap();
        let mut options = Map::new();
        options.insert("polling".into(), json!(15));
        let options_result = manager.submit_flow(options_flow_id, options).unwrap();
        assert_eq!(options_result["type"], "create_entry");
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
}
