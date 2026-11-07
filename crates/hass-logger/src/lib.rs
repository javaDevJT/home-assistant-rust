//! Native logger integration.
//!
//! The integration applies logger configuration to real `tracing` events. Its
//! [`LoggerFilter`] is intended to be attached to the output layer with
//! `fmt_layer.with_filter(manager.filter())`; it checks the current level on
//! every event and runs configured message regexes before the layer records an
//! event.

mod records;
pub use records::{EventRecord, NormalizedEventFormat, event_record};

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value, json};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::{
    layer::{Context as LayerContext, Filter},
    registry::LookupSpan,
};
use uuid::Uuid;

/// Home Assistant service domain and durable metadata key.
pub const DOMAIN: &str = "logger";
pub const STORAGE_KEY: &str = "core.logger";
pub const EVENT_LOGGING_CHANGED: &str = "logging_changed";

/// A domain name and its concrete Rust tracing target roots.
///
/// The catalog is deliberately explicit: a Python logger name is considered
/// native only when the application supplies the corresponding Rust target.
pub type IntegrationCatalog = BTreeMap<String, Vec<String>>;

/// Authorization hook supplied by the application. The application can use a
/// weak reference to its auth manager; `context.user_id == None` may represent
/// trusted internal service calls.
pub type AdminAuthorizer = Arc<dyn Fn(&Context) -> Result<()> + Send + Sync>;

/// Returns the current native integration catalog (`domain -> tracing targets`).
pub type IntegrationCatalogProvider = Arc<dyn Fn() -> IntegrationCatalog + Send + Sync>;

/// Supported Home Assistant logger severities.
///
/// The string parser matches Home Assistant's service/config vocabulary. Rust
/// Events carrying an explicit `hass.level="CRITICAL"` marker preserve Python's
/// critical severity; ordinary tracing error events retain the error level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoggerLevel {
    NotSet,
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl LoggerLevel {
    /// Parse a Home Assistant logger level name.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "NOTSET" => Ok(Self::NotSet),
            "DEBUG" => Ok(Self::Debug),
            "INFO" => Ok(Self::Info),
            "WARNING" | "WARN" => Ok(Self::Warning),
            "ERROR" => Ok(Self::Error),
            "CRITICAL" | "FATAL" => Ok(Self::Critical),
            _ => Err(Error::InvalidInput(format!(
                "invalid logger level {value:?}; expected NOTSET, DEBUG, INFO, WARNING, WARN, ERROR, CRITICAL, or FATAL"
            ))),
        }
    }

    fn parse_config_or_service(value: &str) -> Result<Self> {
        // Home Assistant applies `Upper` to config and service values. Do not
        // trim: values with surrounding whitespace remain invalid.
        Self::parse(&value.to_uppercase())
    }

    /// Home Assistant's numeric logging value (also returned by logger info).
    pub const fn number(self) -> i64 {
        match self {
            Self::NotSet => 0,
            Self::Debug => 10,
            Self::Info => 20,
            Self::Warning => 30,
            Self::Error => 40,
            Self::Critical => 50,
        }
    }

    /// Early metadata-only admission cannot inspect explicit critical markers.
    const fn native_threshold(self) -> i64 {
        match self {
            Self::Critical => 40,
            _ => self.number(),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSet => "NOTSET",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }
}

impl FromStr for LoggerLevel {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for LoggerLevel {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LoggerLevel {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LevelVisitor;

        impl de::Visitor<'_> for LevelVisitor {
            type Value = LoggerLevel;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "one of NOTSET, DEBUG, INFO, WARNING, WARN, ERROR, CRITICAL, or FATAL",
                )
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                LoggerLevel::parse_config_or_service(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_str(LevelVisitor)
    }
}

/// Strict YAML-compatible logger configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggerConfig {
    /// Default level for targets without a more specific setting. Omission
    /// means Home Assistant's logger default, `INFO`.
    #[serde(default, deserialize_with = "deserialize_present_default_level")]
    pub default: Option<LoggerLevel>,
    /// Exact logger names or dotted logger prefixes and their levels.
    pub logs: BTreeMap<String, LoggerLevel>,
    /// Exact logger names and message patterns to suppress.
    pub filters: BTreeMap<String, Vec<String>>,
}

fn deserialize_present_default_level<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<LoggerLevel>, D::Error>
where
    D: Deserializer<'de>,
{
    // Deserializing `String` intentionally rejects explicit null and numeric
    // YAML values while a missing field still uses `LoggerConfig::default`.
    let value = String::deserialize(deserializer)?;
    LoggerLevel::parse_config_or_service(&value)
        .map(Some)
        .map_err(de::Error::custom)
}

struct CompiledConfig {
    default_level: LoggerLevel,
    configured_default: bool,
    yaml_levels: BTreeMap<String, LoggerLevel>,
    filters: BTreeMap<String, Vec<Regex>>,
}

/// Validate the logger configuration without reading runtime storage or
/// registering services. Use this during config-only validation before
/// opening the runtime database.
pub fn validate_config(config: &LoggerConfig) -> Result<()> {
    compile_config(config).map(|_| ())
}

fn compile_config(config: &LoggerConfig) -> Result<CompiledConfig> {
    let mut yaml_levels = BTreeMap::new();
    for (target, level) in &config.logs {
        yaml_levels.insert(normalize_logger_name(target), *level);
    }

    let mut filters = BTreeMap::new();
    for (target, patterns) in &config.filters {
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let regex = Regex::new(pattern).map_err(|error| {
                Error::InvalidInput(format!(
                    "invalid logger filter for {target:?}: {pattern:?}: {error}"
                ))
            })?;
            compiled.push(regex);
        }
        filters.insert(normalize_logger_name(target), compiled);
    }

    Ok(CompiledConfig {
        default_level: config.default.unwrap_or(LoggerLevel::Info),
        configured_default: config.default.is_some(),
        yaml_levels,
        filters,
    })
}

/// Target kind used by the logger WebSocket commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Module,
    Integration,
}

/// Persistence requested by the logger WebSocket commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persistence {
    /// Apply until this process stops and clear any older saved setting.
    None,
    /// Apply now and once after the next process start, then remove from storage.
    Once,
    /// Apply now and after subsequent process starts until replaced.
    Permanent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSetting {
    level: LoggerLevel,
    persistence: Persistence,
    #[serde(rename = "type")]
    target_kind: TargetKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StoredLoggerConfig {
    logs: BTreeMap<String, StoredSetting>,
}

#[derive(Debug, Clone)]
struct LoggerState {
    default_level: LoggerLevel,
    yaml_levels: BTreeMap<String, LoggerLevel>,
    stored_levels: BTreeMap<String, LoggerLevel>,
    filters: BTreeMap<String, Vec<Regex>>,
    runtime_levels: BTreeMap<String, LoggerLevel>,
    persisted: BTreeMap<String, StoredSetting>,
    active_integration_settings: BTreeSet<String>,
}

impl LoggerState {
    fn level_for(&self, target: &str, catalog: &IntegrationCatalog) -> LoggerLevel {
        let paths = target_paths(target, catalog);
        let mut suppressed_yaml = BTreeSet::new();
        let mut suppressed_stored = BTreeSet::new();
        let mut suppressed_runtime = BTreeSet::new();
        while let Some((matched_name, level, _)) =
            nearest_rule(&self.runtime_levels, &paths, &suppressed_runtime)
        {
            if *level == LoggerLevel::NotSet {
                if matched_name.is_empty() {
                    // The root logger has no parent to inherit from.
                    return LoggerLevel::NotSet;
                }
                // NOTSET resets this logger and inherits the closest parent
                // runtime setting, just as Python logging does.
                for alias in equivalent_rule_names(matched_name, catalog) {
                    suppressed_runtime.insert(alias.clone());
                    suppressed_yaml.insert(alias.clone());
                    suppressed_stored.insert(alias);
                }
                continue;
            }
            return *level;
        }

        if let Some(level) = nearest_config_level(
            &self.yaml_levels,
            &self.stored_levels,
            &paths,
            &suppressed_yaml,
            &suppressed_stored,
        ) {
            return level;
        }
        self.default_level
    }

    fn filters_for<'a>(
        &'a self,
        target: &str,
        catalog: &IntegrationCatalog,
    ) -> Option<&'a [Regex]> {
        let paths = target_paths(target, catalog);
        paths
            .iter()
            .find_map(|path| self.filters.get(path).map(|patterns| patterns.as_slice()))
    }
}

struct ManagerInner {
    runtime: Runtime,
    state: RwLock<LoggerState>,
    update_gate: Mutex<()>,
    active: AtomicBool,
    catalog: IntegrationCatalogProvider,
}

/// Weak handle to the running logger integration.
///
/// This handle does not keep the runtime or integration alive. Service
/// registrations also retain only a weak reference to the manager, so the
/// runtime's service registry cannot form a reference cycle.
#[derive(Clone)]
pub struct LoggerManager {
    inner: Weak<ManagerInner>,
}

impl fmt::Debug for LoggerManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoggerManager")
            .field("running", &self.inner.strong_count())
            .finish()
    }
}

impl LoggerManager {
    fn upgrade(&self) -> Result<Arc<ManagerInner>> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| Error::Service("Logger integration is stopped".into()))?;
        if !inner.active.load(Ordering::Acquire) {
            return Err(Error::Service("Logger integration is stopped".into()));
        }
        Ok(inner)
    }

    /// Return a cloneable dynamic filter for a tracing output layer.
    pub fn filter(&self) -> LoggerFilter {
        LoggerFilter {
            inner: self.inner.clone(),
        }
    }

    /// Change a module or integration logger level and persistence.
    ///
    /// The saved value is written before the active filter state changes. A
    /// settings-write error leaves the previous active level in place; a
    /// later `logging_changed` event-write error is returned after the update
    /// has committed.
    pub fn update_settings(
        &self,
        target: &str,
        target_kind: TargetKind,
        level: LoggerLevel,
        persistence: Persistence,
    ) -> Result<()> {
        let inner = self.upgrade()?;
        let _update = inner
            .update_gate
            .lock()
            .map_err(|_| Error::Storage("logger update lock poisoned".into()))?;
        ensure_active(&inner)?;
        let catalog = (inner.catalog)();
        if target_kind == TargetKind::Integration && !catalog.contains_key(target) {
            return Err(Error::NotFound(format!("Integration {target} not found")));
        }

        let mut next = read_state(&inner)?.clone();
        let rule_names = target_rule_names(target, target_kind, &catalog);
        remove_stored_rule_names(&mut next.stored_levels, target, &catalog);
        for rule_name in rule_names {
            next.runtime_levels.insert(rule_name, level);
        }
        match target_kind {
            TargetKind::Integration if level != LoggerLevel::NotSet => {
                next.active_integration_settings.insert(target.to_owned());
            }
            TargetKind::Integration => {
                next.active_integration_settings.remove(target);
            }
            TargetKind::Module => {
                // LoggerSettings is keyed by target name regardless of type;
                // replacing an integration setting with a module setting
                // removes it from integration_log_info membership.
                next.active_integration_settings.remove(target);
            }
        }

        if level == LoggerLevel::NotSet || persistence == Persistence::None {
            next.persisted.remove(target);
        } else {
            next.persisted.insert(
                target.to_owned(),
                StoredSetting {
                    level,
                    persistence,
                    target_kind,
                },
            );
        }

        persist_settings(&inner.runtime, &next.persisted)?;
        *write_state(&inner)? = next;
        notify_logging_changed(&inner.runtime)
    }

    /// Set the process-wide default level. This operation is session-only. If
    /// persisting its `logging_changed` notification fails, the level change
    /// has already committed.
    pub fn set_default_level(&self, level: LoggerLevel) -> Result<()> {
        let inner = self.upgrade()?;
        set_default_level(&inner, level)
    }

    /// Read effective levels for loaded integrations plus integration domains
    /// with a current logger setting.
    ///
    /// The returned numeric level follows Python logging's values. No
    /// synthetic entries are added for unregistered Python logger names.
    pub fn log_info(&self, catalog: &IntegrationCatalog) -> Result<Vec<IntegrationLogInfo>> {
        let inner = self.upgrade()?;
        let state = read_state(&inner)?;
        let mut full_catalog = (inner.catalog)();
        for (domain, targets) in catalog {
            let merged = full_catalog.entry(domain.clone()).or_default();
            for target in targets {
                if !merged.contains(target) {
                    merged.push(target.clone());
                }
            }
        }
        let mut domains = catalog.keys().cloned().collect::<BTreeSet<_>>();
        domains.extend(state.active_integration_settings.iter().cloned());
        let mut entries = Vec::with_capacity(domains.len());
        for domain in domains {
            let targets = catalog.get(&domain).or_else(|| full_catalog.get(&domain));
            let target = targets
                .and_then(|targets| targets.iter().find(|target| !target.is_empty()))
                .map(String::as_str)
                .unwrap_or_else(|| "");
            let logger_name = if target.is_empty() {
                format!("homeassistant.components.{domain}")
            } else {
                target.to_owned()
            };
            entries.push(IntegrationLogInfo {
                domain,
                level: state.level_for(&logger_name, &full_catalog).number(),
            });
        }
        Ok(entries)
    }

    /// Return the effective Home Assistant numeric level for a logger target.
    pub fn effective_level(&self, target: &str) -> Result<LoggerLevel> {
        let inner = self.upgrade()?;
        let catalog = (inner.catalog)();
        Ok(read_state(&inner)?.level_for(target, &catalog))
    }
}

/// One item in the logger WebSocket `logger/log_info` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationLogInfo {
    pub domain: String,
    pub level: i64,
}

/// Dynamic per-layer logger filter.
///
/// `callsite_enabled` deliberately remains `Interest::sometimes()` (the
/// trait default), and `enabled` reads the current state on every event. A
/// runtime level change therefore takes effect even at an already-seen
/// callsite.
#[derive(Clone)]
pub struct LoggerFilter {
    inner: Weak<ManagerInner>,
}

impl fmt::Debug for LoggerFilter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoggerFilter")
            .field("running", &self.inner.strong_count())
            .finish()
    }
}

impl<S> Filter<S> for LoggerFilter
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn callsite_enabled(&self, _metadata: &Metadata<'_>) -> tracing::subscriber::Interest {
        // Settings change live; neither a disabled span nor an admitted event
        // can be cached permanently from its first use.
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>, _context: &LayerContext<'_, S>) -> bool {
        self.inner
            .upgrade()
            .is_some_and(|inner| inner.event_level_enabled(metadata))
    }

    fn event_enabled(&self, event: &Event<'_>, _context: &LayerContext<'_, S>) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        inner.event_enabled(event)
    }
}

impl ManagerInner {
    fn event_level_enabled(&self, metadata: &Metadata<'_>) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        // This is context for suppressing recursive raw delivery, not an
        // emitted record. Keep it even when only a protocol dependency has
        // verbose logging enabled and the API logger itself is at INFO.
        if metadata.is_span() && metadata.name() == "hass_raw_stream_delivery" {
            return true;
        }
        // Dynamic target and severity fields are only available on the event.
        if metadata.target() == "log" || metadata.fields().field("hass.target").is_some() {
            return true;
        }
        let catalog = (self.catalog)();
        let Ok(state) = self.state.read() else {
            return false;
        };
        let effective = state
            .level_for(metadata.target(), &catalog)
            .native_threshold();
        tracing_level_number(metadata.level()) >= effective
    }

    fn event_enabled(&self, event: &Event<'_>) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let record = event_record(event);
        let catalog = (self.catalog)();
        let Ok(state) = self.state.read() else {
            return false;
        };
        if record.severity_number < state.level_for(&record.target, &catalog).number() {
            return false;
        }
        let Some(patterns) = state.filters_for(&record.target, &catalog) else {
            return true;
        };
        if patterns.is_empty() {
            return true;
        }
        !patterns
            .iter()
            .any(|pattern| pattern.is_match(&record.message))
    }
}

/// Owns the logger services and keeps the runtime alive while they are active.
pub struct LoggerIntegration {
    runtime: Runtime,
    owner: String,
    inner: Arc<ManagerInner>,
}

impl fmt::Debug for LoggerIntegration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoggerIntegration")
            .field("owner", &self.owner)
            .field("running", &self.inner.active.load(Ordering::Acquire))
            .finish()
    }
}

impl LoggerIntegration {
    /// Validate config and stored settings, apply saved overrides, and
    /// register the real logger services.
    pub fn start(
        runtime: Runtime,
        config: LoggerConfig,
        catalog: IntegrationCatalogProvider,
        authorize_admin: AdminAuthorizer,
    ) -> Result<Self> {
        let compiled = compile_config(&config)?;
        let initial_catalog = catalog();

        let old_value = runtime.get_data(STORAGE_KEY)?;
        let stored = if let Some(value) = old_value.as_ref() {
            serde_json::from_value::<StoredLoggerConfig>(value.clone()).map_err(|error| {
                Error::Storage(format!("invalid stored logger settings: {error}"))
            })?
        } else {
            StoredLoggerConfig::default()
        };

        let runtime_levels = BTreeMap::new();
        let mut stored_levels = BTreeMap::new();
        let mut persisted = BTreeMap::new();
        let mut active_integration_settings = BTreeSet::new();
        let mut needs_cleanup = false;
        for (target, setting) in &stored.logs {
            if setting.level == LoggerLevel::NotSet {
                needs_cleanup = true;
                continue;
            }
            if setting.target_kind == TargetKind::Integration {
                active_integration_settings.insert(target.clone());
            }

            for rule_name in target_rule_names(target, setting.target_kind, &initial_catalog) {
                merge_chattiest(&mut stored_levels, rule_name, setting.level);
            }
            if setting.persistence != Persistence::Permanent {
                // It is active for this process, but removed durably before
                // the integration becomes available for the next process.
                needs_cleanup = true;
            } else {
                persisted.insert(target.clone(), setting.clone());
            }
        }

        let inner = Arc::new(ManagerInner {
            runtime: runtime.clone(),
            state: RwLock::new(LoggerState {
                default_level: compiled.default_level,
                yaml_levels: compiled.yaml_levels,
                stored_levels,
                filters: compiled.filters,
                runtime_levels,
                persisted,
                active_integration_settings,
            }),
            update_gate: Mutex::new(()),
            active: AtomicBool::new(true),
            catalog: catalog.clone(),
        });
        let owner = format!("logger.{}", Uuid::new_v4().simple());

        if needs_cleanup {
            let state = read_state(&inner)?.clone();
            if let Err(error) = persist_settings(&runtime, &state.persisted) {
                inner.active.store(false, Ordering::Release);
                return Err(error);
            }
        }

        // Startup calls the same source helpers as the Home Assistant
        // integration: an explicit default emits once, then combined YAML and
        // saved module levels emit once even when that batch is empty.
        let notification = (|| {
            if compiled.configured_default {
                notify_logging_changed(&runtime)?;
            }
            notify_logging_changed(&runtime)
        })();
        if let Err(error) = notification {
            inner.active.store(false, Ordering::Release);
            if needs_cleanup {
                let restore = match old_value.as_ref() {
                    Some(value) => runtime.set_data(STORAGE_KEY, value),
                    None => runtime.delete_data(STORAGE_KEY).map(|_| ()),
                };
                if let Err(restore_error) = restore {
                    return Err(Error::Storage(format!(
                        "logger startup failed ({error}); restoring previous logger settings failed ({restore_error})"
                    )));
                }
            }
            return Err(error);
        }

        if let Err(error) =
            register_services(&runtime, &owner, Arc::downgrade(&inner), authorize_admin)
        {
            let _ = runtime.unregister_owned_services(&owner);
            inner.active.store(false, Ordering::Release);
            if needs_cleanup {
                if let Some(old_value) = old_value {
                    if let Err(restore_error) = runtime.set_data(STORAGE_KEY, &old_value) {
                        return Err(Error::Storage(format!(
                            "cannot register logger services ({error}); cannot restore previous logger settings ({restore_error})"
                        )));
                    }
                } else if let Err(restore_error) = runtime.delete_data(STORAGE_KEY) {
                    return Err(Error::Storage(format!(
                        "cannot register logger services ({error}); cannot restore empty logger settings ({restore_error})"
                    )));
                }
            }
            return Err(error);
        }

        Ok(Self {
            runtime,
            owner,
            inner,
        })
    }

    /// Get a weak handle for API adapters and the tracing filter.
    pub fn manager(&self) -> LoggerManager {
        LoggerManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Get a cloneable filter suitable for a tracing output layer.
    pub fn filter(&self) -> LoggerFilter {
        self.manager().filter()
    }

    /// Unregister services and stop filters. Dropping the value performs the
    /// same cleanup.
    pub fn shutdown(self) -> Result<()> {
        self.stop()
    }

    fn stop(&self) -> Result<()> {
        if self.inner.active.swap(false, Ordering::AcqRel) {
            self.runtime.unregister_owned_services(&self.owner)?;
        }
        Ok(())
    }
}

impl Drop for LoggerIntegration {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn register_services(
    runtime: &Runtime,
    owner: &str,
    manager: Weak<ManagerInner>,
    authorize_admin: AdminAuthorizer,
) -> Result<()> {
    let set_level_manager = manager.clone();
    let set_level_authorizer = authorize_admin.clone();
    runtime.register_owned_service(
        owner,
        DOMAIN,
        "set_level",
        json!({
            "description": "Set one or more logger levels",
            "fields": {"<logger>": {"description": "Logger level name"}}
        }),
        Arc::new(move |_runtime, call| {
            set_level_authorizer(&call.context)?;
            if !call.target.is_empty() {
                return Err(Error::InvalidInput(
                    "logger.set_level does not accept a target".into(),
                ));
            }
            let levels = call
                .data
                .iter()
                .map(|(target, value)| {
                    let level = value.as_str().ok_or_else(|| {
                        Error::InvalidInput(format!("logger level for {target:?} must be a string"))
                    })?;
                    Ok((target.clone(), LoggerLevel::parse_config_or_service(level)?))
                })
                .collect::<Result<Vec<_>>>()?;
            let inner = upgrade_handler(&set_level_manager)?;
            apply_service_levels(&inner, levels)?;
            Ok(empty_result(call))
        }),
    )?;

    let default_manager = manager;
    runtime.register_owned_service(
        owner,
        DOMAIN,
        "set_default_level",
        json!({
            "description": "Set the default logger level",
            "fields": {"level": {"required": true}}
        }),
        Arc::new(move |_runtime, call| {
            authorize_admin(&call.context)?;
            if !call.target.is_empty() || call.data.len() != 1 {
                return Err(Error::InvalidInput(
                    "logger.set_default_level requires only the level field".into(),
                ));
            }
            let value = call
                .data
                .get("level")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::InvalidInput("logger.set_default_level level must be a string".into())
                })?;
            let level = LoggerLevel::parse_config_or_service(value)?;
            let inner = upgrade_handler(&default_manager)?;
            set_default_level(&inner, level)?;
            Ok(empty_result(call))
        }),
    )?;
    Ok(())
}

fn empty_result(call: &ServiceCall) -> ServiceResult {
    ServiceResult {
        changed_states: Vec::new(),
        response: None,
        context: call.context.clone(),
    }
}

fn upgrade_handler(manager: &Weak<ManagerInner>) -> Result<Arc<ManagerInner>> {
    let inner = manager
        .upgrade()
        .ok_or_else(|| Error::Service("Logger integration is stopped".into()))?;
    ensure_active(&inner)?;
    Ok(inner)
}

fn apply_service_levels(inner: &ManagerInner, levels: Vec<(String, LoggerLevel)>) -> Result<()> {
    let _update = inner
        .update_gate
        .lock()
        .map_err(|_| Error::Storage("logger update lock poisoned".into()))?;
    ensure_active(inner)?;
    let catalog = (inner.catalog)();
    let mut next = read_state(inner)?.clone();
    for (target, level) in levels {
        let rule_name = target_rule_names(&target, TargetKind::Module, &catalog);
        for name in rule_name {
            next.runtime_levels.insert(name, level);
        }
    }
    *write_state(inner)? = next;
    notify_logging_changed(&inner.runtime)
}

fn set_default_level(inner: &ManagerInner, level: LoggerLevel) -> Result<()> {
    let _update = inner
        .update_gate
        .lock()
        .map_err(|_| Error::Storage("logger update lock poisoned".into()))?;
    ensure_active(inner)?;
    {
        let mut state = write_state(inner)?;
        state.default_level = level;
        // This helper sets Python's root logger directly and wins over any
        // prior YAML, stored, or module override attached to the root name.
        state.yaml_levels.remove("");
        state.stored_levels.remove("");
        state.runtime_levels.remove("");
    }
    notify_logging_changed(&inner.runtime)
}

fn notify_logging_changed(runtime: &Runtime) -> Result<()> {
    runtime
        .fire_event(EVENT_LOGGING_CHANGED, Map::new(), Context::default())
        .map(|_| ())
}

fn read_state(inner: &ManagerInner) -> Result<std::sync::RwLockReadGuard<'_, LoggerState>> {
    inner
        .state
        .read()
        .map_err(|_| Error::Storage("logger state lock poisoned".into()))
}

fn write_state(inner: &ManagerInner) -> Result<std::sync::RwLockWriteGuard<'_, LoggerState>> {
    inner
        .state
        .write()
        .map_err(|_| Error::Storage("logger state lock poisoned".into()))
}

fn ensure_active(inner: &ManagerInner) -> Result<()> {
    if !inner.active.load(Ordering::Acquire) {
        return Err(Error::Service("Logger integration is stopped".into()));
    }
    Ok(())
}

fn persist_settings(runtime: &Runtime, settings: &BTreeMap<String, StoredSetting>) -> Result<()> {
    if settings.is_empty() {
        runtime.delete_data(STORAGE_KEY)?;
        return Ok(());
    }
    let value = serde_json::to_value(StoredLoggerConfig {
        logs: settings.clone(),
    })
    .map_err(|error| Error::Storage(format!("serialize logger settings: {error}")))?;
    runtime.set_data(STORAGE_KEY, &value)
}

fn target_rule_names(target: &str, kind: TargetKind, catalog: &IntegrationCatalog) -> Vec<String> {
    let names = match kind {
        TargetKind::Integration => {
            let mut names = vec![format!("homeassistant.components.{target}")];
            if let Some(targets) = catalog.get(target) {
                names.extend(
                    targets
                        .iter()
                        .filter(|name| !name.is_empty())
                        .map(|name| canonical_logger_name(name, catalog)),
                );
            }
            names
        }
        TargetKind::Module => vec![canonical_logger_name(target, catalog)],
    };
    names
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_logger_name(target: &str) -> String {
    target.replace("::", ".")
}

fn canonical_logger_name(target: &str, catalog: &IntegrationCatalog) -> String {
    let target = normalize_logger_name(target);
    for (domain, prefix) in catalog_prefixes(catalog) {
        if target == prefix {
            return format!("homeassistant.components.{domain}");
        }
        if let Some(suffix) = target.strip_prefix(&prefix)
            && suffix.starts_with('.')
        {
            return format!("homeassistant.components.{domain}{suffix}");
        }
    }
    target
}

fn catalog_prefixes(catalog: &IntegrationCatalog) -> Vec<(String, String)> {
    let mut prefixes = catalog
        .iter()
        .flat_map(|(domain, targets)| {
            targets
                .iter()
                .filter(|target| !target.is_empty())
                .map(|target| (domain.clone(), normalize_logger_name(target)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    prefixes.sort_by_key(|pair| std::cmp::Reverse(pair.1.len()));
    prefixes
}

fn target_paths(target: &str, catalog: &IntegrationCatalog) -> Vec<String> {
    let raw = normalize_logger_name(target);
    let canonical = canonical_logger_name(&raw, catalog);
    if canonical == raw {
        vec![raw]
    } else {
        vec![canonical, raw]
    }
}

fn ancestors(path: &str) -> Vec<String> {
    if path.is_empty() {
        return vec![String::new()];
    }
    let mut components = path.split('.').collect::<Vec<_>>();
    while components
        .last()
        .is_some_and(|component| component.is_empty())
    {
        components.pop();
    }
    let mut names = Vec::with_capacity(components.len());
    while !components.is_empty() {
        names.push(components.join("."));
        components.pop();
    }
    names.push(String::new());
    names
}

fn nearest_rule<'a>(
    rules: &'a BTreeMap<String, LoggerLevel>,
    paths: &[String],
    excluded: &BTreeSet<String>,
) -> Option<(&'a str, &'a LoggerLevel, usize)> {
    let mut nearest: Option<(&str, &LoggerLevel, usize, usize)> = None;
    for (path_priority, path) in paths.iter().enumerate() {
        for (distance, name) in ancestors(path).into_iter().enumerate() {
            if excluded.contains(&name) {
                continue;
            }
            let Some((matched_name, level)) = rules.get_key_value(&name) else {
                continue;
            };
            let candidate = (matched_name.as_str(), level, distance, path_priority);
            if nearest.is_none_or(|current| {
                candidate.2 < current.2 || (candidate.2 == current.2 && candidate.3 < current.3)
            }) {
                nearest = Some(candidate);
            }
        }
    }
    nearest.map(|(name, level, distance, _)| (name, level, distance))
}

fn nearest_config_level(
    yaml: &BTreeMap<String, LoggerLevel>,
    stored: &BTreeMap<String, LoggerLevel>,
    paths: &[String],
    excluded_yaml: &BTreeSet<String>,
    excluded_stored: &BTreeSet<String>,
) -> Option<LoggerLevel> {
    let mut nearest: Option<(usize, LoggerLevel)> = None;
    for path in paths {
        for (distance, name) in ancestors(path).into_iter().enumerate() {
            for (rules, excluded) in [(yaml, excluded_yaml), (stored, excluded_stored)] {
                if excluded.contains(&name) {
                    continue;
                }
                let Some(level) = rules.get(&name) else {
                    continue;
                };
                if *level == LoggerLevel::NotSet && !name.is_empty() {
                    continue;
                }
                match nearest {
                    None => nearest = Some((distance, *level)),
                    Some((current_distance, _)) if distance < current_distance => {
                        nearest = Some((distance, *level));
                    }
                    Some((current_distance, current_level))
                        if distance == current_distance
                            && *level != LoggerLevel::NotSet
                            && (current_level == LoggerLevel::NotSet
                                || level.number() < current_level.number()) =>
                    {
                        // YAML and saved logger settings are merged using the
                        // chattiest (numerically lower) level at one logger.
                        nearest = Some((distance, *level));
                    }
                    _ => {}
                }
            }
        }
    }
    nearest.map(|(_, level)| level)
}

fn merge_chattiest(levels: &mut BTreeMap<String, LoggerLevel>, name: String, level: LoggerLevel) {
    if level == LoggerLevel::NotSet {
        return;
    }
    levels
        .entry(name)
        .and_modify(|current| {
            if level.number() < current.number() {
                *current = level;
            }
        })
        .or_insert(level);
}

fn remove_stored_rule_names(
    stored: &mut BTreeMap<String, LoggerLevel>,
    target: &str,
    catalog: &IntegrationCatalog,
) {
    for kind in [TargetKind::Module, TargetKind::Integration] {
        for name in target_rule_names(target, kind, catalog) {
            stored.remove(&name);
        }
    }
}

fn equivalent_rule_names(target: &str, catalog: &IntegrationCatalog) -> BTreeSet<String> {
    let mut names = target_paths(target, catalog)
        .into_iter()
        .collect::<BTreeSet<_>>();
    const COMPONENT_PREFIX: &str = "homeassistant.components.";
    if let Some(component) = target.strip_prefix(COMPONENT_PREFIX) {
        let mut parts = component.splitn(2, '.');
        let domain = parts.next().unwrap_or_default();
        let suffix = parts.next().map(|suffix| format!(".{suffix}"));
        if let Some(targets) = catalog.get(domain) {
            names.extend(targets.iter().filter(|name| !name.is_empty()).map(|name| {
                let mut alias = normalize_logger_name(name);
                if let Some(suffix) = &suffix {
                    alias.push_str(suffix);
                }
                alias
            }));
        }
    }
    names
}

fn tracing_level_number(level: &Level) -> i64 {
    match *level {
        Level::TRACE => 5,
        Level::DEBUG => 10,
        Level::INFO => 20,
        Level::WARN => 30,
        Level::ERROR => 40,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{Event, MemoryStore, Store};
    use serde_json::Map;
    use std::{
        io,
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::{Layer, fmt as tracing_fmt, layer::SubscriberExt};

    fn provider(catalog: IntegrationCatalog) -> IntegrationCatalogProvider {
        Arc::new(move || catalog.clone())
    }

    fn allowed() -> AdminAuthorizer {
        Arc::new(|_| Ok(()))
    }

    fn runtime(store: Arc<MemoryStore>) -> Runtime {
        Runtime::new(store).unwrap()
    }

    #[derive(Default)]
    struct FailingEventStore {
        inner: MemoryStore,
        fail_at: std::sync::atomic::AtomicUsize,
    }

    impl Store for FailingEventStore {
        fn load_states(&self) -> Result<Vec<hass_core::State>> {
            self.inner.load_states()
        }
        fn put_state(&self, state: &hass_core::State, event: Option<&Event>) -> Result<()> {
            self.inner.put_state(state, event)
        }
        fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
            self.inner.remove_state(entity_id, event)
        }
        fn append_event(&self, event: &Event) -> Result<()> {
            if self
                .fail_at
                .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_sub(1)
                })
                == Ok(1)
            {
                return Err(Error::Storage("injected startup event failure".into()));
            }
            self.inner.append_event(event)
        }
        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }
        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            self.inner.set_data(key, value)
        }
        fn delete_data(&self, key: &str) -> Result<bool> {
            self.inner.delete_data(key)
        }
        fn history(
            &self,
            id: &str,
            start: chrono::DateTime<chrono::Utc>,
            end: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<hass_core::State>> {
            self.inner.history(id, start, end)
        }
        fn history_query(
            &self,
            query: &hass_core::HistoryQuery,
        ) -> Result<Vec<hass_core::HistorySeries>> {
            self.inner.history_query(query)
        }
        fn event_history_query(&self, query: &hass_core::EventHistoryQuery) -> Result<Vec<Event>> {
            self.inner.event_history_query(query)
        }
        fn purge_before(
            &self,
            cutoff: chrono::DateTime<chrono::Utc>,
        ) -> Result<hass_core::PurgeResult> {
            self.inner.purge_before(cutoff)
        }
        fn repack(&self) -> Result<()> {
            self.inner.repack()
        }
    }

    #[test]
    fn failed_startup_notification_does_not_consume_once_settings() {
        for failed_notification in [1, 2] {
            let store = Arc::new(FailingEventStore::default());
            let runtime = Runtime::new(store.clone()).unwrap();
            let catalog = provider(IntegrationCatalog::from([(
                "mqtt".into(),
                vec!["hass_mqtt".into()],
            )]));
            let original = LoggerIntegration::start(
                runtime.clone(),
                LoggerConfig::default(),
                catalog.clone(),
                allowed(),
            )
            .unwrap();
            original
                .manager()
                .update_settings(
                    "mqtt",
                    TargetKind::Integration,
                    LoggerLevel::Debug,
                    Persistence::Once,
                )
                .unwrap();
            original.shutdown().unwrap();
            let saved = runtime.get_data(STORAGE_KEY).unwrap();
            store.fail_at.store(failed_notification, Ordering::Release);
            assert!(
                LoggerIntegration::start(
                    runtime.clone(),
                    LoggerConfig {
                        default: Some(LoggerLevel::Info),
                        ..LoggerConfig::default()
                    },
                    catalog.clone(),
                    allowed()
                )
                .is_err()
            );
            assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), saved);
            assert!(runtime.services().get(DOMAIN).is_none());
            let recovered = LoggerIntegration::start(
                runtime.clone(),
                LoggerConfig::default(),
                catalog.clone(),
                allowed(),
            )
            .unwrap();
            assert_eq!(
                recovered.manager().effective_level("hass_mqtt").unwrap(),
                LoggerLevel::Debug
            );
            assert!(runtime.get_data(STORAGE_KEY).unwrap().is_none());
            recovered.shutdown().unwrap();
            let next =
                LoggerIntegration::start(runtime, LoggerConfig::default(), catalog, allowed())
                    .unwrap();
            assert_eq!(
                next.manager().effective_level("hass_mqtt").unwrap(),
                LoggerLevel::Info
            );
            next.shutdown().unwrap();
        }
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    struct SharedWriterGuard(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedWriterGuard {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("capture writer lock poisoned"))?
                .write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriterGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriterGuard(self.0.clone())
        }
    }

    fn captured(writer: &SharedWriter) -> String {
        String::from_utf8(writer.0.lock().unwrap().clone()).unwrap()
    }

    fn service_call(domain: &str, service: &str, data: Value) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: data.as_object().unwrap().clone(),
            target: Map::new(),
            context: Context::default(),
        }
    }

    fn assert_logging_changed(event: Event) {
        assert_eq!(event.event_type, EVENT_LOGGING_CHANGED);
        assert!(event.data.is_empty());
        assert_eq!(event.origin, "LOCAL");
        assert!(event.context.user_id.is_none());
        assert!(event.context.parent_id.is_none());
    }

    #[test]
    fn strict_config_accepts_aliases_and_rejects_unknown_fields_or_bad_regex() {
        let config: LoggerConfig = serde_yaml::from_str(
            "default: wArN\nlogs:\n  hass_demo: fAtAl\nfilters:\n  hass_demo:\n    - 'secret-[0-9]+'\n",
        )
        .unwrap();
        assert_eq!(config.default, Some(LoggerLevel::Warning));
        assert_eq!(config.logs["hass_demo"], LoggerLevel::Critical);

        assert!(serde_yaml::from_str::<LoggerConfig>("unknown: true\n").is_err());
        assert!(serde_yaml::from_str::<LoggerConfig>("default: null\n").is_err());
        assert!(serde_yaml::from_str::<LoggerConfig>("default: 20\n").is_err());
        assert!(serde_yaml::from_str::<LoggerConfig>("default: ' INFO '\n").is_err());
        assert!(LoggerLevel::parse("debug").is_err());
        assert!(
            validate_config(&LoggerConfig {
                filters: BTreeMap::from([("demo".into(), vec!["[".into()])]),
                ..LoggerConfig::default()
            })
            .is_err()
        );
        let root_config: LoggerConfig = serde_yaml::from_str("logs:\n  '': DEBUG\n").unwrap();
        assert_eq!(root_config.logs[""], LoggerLevel::Debug);

        let store = Arc::new(MemoryStore::default());
        let result = LoggerIntegration::start(
            runtime(store.clone()),
            LoggerConfig {
                filters: BTreeMap::from([("hass_demo".into(), vec!["[".into()])]),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        );
        assert!(matches!(result, Err(Error::InvalidInput(_))));
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);
    }

    #[test]
    fn raw_transport_scope_survives_independent_protocol_logger_levels() {
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig {
                logs: BTreeMap::from([("wire.fixture".into(), LoggerLevel::Debug)]),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let writer = SharedWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_fmt::layer()
                .event_format(NormalizedEventFormat)
                .with_ansi(false)
                .with_writer(writer.clone())
                .with_filter(integration.filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let ordinary = tracing::debug_span!("ordinary_debug_scope");
            {
                let _entered = ordinary.enter();
                tracing::debug!(target: "wire.fixture", "ordinary scope fixture");
            }
            let delivery = tracing::debug_span!("hass_raw_stream_delivery");
            let _entered = delivery.enter();
            tracing::debug!(target: "wire.fixture", "visible protocol fixture");
            tracing::debug!(target: "ordinary.fixture", "hidden ordinary fixture");
        });
        let output = captured(&writer);
        assert!(output.contains("ordinary scope fixture"), "{output}");
        assert!(!output.contains("ordinary_debug_scope"), "{output}");
        assert!(
            output.contains("hass_raw_stream_delivery: visible protocol fixture"),
            "{output}"
        );
        assert!(!output.contains("hidden ordinary fixture"), "{output}");
    }

    #[test]
    fn formatter_preserves_original_span_and_event_fields() {
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig {
                default: Some(LoggerLevel::NotSet),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let writer = SharedWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_fmt::layer()
                .event_format(NormalizedEventFormat)
                .with_ansi(false)
                .with_writer(writer.clone())
                .with_filter(integration.filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", correlation = 7);
            let _entered = span.enter();
            tracing::info!(answer = 42, "original message");
            tracing::trace!("native trace message");
        });
        let output = captured(&writer);
        assert!(
            output.contains("request{correlation=7}: original message answer=42"),
            "{output}"
        );
        assert!(
            output.contains("TRACE [") && output.contains("native trace message"),
            "{output}"
        );
    }

    #[test]
    fn bridged_dynamic_target_uses_its_level_and_message_filter() {
        use log::Log;
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig {
                logs: BTreeMap::from([
                    ("log".into(), LoggerLevel::Critical),
                    ("bridge.dynamic".into(), LoggerLevel::Debug),
                ]),
                filters: BTreeMap::from([(
                    "bridge.dynamic".into(),
                    vec!["^private fixture$".into()],
                )]),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let writer = SharedWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_fmt::layer()
                .event_format(NormalizedEventFormat)
                .with_ansi(false)
                .with_writer(writer.clone())
                .with_filter(integration.filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let tracer = tracing_log::LogTracer::new();
            let target = ["bridge", "dynamic"].join(".");
            for message in ["visible bridged fixture", "private fixture"] {
                tracer.log(
                    &log::Record::builder()
                        .args(format_args!("{message}"))
                        .target(&target)
                        .level(log::Level::Debug)
                        .file(Some("bridge-source.rs"))
                        .line(Some(42))
                        .build(),
                );
            }
        });
        let output = captured(&writer);
        assert!(
            output.contains("DEBUG [bridge.dynamic] visible bridged fixture"),
            "{output}"
        );
        assert!(!output.contains("private fixture"), "{output}");
        assert!(!output.contains("log.target"), "{output}");
    }

    #[test]
    fn marked_critical_retains_severity_and_dynamic_target_in_real_output() {
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig {
                default: Some(LoggerLevel::Critical),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let writer = SharedWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_fmt::layer()
                .event_format(NormalizedEventFormat)
                .with_ansi(false)
                .with_writer(writer.clone())
                .with_filter(integration.filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let target = ["dynamic", "critical"].join(".");
            tracing::error!(target: "hass_system_log::external", { hass.target = target.as_str(), hass.level = "CRITICAL", answer = 42 }, "critical fixture");
            tracing::error!(target: "hass_system_log::external", { hass.target = target.as_str() }, "ordinary error fixture");
            tracing::info!(target: "hass_system_log::external", { hass.target = target.as_str(), hass.level = "CRITICAL" }, "unpromoted info fixture");
        });
        let output = captured(&writer);
        assert!(
            output.contains("CRITICAL [dynamic.critical] critical fixture answer=42"),
            "{output}"
        );
        assert!(!output.contains("ordinary error fixture"), "{output}");
        assert!(!output.contains("unpromoted info fixture"), "{output}");
        assert!(!output.contains("hass.target"), "{output}");
    }

    #[test]
    fn actual_fmt_layer_applies_levels_regexes_aliases_and_live_updates() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime(store);
        let catalog = IntegrationCatalog::from([("demo".into(), vec!["hass_demo".into()])]);
        let integration = LoggerIntegration::start(
            runtime,
            LoggerConfig {
                filters: BTreeMap::from([(
                    "homeassistant.components.demo".into(),
                    vec!["^secret-42 filtered at exact logger$".into()],
                )]),
                ..LoggerConfig::default()
            },
            provider(catalog.clone()),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();
        let writer = SharedWriter::default();
        let layer = tracing_fmt::layer()
            .with_ansi(false)
            .with_writer(writer.clone())
            .with_filter(integration.filter());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "hass_demo", "debug hidden at INFO");
            tracing::info!(target: "hass_demo", "secret-42 filtered at exact logger");
            tracing::info!(target: "hass_demo::child", "secret-43 survives child logger");
            manager
                .update_settings(
                    "demo",
                    TargetKind::Integration,
                    LoggerLevel::Debug,
                    Persistence::None,
                )
                .unwrap();
            tracing::debug!(target: "hass_demo", "debug visible after live update");
        });

        let output = captured(&writer);
        assert!(!output.contains("debug hidden at INFO"), "{output}");
        assert!(!output.contains("secret-42 filtered"), "{output}");
        assert!(
            output.contains("secret-43 survives child logger"),
            "{output}"
        );
        assert!(
            output.contains("debug visible after live update"),
            "{output}"
        );
        assert_eq!(
            manager.effective_level("hass_demo::child").unwrap(),
            LoggerLevel::Debug
        );
        assert_eq!(
            manager.log_info(&catalog).unwrap(),
            vec![IntegrationLogInfo {
                domain: "demo".into(),
                level: 10,
            }]
        );
    }

    #[test]
    fn empty_root_logger_name_controls_real_events_and_inherits() {
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig {
                logs: BTreeMap::from([("".into(), LoggerLevel::Debug)]),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();
        assert_eq!(
            manager.effective_level("some.module").unwrap(),
            LoggerLevel::Debug
        );

        let writer = SharedWriter::default();
        let layer = tracing_fmt::layer()
            .with_ansi(false)
            .with_writer(writer.clone())
            .with_filter(integration.filter());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "", "root debug before override");
            manager.set_default_level(LoggerLevel::Info).unwrap();
            tracing::debug!(target: "some.module", "root debug after override hidden");
            tracing::info!(target: "some.module", "root info after override visible");
        });

        let output = captured(&writer);
        assert!(output.contains("root debug before override"), "{output}");
        assert!(
            !output.contains("root debug after override hidden"),
            "{output}"
        );
        assert!(
            output.contains("root info after override visible"),
            "{output}"
        );
        assert_eq!(
            manager.effective_level("some.module").unwrap(),
            LoggerLevel::Info
        );
    }

    #[test]
    fn set_level_service_validates_whole_batch_and_requires_admin() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime(store.clone());
        let integration = LoggerIntegration::start(
            runtime.clone(),
            LoggerConfig::default(),
            provider(IntegrationCatalog::new()),
            Arc::new(|context| {
                if context.user_id.as_deref() == Some("admin") {
                    Ok(())
                } else {
                    Err(Error::NotAllowed(
                        "Administrator privileges are required".into(),
                    ))
                }
            }),
        )
        .unwrap();

        let mut bad_batch = service_call(
            "logger",
            "set_level",
            json!({"one": "DEBUG", "two": "nope"}),
        );
        bad_batch.context.user_id = Some("admin".into());
        assert!(matches!(
            runtime.call_service(bad_batch),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(
            integration.manager().effective_level("one").unwrap(),
            LoggerLevel::Info
        );
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);

        assert!(matches!(
            runtime.call_service(service_call(
                "logger",
                "set_default_level",
                json!({"level":"DEBUG"})
            )),
            Err(Error::NotAllowed(_))
        ));

        let mut good_batch = service_call(
            "logger",
            "set_level",
            json!({"one": "dEbUg", "two": "wArN"}),
        );
        good_batch.context.user_id = Some("admin".into());
        runtime.call_service(good_batch).unwrap();
        assert_eq!(
            integration.manager().effective_level("one").unwrap(),
            LoggerLevel::Debug
        );
        assert_eq!(
            integration.manager().effective_level("two").unwrap(),
            LoggerLevel::Warning
        );
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);

        let mut default_call =
            service_call("logger", "set_default_level", json!({"level":"eRrOr"}));
        default_call.context.user_id = Some("admin".into());
        runtime.call_service(default_call).unwrap();
        assert_eq!(
            integration
                .manager()
                .effective_level("unconfigured.module")
                .unwrap(),
            LoggerLevel::Error
        );
    }

    #[test]
    fn every_level_update_emits_public_event_after_commit() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let mut events = runtime.subscribe();
        let integration = LoggerIntegration::start(
            runtime.clone(),
            LoggerConfig::default(),
            provider(IntegrationCatalog::new()),
            Arc::new(|context| {
                if context.user_id.as_deref() == Some("admin") {
                    Ok(())
                } else {
                    Err(Error::NotAllowed("admin required".into()))
                }
            }),
        )
        .unwrap();
        // Startup calls set_log_levels even for an empty combined batch.
        assert_logging_changed(events.try_recv().unwrap());

        let mut invalid = service_call(
            "logger",
            "set_level",
            json!({"secret.module": "DEBUG", "invalid.module": 10}),
        );
        invalid.context.user_id = Some("admin".into());
        assert!(matches!(
            runtime.call_service(invalid),
            Err(Error::InvalidInput(_))
        ));
        assert!(events.try_recv().is_err());

        let mut empty_batch = service_call("logger", "set_level", json!({}));
        empty_batch.context.user_id = Some("admin".into());
        runtime.call_service(empty_batch).unwrap();
        assert_logging_changed(events.try_recv().unwrap());

        let mut default = service_call("logger", "set_default_level", json!({"level":"dEbUg"}));
        default.context.user_id = Some("admin".into());
        runtime.call_service(default).unwrap();
        assert_logging_changed(events.try_recv().unwrap());

        integration
            .manager()
            .update_settings(
                "secret.module",
                TargetKind::Module,
                LoggerLevel::Warning,
                Persistence::None,
            )
            .unwrap();
        assert_logging_changed(events.try_recv().unwrap());
    }

    #[test]
    fn once_settings_apply_for_one_restart_and_are_removed_before_exposure() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime(store.clone());
        let catalog = IntegrationCatalog::from([("demo".into(), vec!["hass_demo".into()])]);
        let first = LoggerIntegration::start(
            runtime.clone(),
            LoggerConfig::default(),
            provider(catalog.clone()),
            allowed(),
        )
        .unwrap();
        first
            .manager()
            .update_settings(
                "demo",
                TargetKind::Integration,
                LoggerLevel::Warning,
                Persistence::Once,
            )
            .unwrap();
        drop(first);

        assert!(store.get_data(STORAGE_KEY).unwrap().is_some());
        let second = LoggerIntegration::start(
            runtime.clone(),
            LoggerConfig::default(),
            provider(catalog.clone()),
            allowed(),
        )
        .unwrap();
        assert_eq!(
            second.manager().effective_level("hass_demo").unwrap(),
            LoggerLevel::Warning
        );
        assert_eq!(
            second
                .manager()
                .log_info(&IntegrationCatalog::new())
                .unwrap(),
            vec![IntegrationLogInfo {
                domain: "demo".into(),
                level: 30,
            }]
        );
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);
        drop(second);

        let third = LoggerIntegration::start(
            runtime,
            LoggerConfig::default(),
            provider(catalog),
            allowed(),
        )
        .unwrap();
        assert_eq!(
            third.manager().effective_level("hass_demo").unwrap(),
            LoggerLevel::Info
        );
        assert!(
            third
                .manager()
                .log_info(&IntegrationCatalog::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn permanent_none_and_notset_update_storage_and_inheritance() {
        let store = Arc::new(MemoryStore::default());
        let catalog = IntegrationCatalog::from([("demo".into(), vec!["hass_demo".into()])]);
        let integration = LoggerIntegration::start(
            runtime(store.clone()),
            LoggerConfig {
                logs: BTreeMap::from([
                    ("homeassistant.components.demo".into(), LoggerLevel::Debug),
                    (
                        "homeassistant.components.demo.child".into(),
                        LoggerLevel::Info,
                    ),
                ]),
                ..LoggerConfig::default()
            },
            provider(catalog),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();

        manager
            .update_settings(
                "homeassistant.components.demo.child",
                TargetKind::Module,
                LoggerLevel::NotSet,
                Persistence::Once,
            )
            .unwrap();
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);
        assert_eq!(
            manager.effective_level("hass_demo::child::worker").unwrap(),
            LoggerLevel::Debug
        );

        manager
            .update_settings(
                "demo",
                TargetKind::Integration,
                LoggerLevel::Warning,
                Persistence::Permanent,
            )
            .unwrap();
        assert!(store.get_data(STORAGE_KEY).unwrap().is_some());
        manager
            .update_settings(
                "demo",
                TargetKind::Integration,
                LoggerLevel::Error,
                Persistence::None,
            )
            .unwrap();
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);
        assert_eq!(
            manager.effective_level("hass_demo::child::worker").unwrap(),
            LoggerLevel::Error
        );
    }

    #[test]
    fn yaml_and_saved_levels_merge_to_chattiest_then_live_override_wins() {
        let store = Arc::new(MemoryStore::default());
        store
            .set_data(
                STORAGE_KEY,
                &json!({
                    "logs": {
                        "demo": {
                            "level": "WARNING",
                            "persistence": "permanent",
                            "type": "integration"
                        }
                    }
                }),
            )
            .unwrap();
        let catalog = IntegrationCatalog::from([("demo".into(), vec!["hass_demo".into()])]);
        let integration = LoggerIntegration::start(
            runtime(store.clone()),
            LoggerConfig {
                logs: BTreeMap::from([(
                    "homeassistant.components.demo".into(),
                    LoggerLevel::Debug,
                )]),
                ..LoggerConfig::default()
            },
            provider(catalog),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();

        // Startup merges saved WARNING with YAML DEBUG using the more-chatty
        // value; a new live setting wins until the process restarts.
        assert_eq!(
            manager.effective_level("hass_demo::child").unwrap(),
            LoggerLevel::Debug
        );
        manager
            .update_settings(
                "demo",
                TargetKind::Integration,
                LoggerLevel::Warning,
                Persistence::None,
            )
            .unwrap();
        assert_eq!(
            manager.effective_level("hass_demo::child").unwrap(),
            LoggerLevel::Warning
        );
        assert_eq!(store.get_data(STORAGE_KEY).unwrap(), None);
    }

    #[test]
    fn dropping_integration_unregisters_services_and_invalidates_weak_handles() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime(store);
        let integration = LoggerIntegration::start(
            runtime.clone(),
            LoggerConfig::default(),
            provider(IntegrationCatalog::new()),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();
        drop(integration);

        assert!(matches!(
            manager.effective_level("anything"),
            Err(Error::Service(_))
        ));
        assert!(matches!(
            runtime.call_service(service_call(
                "logger",
                "set_default_level",
                json!({"level":"DEBUG"})
            )),
            Err(Error::Service(_))
        ));
    }

    #[test]
    fn integration_log_info_only_reports_supplied_native_catalog() {
        let store = Arc::new(MemoryStore::default());
        let integration = LoggerIntegration::start(
            runtime(store),
            LoggerConfig {
                logs: BTreeMap::from([(
                    "homeassistant.components.demo".into(),
                    LoggerLevel::Critical,
                )]),
                ..LoggerConfig::default()
            },
            provider(IntegrationCatalog::from([(
                "demo".into(),
                vec!["hass_demo".into()],
            )])),
            allowed(),
        )
        .unwrap();
        let catalog = IntegrationCatalog::from([
            ("demo".into(), vec!["hass_demo".into()]),
            ("empty".into(), Vec::new()),
        ]);
        assert_eq!(
            integration.manager().log_info(&catalog).unwrap(),
            vec![
                IntegrationLogInfo {
                    domain: "demo".into(),
                    level: 50,
                },
                IntegrationLogInfo {
                    domain: "empty".into(),
                    level: 20,
                }
            ]
        );
    }

    #[test]
    fn log_info_unions_unloaded_integration_settings_and_removes_replaced_entries() {
        let full_catalog = IntegrationCatalog::from([
            ("loaded".into(), vec!["hass_loaded".into()]),
            ("configured".into(), vec!["hass_configured".into()]),
        ]);
        let loaded_catalog =
            IntegrationCatalog::from([("loaded".into(), vec!["hass_loaded".into()])]);
        let integration = LoggerIntegration::start(
            runtime(Arc::new(MemoryStore::default())),
            LoggerConfig::default(),
            provider(full_catalog),
            allowed(),
        )
        .unwrap();
        let manager = integration.manager();
        manager
            .update_settings(
                "configured",
                TargetKind::Integration,
                LoggerLevel::Warning,
                Persistence::None,
            )
            .unwrap();

        assert_eq!(
            manager.log_info(&loaded_catalog).unwrap(),
            vec![
                IntegrationLogInfo {
                    domain: "configured".into(),
                    level: 30,
                },
                IntegrationLogInfo {
                    domain: "loaded".into(),
                    level: 20,
                }
            ]
        );

        manager
            .update_settings(
                "configured",
                TargetKind::Integration,
                LoggerLevel::NotSet,
                Persistence::Permanent,
            )
            .unwrap();
        assert_eq!(
            manager.log_info(&loaded_catalog).unwrap(),
            vec![IntegrationLogInfo {
                domain: "loaded".into(),
                level: 20,
            }]
        );

        manager
            .update_settings(
                "configured",
                TargetKind::Integration,
                LoggerLevel::Debug,
                Persistence::None,
            )
            .unwrap();
        manager
            .update_settings(
                "configured",
                TargetKind::Module,
                LoggerLevel::Debug,
                Persistence::None,
            )
            .unwrap();
        assert_eq!(
            manager.log_info(&loaded_catalog).unwrap(),
            vec![IntegrationLogInfo {
                domain: "loaded".into(),
                level: 20,
            }]
        );
    }
}
