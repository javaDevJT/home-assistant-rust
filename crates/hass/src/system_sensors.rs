//! Config-entry and YAML host for the native time/date and uptime sensors.

use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest,
    ConfigFlowResult, SetupOutcome,
};
use hass_core::{Error as CoreError, Runtime};
use hass_entity::EntityLifecycle;
use hass_registry::{DeviceRegistration, Registries};
use hass_system_sensors::{
    ConfigError, DisplayOption, ProviderError, TIME_DATE_DOMAIN, TimeDateFlowConfig,
    TimeDateIntegration, TimeDateYamlConfig, UPTIME_DOMAIN, UptimeFlowConfig, UptimeIntegration,
    UptimeSensor,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    runtime::Handle,
    sync::oneshot,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};

const TIME_DATE_PROVIDER_ID: &str = "time_date:yaml";
const RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_BASE_SECS: u64 = 5;
const RETRY_MAX_SECS: u64 = 300;

/// One tagged `sensor:` platform block accepted by the root configuration.
///
/// The root loader validates shared sensor fields first and passes this type
/// the full block. Only `platform: time_date` is supported here; the parsed
/// integration-specific fields are kept private so callers cannot bypass
/// validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemSensorYamlPlatform {
    config: TimeDateYamlConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemSensorYamlError {
    ExpectedObject,
    MissingPlatform,
    InvalidPlatformType,
    UnsupportedPlatform(String),
    TimeDate(ConfigError),
}

impl std::fmt::Display for SystemSensorYamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedObject => f.write_str("sensor platform configuration must be an object"),
            Self::MissingPlatform => f.write_str("sensor platform is required"),
            Self::InvalidPlatformType => f.write_str("sensor platform must be a string"),
            Self::UnsupportedPlatform(platform) => {
                write!(f, "unsupported system sensor platform: {platform}")
            }
            Self::TimeDate(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SystemSensorYamlError {}

impl From<ConfigError> for SystemSensorYamlError {
    fn from(error: ConfigError) -> Self {
        Self::TimeDate(error)
    }
}

impl SystemSensorYamlPlatform {
    /// Parse and validate a tagged YAML sensor block represented as a value.
    pub fn parse(value: &Value) -> std::result::Result<Self, SystemSensorYamlError> {
        let object = value
            .as_object()
            .ok_or(SystemSensorYamlError::ExpectedObject)?;
        let platform = object
            .get("platform")
            .ok_or(SystemSensorYamlError::MissingPlatform)?
            .as_str()
            .ok_or(SystemSensorYamlError::InvalidPlatformType)?;
        if platform != TIME_DATE_DOMAIN {
            return Err(SystemSensorYamlError::UnsupportedPlatform(
                platform.to_owned(),
            ));
        }
        let mut platform_config = object.clone();
        platform_config.remove("platform");
        let config = TimeDateYamlConfig::parse(&Value::Object(platform_config))?;
        Ok(Self { config })
    }

    /// Pure validation entrypoint for root YAML loading.
    #[cfg(test)]
    pub fn validate(value: &Value) -> std::result::Result<(), SystemSensorYamlError> {
        Self::parse(value).map(|_| ())
    }

    #[cfg(test)]
    pub const fn platform(&self) -> &'static str {
        TIME_DATE_DOMAIN
    }

    #[cfg(test)]
    pub fn time_date_config(&self) -> &TimeDateYamlConfig {
        &self.config
    }

    pub fn into_time_date_config(self) -> TimeDateYamlConfig {
        self.config
    }
}

impl<'de> Deserialize<'de> for SystemSensorYamlPlatform {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Owns the YAML provider, native config-entry drivers, and retry scheduler.
pub struct SystemSensors {
    entries: Arc<ConfigEntries>,
    state: Arc<SystemSensorState>,
    drivers: Vec<Arc<dyn ConfigEntryDriver>>,
    yaml_time_date: Option<TimeDateIntegration>,
    retry_stop: Option<oneshot::Sender<()>>,
    retry_task: Option<JoinHandle<()>>,
}

struct SystemSensorState {
    runtime: Runtime,
    registries: Registries,
    lifecycle: EntityLifecycle,
    entries: Weak<ConfigEntries>,
    handle: Handle,
    operation: Mutex<()>,
    closed: AtomicBool,
    time_date: Mutex<HashMap<String, TimeDateIntegration>>,
    uptime: Mutex<HashMap<String, UptimeIntegration>>,
}

struct TimeDateConfigEntryDriver {
    state: Weak<SystemSensorState>,
    registries: Registries,
}

struct UptimeConfigEntryDriver {
    state: Weak<SystemSensorState>,
    registries: Registries,
}

impl SystemSensors {
    /// Register the native sensor drivers, restore their entries, and start the
    /// current YAML time/date platform set.
    ///
    /// Platform blocks are flattened in source order under one stable YAML
    /// provider namespace so a changed or empty list can reconcile stale YAML
    /// entities without touching config-entry entities.
    pub async fn start(
        runtime: Runtime,
        registries: Registries,
        lifecycle: EntityLifecycle,
        config_entries: ConfigEntries,
        yaml_platforms: Vec<SystemSensorYamlPlatform>,
    ) -> Result<Self> {
        let entries = Arc::new(config_entries);
        let state = Arc::new(SystemSensorState {
            runtime: runtime.clone(),
            registries,
            lifecycle: lifecycle.clone(),
            entries: Arc::downgrade(&entries),
            handle: Handle::current(),
            operation: Mutex::new(()),
            closed: AtomicBool::new(false),
            time_date: Mutex::new(HashMap::new()),
            uptime: Mutex::new(HashMap::new()),
        });

        // Claim both domains before reconciling the shared YAML provider. A
        // second host over the same ConfigEntries must fail without replacing
        // the first host's YAML entity leases.
        let time_date_driver: Arc<dyn ConfigEntryDriver> = Arc::new(TimeDateConfigEntryDriver {
            state: Arc::downgrade(&state),
            registries: state.registries.clone(),
        });
        if let Err(error) = entries.register_driver(Arc::clone(&time_date_driver)) {
            state.close();
            return Err(error).context("register native time/date config-entry driver");
        }
        let uptime_driver: Arc<dyn ConfigEntryDriver> = Arc::new(UptimeConfigEntryDriver {
            state: Arc::downgrade(&state),
            registries: state.registries.clone(),
        });
        if let Err(error) = entries.register_driver(Arc::clone(&uptime_driver)) {
            state.close();
            entries.unregister_driver(&time_date_driver);
            return Err(error).context("register native uptime config-entry driver");
        }
        let drivers = vec![time_date_driver, uptime_driver];

        let sensors: Vec<_> = yaml_platforms
            .into_iter()
            .flat_map(|platform| platform.into_time_date_config().sensors(None))
            .collect();
        let has_yaml_sensors = !sensors.is_empty();
        let yaml_time_date = match TimeDateIntegration::start_with_sensors(
            runtime.clone(),
            &lifecycle,
            sensors,
            TIME_DATE_PROVIDER_ID,
        ) {
            Ok(integration) => Some(integration),
            Err(error) => {
                state.close();
                unregister_owned_drivers(&entries, &drivers);
                return Err(error).context("start YAML time/date sensors");
            }
        };

        let entries_for_restore = Arc::clone(&entries);
        if let Err(error) =
            tokio::task::spawn_blocking(move || restore_system_sensor_entries(&entries_for_restore))
                .await
                .context("join restored system sensor config entries")
        {
            state.close();
            if let Some(integration) = yaml_time_date {
                let _ = integration.shutdown().await;
            }
            let entries_for_cleanup = Arc::clone(&entries);
            let state_for_cleanup = Arc::clone(&state);
            if let Err(cleanup_error) = tokio::task::spawn_blocking(move || {
                drop(state_for_cleanup.lock_operation());
                let mut cleanup_errors = Vec::new();
                for domain in [TIME_DATE_DOMAIN, UPTIME_DOMAIN] {
                    for entry in entries_for_cleanup.list_entries(Some(domain)) {
                        if entry.state == ConfigEntryState::Loaded
                            && let Err(error) = entries_for_cleanup.unload_entry(&entry.entry_id)
                        {
                            cleanup_errors.push(format!("{}: {error}", entry.entry_id));
                        }
                    }
                }
                state_for_cleanup.shutdown_remaining(&mut cleanup_errors);
                if cleanup_errors.is_empty() {
                    Ok(())
                } else {
                    bail!(cleanup_errors.join("; "))
                }
            })
            .await
            {
                tracing::warn!(%cleanup_error, "failed to clean up system sensors after startup failure");
            }
            unregister_owned_drivers(&entries, &drivers);
            return Err(error);
        }

        let (retry_stop, retry_rx) = oneshot::channel();
        let retry_entries = Arc::clone(&entries);
        let retry_task = tokio::spawn(async move {
            retry_scheduler(retry_entries, retry_rx).await;
        });

        if has_yaml_sensors {
            runtime
                .loaded_components()
                .record_successful_component("sensor");
            runtime
                .loaded_components()
                .record_successful_component("time_date.sensor");
        }

        Ok(Self {
            entries,
            state,
            drivers,
            yaml_time_date,
            retry_stop: Some(retry_stop),
            retry_task: Some(retry_task),
        })
    }

    /// Stop retries, release YAML entities, unload active entries, and join
    /// provider tasks.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(stop) = self.retry_stop.take() {
            let _ = stop.send(());
        }
        let mut errors = Vec::new();
        if let Some(task) = self.retry_task.take()
            && let Err(error) = task.await
        {
            errors.push(format!("join system sensor retry task: {error}"));
        }
        self.state.close();
        let state_for_barrier = Arc::clone(&self.state);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            drop(state_for_barrier.lock_operation());
        })
        .await
        {
            errors.push(format!("join system sensor operations: {error}"));
        }
        if let Some(integration) = self.yaml_time_date.take()
            && let Err(error) = integration.shutdown().await
        {
            errors.push(format!("stop YAML time/date provider: {error}"));
        }

        let entries = Arc::clone(&self.entries);
        let state = Arc::clone(&self.state);
        match tokio::task::spawn_blocking(move || {
            let mut unload_errors = Vec::new();
            for domain in [TIME_DATE_DOMAIN, UPTIME_DOMAIN] {
                for entry in entries.list_entries(Some(domain)) {
                    if entry.state == ConfigEntryState::Loaded
                        && let Err(error) = entries.unload_entry(&entry.entry_id)
                    {
                        unload_errors.push(format!("{}: {error}", entry.entry_id));
                    }
                }
            }
            state.shutdown_remaining(&mut unload_errors);
            if !unload_errors.is_empty() {
                bail!(
                    "failed to unload system sensor config entries: {}",
                    unload_errors.join("; ")
                );
            }
            Ok(())
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error.to_string()),
            Err(error) => errors.push(format!("join system sensor shutdown: {error}")),
        }
        unregister_owned_drivers(&self.entries, &self.drivers);
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("; "))
        }
    }
}

fn unregister_owned_drivers(entries: &ConfigEntries, drivers: &[Arc<dyn ConfigEntryDriver>]) {
    for driver in drivers {
        entries.unregister_driver(driver);
    }
}

impl SystemSensorState {
    fn lock_operation(&self) -> std::sync::MutexGuard<'_, ()> {
        self.operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ensure_open(&self) -> hass_core::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(CoreError::Service(
                "system sensor host is shutting down".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn lock_time_date(&self) -> std::sync::MutexGuard<'_, HashMap<String, TimeDateIntegration>> {
        self.time_date
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_uptime(&self) -> std::sync::MutexGuard<'_, HashMap<String, UptimeIntegration>> {
        self.uptime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn shutdown_remaining(&self, errors: &mut Vec<String>) {
        for (entry_id, integration) in std::mem::take(&mut *self.lock_time_date()) {
            if let Err(error) = self.handle.block_on(integration.shutdown()) {
                errors.push(format!("time/date {entry_id}: {error}"));
            }
        }
        for (entry_id, integration) in std::mem::take(&mut *self.lock_uptime()) {
            if let Err(error) = integration.shutdown() {
                errors.push(format!("uptime {entry_id}: {error}"));
            }
        }
    }
}

impl Drop for SystemSensors {
    fn drop(&mut self) {
        self.state.close();
        if let Some(stop) = self.retry_stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.retry_task.take() {
            task.abort();
        }
        unregister_owned_drivers(&self.entries, &self.drivers);
    }
}

fn upgrade_system_sensor_state(
    state: &Weak<SystemSensorState>,
) -> hass_core::Result<Arc<SystemSensorState>> {
    state.upgrade().ok_or_else(|| {
        CoreError::Service("system sensor integration driver is no longer active".into())
    })
}

impl ConfigEntryDriver for TimeDateConfigEntryDriver {
    fn domain(&self) -> &str {
        TIME_DATE_DOMAIN
    }

    fn integration_type(&self) -> &str {
        "service"
    }

    fn setup(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        let state = upgrade_system_sensor_state(&self.state)?;
        let config = time_date_entry_config(entry, runtime.time_zone().as_str())?;
        let _operation = state.lock_operation();
        state.ensure_open()?;
        if state.lock_time_date().contains_key(&entry.entry_id) {
            return Ok(SetupOutcome::Ready);
        }
        let runtime = runtime.clone();
        let lifecycle = state.lifecycle.clone();
        match state.handle.block_on(async move {
            TimeDateIntegration::start(
                runtime,
                &lifecycle,
                config,
                "time_date:config-entry",
                Some(&entry.entry_id),
            )
        }) {
            Ok(integration) => {
                state
                    .lock_time_date()
                    .insert(entry.entry_id.clone(), integration);
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component("sensor");
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component(TIME_DATE_DOMAIN);
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component("time_date.sensor");
                Ok(SetupOutcome::Ready)
            }
            Err(error) => setup_outcome(error, entry.retry_attempts),
        }
    }

    fn unload(&self, _runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        let _operation = state.lock_operation();
        let Some(integration) = state.lock_time_date().remove(&entry.entry_id) else {
            return Ok(());
        };
        state
            .handle
            .block_on(integration.shutdown())
            .map_err(|error| {
                CoreError::Service(format!(
                    "failed to stop time/date sensors for config entry {}: {error}",
                    entry.entry_id
                ))
            })
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        let state = upgrade_system_sensor_state(&self.state)?;
        let _operation = state.lock_operation();
        state.ensure_open()?;
        let Some(input) = request.user_input.as_ref() else {
            return Ok(time_date_form(None, None));
        };
        let runtime = &state.runtime;
        let flow = match TimeDateFlowConfig::parse(
            &Value::Object(input.clone()),
            Some(runtime.time_zone().as_str()),
        ) {
            Ok(flow) => flow,
            Err(ConfigError::TimezoneNotConfigured) => {
                return Ok(time_date_form(
                    Some("timezone_not_exist"),
                    Some("Home Assistant timezone is not configured"),
                ));
            }
            Err(error) => {
                return Ok(time_date_form(
                    Some("invalid_config"),
                    Some(&error.to_string()),
                ));
            }
        };

        let entries = state.entries.upgrade().ok_or_else(|| {
            CoreError::Service("system sensor config-entry manager is unavailable".into())
        })?;
        if entries
            .list_entries(Some(TIME_DATE_DOMAIN))
            .iter()
            .any(|entry| {
                Some(entry.entry_id.as_str()) != request.entry_id.as_deref()
                    && entry.options.get("display_options")
                        == Some(&Value::String(flow.display_option.key().to_owned()))
            })
        {
            return Ok(ConfigFlowResult::Abort {
                reason: "already_configured".into(),
            });
        }

        Ok(ConfigFlowResult::CreateEntry {
            title: flow.entry_title(),
            data: flow.entry_data(),
            options: flow.entry_options(),
            unique_id: None,
            version: 1,
            minor_version: 1,
        })
    }

    fn remove(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        remove_entry_resources(runtime, &self.registries, entry)
    }
}

impl ConfigEntryDriver for UptimeConfigEntryDriver {
    fn domain(&self) -> &str {
        UPTIME_DOMAIN
    }

    fn integration_type(&self) -> &str {
        "service"
    }

    fn version(&self) -> (u32, u32) {
        (1, 0)
    }

    fn setup(&self, _runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        let state = upgrade_system_sensor_state(&self.state)?;
        let _operation = state.lock_operation();
        state.ensure_open()?;
        if state.lock_uptime().contains_key(&entry.entry_id) {
            return Ok(SetupOutcome::Ready);
        }
        let registration = DeviceRegistration {
            config_entry_id: Some(entry.entry_id.clone()),
            identifiers: BTreeSet::from([(UPTIME_DOMAIN.to_owned(), entry.entry_id.clone())]),
            name: Some(entry.title.clone()),
            entry_type: Some("service".to_owned()),
            ..DeviceRegistration::default()
        };
        let device = match state.registries.get_or_create_device(registration) {
            Ok(device) => device,
            Err(CoreError::InvalidInput(message)) => {
                return Err(CoreError::InvalidInput(message));
            }
            Err(error) => {
                return Ok(SetupOutcome::RetryAfter {
                    seconds: retry_delay(entry.retry_attempts),
                    reason: Some(error.to_string()),
                });
            }
        };
        let sensor = UptimeSensor::new(entry.entry_id.clone(), entry.title.clone());
        match UptimeIntegration::start(&state.lifecycle, sensor, &device.id) {
            Ok(integration) => {
                state
                    .lock_uptime()
                    .insert(entry.entry_id.clone(), integration);
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component("sensor");
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component(UPTIME_DOMAIN);
                state
                    .runtime
                    .loaded_components()
                    .record_successful_component("uptime.sensor");
                Ok(SetupOutcome::Ready)
            }
            Err(error) => setup_outcome(error, entry.retry_attempts),
        }
    }

    fn unload(&self, _runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        let _operation = state.lock_operation();
        if let Some(integration) = state.lock_uptime().remove(&entry.entry_id) {
            integration.shutdown().map_err(|error| {
                CoreError::Service(format!(
                    "failed to stop uptime sensor for config entry {}: {error}",
                    entry.entry_id
                ))
            })?;
        }
        Ok(())
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        let state = upgrade_system_sensor_state(&self.state)?;
        let _operation = state.lock_operation();
        state.ensure_open()?;
        let Some(input) = request.user_input.as_ref() else {
            return Ok(ConfigFlowResult::form(
                "user",
                Some(json!({ "type": "object" })),
            ));
        };
        if let Err(error) = UptimeFlowConfig::parse(&Value::Object(input.clone())) {
            return Ok(ConfigFlowResult::Form {
                step_id: "user".into(),
                data_schema: Some(json!({ "type": "object" })),
                errors: Some(Map::from_iter([("base".into(), json!("invalid_config"))])),
                description_placeholders: Some(Map::from_iter([(
                    "error".into(),
                    json!(error.to_string()),
                )])),
            });
        }
        let entries = state.entries.upgrade().ok_or_else(|| {
            CoreError::Service("system sensor config-entry manager is unavailable".into())
        })?;
        if !entries.list_entries(Some(UPTIME_DOMAIN)).is_empty() {
            return Ok(ConfigFlowResult::Abort {
                reason: "single_instance_allowed".into(),
            });
        }
        Ok(ConfigFlowResult::CreateEntry {
            title: UptimeFlowConfig.entry_title().into(),
            data: UptimeFlowConfig.entry_data(),
            options: UptimeFlowConfig.entry_options(),
            unique_id: None,
            version: 1,
            minor_version: 0,
        })
    }

    fn remove(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        remove_entry_resources(runtime, &self.registries, entry)
    }
}

fn time_date_entry_config(
    entry: &ConfigEntry,
    time_zone: &str,
) -> hass_core::Result<TimeDateYamlConfig> {
    let value = Value::Object(entry.options.clone());
    let flow = TimeDateFlowConfig::parse(&value, Some(time_zone))
        .map_err(|error| CoreError::InvalidInput(error.to_string()))?;
    Ok(TimeDateYamlConfig {
        display_options: vec![flow.display_option],
        entity_namespace: None,
        scan_interval: None,
    })
}

fn time_date_form(error_key: Option<&str>, message: Option<&str>) -> ConfigFlowResult {
    let options = DisplayOption::ALL.map(DisplayOption::key);
    let errors = error_key.map(|key| Map::from_iter([("base".into(), json!(key))]));
    let description_placeholders =
        message.map(|value| Map::from_iter([("error".into(), json!(value))]));
    ConfigFlowResult::Form {
        step_id: "user".into(),
        data_schema: Some(json!({
            "type": "object",
            "required": ["display_options"],
            "properties": {
                "display_options": {"type": "string", "enum": options}
            }
        })),
        errors,
        description_placeholders,
    }
}

fn setup_outcome(error: ProviderError, attempts: u32) -> hass_core::Result<SetupOutcome> {
    if let ProviderError::Config(error) = &error {
        return Err(CoreError::InvalidInput(error.to_string()));
    }
    if let ProviderError::Runtime(CoreError::InvalidInput(message)) = error {
        return Err(CoreError::InvalidInput(message));
    }
    Ok(SetupOutcome::RetryAfter {
        seconds: retry_delay(attempts),
        reason: Some(error.to_string()),
    })
}

fn remove_entry_resources(
    runtime: &Runtime,
    registries: &Registries,
    entry: &ConfigEntry,
) -> hass_core::Result<()> {
    for entity in registries
        .entities()
        .into_iter()
        .filter(|entity| entity.config_entry_id.as_deref() == Some(&entry.entry_id))
    {
        let observed_state = runtime.get_state(&entity.entity_id);
        registries.remove_entity(&entity.entity_id)?;
        if let Some(state) = observed_state {
            runtime.remove_state_if_unchanged(&state, state.context.clone())?;
        }
    }
    for device in registries.devices().into_iter().filter(|device| {
        device.config_entry_id.as_deref() == Some(&entry.entry_id)
            || device.config_entries.iter().any(|id| id == &entry.entry_id)
    }) {
        if device
            .config_entries
            .iter()
            .all(|config_entry_id| config_entry_id == &entry.entry_id)
        {
            registries.remove_device(&device.id)?;
        }
    }
    Ok(())
}

fn retry_delay(attempts: u32) -> u64 {
    RETRY_BASE_SECS
        .saturating_mul(1_u64 << attempts.min(6))
        .min(RETRY_MAX_SECS)
}

fn trace_entry_failure(domain: &str, entry_id: &str, error: &hass_core::Error, message: &str) {
    match domain {
        TIME_DATE_DOMAIN => {
            tracing::warn!(target: "hass::system_sensors::time_date", %entry_id, %error, "{message}")
        }
        UPTIME_DOMAIN => {
            tracing::warn!(target: "hass::system_sensors::uptime", %entry_id, %error, "{message}")
        }
        _ => tracing::warn!(%entry_id, %error, "{message}"),
    }
}

fn restore_system_sensor_entries(entries: &ConfigEntries) {
    for domain in [TIME_DATE_DOMAIN, UPTIME_DOMAIN] {
        for entry in entries.list_entries(Some(domain)) {
            if entry.disabled_by.is_some() || entry.state == ConfigEntryState::Loaded {
                continue;
            }
            if entry.state == ConfigEntryState::SetupRetry {
                if entry
                    .retry_at
                    .is_some_and(|retry_at| retry_at <= Utc::now())
                    && let Err(error) = entries.retry_entry(&entry.entry_id)
                {
                    trace_entry_failure(
                        domain,
                        &entry.entry_id,
                        &error,
                        "system sensor config entry retry failed during restore",
                    );
                }
                continue;
            }
            if let Err(error) = entries.setup_entry(&entry.entry_id) {
                trace_entry_failure(
                    domain,
                    &entry.entry_id,
                    &error,
                    "system sensor config entry setup failed during restore",
                );
            }
        }
    }
}

async fn retry_scheduler(entries: Arc<ConfigEntries>, mut stop: oneshot::Receiver<()>) {
    let mut interval = time::interval(RETRY_POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = interval.tick() => {
                let entries = Arc::clone(&entries);
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    for domain in [TIME_DATE_DOMAIN, UPTIME_DOMAIN] {
                        for entry in entries.list_entries(Some(domain)) {
                            if entry.state == ConfigEntryState::SetupRetry
                                && entry.retry_at.is_some_and(|retry_at| retry_at <= Utc::now())
                                && let Err(error) = entries.retry_entry(&entry.entry_id)
                            {
                                trace_entry_failure(domain, &entry.entry_id, &error, "system sensor config entry retry failed");
                            }
                        }
                    }
                }).await {
                    tracing::warn!(%error, "system sensor retry worker failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;

    fn managers() -> (Runtime, Registries, EntityLifecycle, ConfigEntries) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        (runtime, registries, lifecycle, entries)
    }

    struct ExistingUptimeDriver;

    impl ConfigEntryDriver for ExistingUptimeDriver {
        fn domain(&self) -> &str {
            UPTIME_DOMAIN
        }

        fn setup(
            &self,
            _runtime: &Runtime,
            _entry: &ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form(
                "user",
                Some(json!({"type":"object"})),
            ))
        }
    }

    fn time_input(option: &str) -> Map<String, Value> {
        Map::from_iter([("display_options".into(), json!(option))])
    }

    async fn config_flow(
        entries: ConfigEntries,
        domain: &'static str,
        input: Option<Map<String, Value>>,
    ) -> Value {
        tokio::task::spawn_blocking(move || match input {
            Some(input) => {
                entries.init_config_flow_with_input(domain, "user", Map::new(), Some(input))
            }
            None => entries.init_config_flow(domain, "user", Map::new()),
        })
        .await
        .unwrap()
        .unwrap()
    }

    #[test]
    fn yaml_platform_is_tagged_and_rejects_unknown_or_unsupported_fields() {
        let platform: SystemSensorYamlPlatform = serde_yaml::from_str(
            "platform: time_date\ndisplay_options: [time, date]\nentity_namespace: office\nscan_interval: '00:00:30'\n",
        )
        .unwrap();
        assert_eq!(platform.platform(), "time_date");
        assert_eq!(
            platform.time_date_config().display_options,
            [
                hass_system_sensors::DisplayOption::Time,
                hass_system_sensors::DisplayOption::Date
            ]
        );
        assert_eq!(
            platform.time_date_config().entity_namespace.as_deref(),
            Some("office")
        );
        assert_eq!(
            platform.time_date_config().scan_interval,
            Some(json!("00:00:30"))
        );
        assert!(SystemSensorYamlPlatform::validate(&json!({"platform":"time_date"})).is_ok());
        assert!(serde_yaml::from_str::<SystemSensorYamlPlatform>("platform: uptime\n").is_err());
        assert!(
            serde_yaml::from_str::<SystemSensorYamlPlatform>(
                "platform: time_date\nunsupported: true\n"
            )
            .is_err()
        );
        assert!(
            serde_yaml::from_str::<SystemSensorYamlPlatform>("display_options: time\n").is_err()
        );
    }

    #[tokio::test]
    async fn time_date_flow_creates_entry_and_duplicate_option_aborts() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SystemSensors::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();

        let form = config_flow(entries.clone(), "time_date", None).await;
        assert_eq!(form["type"], "form");
        assert_eq!(
            form["data_schema"]["properties"]["display_options"]["enum"][0],
            "time"
        );

        let created = config_flow(entries.clone(), "time_date", Some(time_input("time"))).await;
        assert_eq!(created["type"], "create_entry");
        assert_eq!(created["result"]["title"], "Time & Date time");
        assert!(
            runtime
                .loaded_components()
                .components()
                .contains(&"time_date.sensor".to_owned())
        );
        assert!(
            runtime
                .loaded_components()
                .all_components()
                .contains(&"sensor".to_owned())
        );
        let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
        let entity = registries
            .entities()
            .into_iter()
            .find(|entity| entity.config_entry_id.as_deref() == Some(&entry_id))
            .unwrap();
        assert_ne!(
            runtime.get_state(&entity.entity_id).unwrap().state,
            "unavailable"
        );

        let duplicate = config_flow(entries.clone(), "time_date", Some(time_input("time"))).await;
        assert_eq!(duplicate["type"], "abort");
        assert_eq!(duplicate["reason"], "already_configured");

        let entries_for_unload = entries.clone();
        tokio::task::spawn_blocking(move || entries_for_unload.unload_entry(&entry_id))
            .await
            .unwrap()
            .unwrap();
        host.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn yaml_platforms_share_one_provider_and_shutdown_removes_generated_states() {
        let (runtime, registries, lifecycle, entries) = managers();
        let time = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "time",
            "scan_interval": "00:00:01"
        }))
        .unwrap();
        let date = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": ["date"]
        }))
        .unwrap();
        let namespaced = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "time",
            "entity_namespace": "office"
        }))
        .unwrap();
        let host = SystemSensors::start(
            runtime.clone(),
            registries,
            lifecycle.clone(),
            entries.clone(),
            vec![time, date, namespaced],
        )
        .await
        .unwrap();

        assert!(
            runtime
                .loaded_components()
                .components()
                .contains(&"time_date.sensor".to_owned())
        );
        assert!(
            runtime
                .loaded_components()
                .all_components()
                .contains(&"sensor".to_owned())
        );
        assert!(runtime.get_state("sensor.time").is_some());
        assert!(runtime.get_state("sensor.date").is_some());
        assert!(runtime.get_state("sensor.office_time").is_some());
        host.shutdown().await.unwrap();
        assert!(entries.driver(TIME_DATE_DOMAIN).is_none());
        assert!(entries.driver(UPTIME_DOMAIN).is_none());
        assert!(runtime.get_state("sensor.time").is_none());
        assert!(runtime.get_state("sensor.date").is_none());
        assert!(runtime.get_state("sensor.office_time").is_none());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_yaml_start_leaves_no_stale_driver_and_can_retry() {
        let (runtime, registries, failed_lifecycle, entries) = managers();
        failed_lifecycle.shutdown().await.unwrap();
        let platform = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "time"
        }))
        .unwrap();

        assert!(
            SystemSensors::start(
                runtime.clone(),
                registries.clone(),
                failed_lifecycle,
                entries.clone(),
                vec![platform],
            )
            .await
            .is_err()
        );

        let entries_for_check = entries.clone();
        assert!(
            tokio::task::spawn_blocking(move || {
                entries_for_check.init_config_flow("time_date", "user", Map::new())
            })
            .await
            .unwrap()
            .is_err()
        );

        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let host = SystemSensors::start(
            runtime.clone(),
            registries,
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            config_flow(entries.clone(), "uptime", None).await["type"],
            "form"
        );
        host.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn partial_driver_registration_failure_closes_the_installed_driver() {
        let (runtime, registries, lifecycle, entries) = managers();
        entries
            .register_driver(Arc::new(ExistingUptimeDriver))
            .unwrap();
        let platform = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "time"
        }))
        .unwrap();

        assert!(
            SystemSensors::start(
                runtime.clone(),
                registries.clone(),
                lifecycle.clone(),
                entries.clone(),
                vec![platform],
            )
            .await
            .is_err()
        );
        assert!(runtime.get_state("sensor.time").is_none());
        assert!(entries.driver(TIME_DATE_DOMAIN).is_none());
        assert!(entries.driver(UPTIME_DOMAIN).is_some());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn drop_unregisters_owned_callbacks_and_allows_host_restart() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SystemSensors::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        let stale = entries.driver(TIME_DATE_DOMAIN).unwrap();
        drop(host);
        assert!(entries.driver(TIME_DATE_DOMAIN).is_none());
        assert!(entries.driver(UPTIME_DOMAIN).is_none());
        let restarted = SystemSensors::start(
            runtime,
            registries,
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert!(!entries.unregister_driver(&stale));
        assert!(entries.driver(TIME_DATE_DOMAIN).is_some());
        restarted.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_host_start_fails_before_changing_yaml_provider_state() {
        let (runtime, registries, lifecycle, entries) = managers();
        let time = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "time"
        }))
        .unwrap();
        let date = SystemSensorYamlPlatform::parse(&json!({
            "platform": "time_date",
            "display_options": "date"
        }))
        .unwrap();
        let host = SystemSensors::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            vec![time],
        )
        .await
        .unwrap();
        let registered_time_date_driver = entries.driver(TIME_DATE_DOMAIN).unwrap();
        assert!(runtime.get_state("sensor.time").is_some());
        assert!(runtime.get_state("sensor.date").is_none());

        let duplicate = SystemSensors::start(
            runtime.clone(),
            registries,
            lifecycle.clone(),
            entries.clone(),
            vec![date],
        )
        .await;
        assert!(duplicate.is_err());
        assert!(Arc::ptr_eq(
            &registered_time_date_driver,
            &entries.driver(TIME_DATE_DOMAIN).unwrap()
        ));
        assert!(runtime.get_state("sensor.time").is_some());
        assert!(runtime.get_state("sensor.date").is_none());

        host.shutdown().await.unwrap();
        assert!(entries.driver(TIME_DATE_DOMAIN).is_none());
        assert!(entries.driver(UPTIME_DOMAIN).is_none());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn uptime_flow_is_single_instance_and_registers_service_device() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SystemSensors::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        let form = config_flow(entries.clone(), "uptime", None).await;
        assert_eq!(form["type"], "form");

        let created = config_flow(entries.clone(), "uptime", Some(Map::new())).await;
        assert_eq!(created["type"], "create_entry");
        assert_eq!(created["result"]["title"], "Uptime");
        assert_eq!(created["result"]["state"], "loaded", "{created}");
        assert!(
            runtime
                .loaded_components()
                .components()
                .contains(&"uptime.sensor".to_owned())
        );
        assert!(
            runtime
                .loaded_components()
                .all_components()
                .contains(&"sensor".to_owned())
        );
        let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
        let entity = registries
            .entities()
            .into_iter()
            .find(|entity| entity.config_entry_id.as_deref() == Some(&entry_id))
            .unwrap();
        let device = registries
            .devices()
            .into_iter()
            .find(|device| device.config_entry_id.as_deref() == Some(&entry_id))
            .unwrap();
        assert_eq!(device.entry_type.as_deref(), Some("service"));
        assert_eq!(device.name.as_deref(), Some("Uptime"));
        assert_eq!(
            runtime.get_state(&entity.entity_id).unwrap().attributes["device_class"],
            "uptime"
        );

        let duplicate = config_flow(entries.clone(), "uptime", Some(Map::new())).await;
        assert_eq!(duplicate["type"], "abort");
        assert_eq!(duplicate["reason"], "single_instance_allowed");

        let entries_for_remove = entries.clone();
        let entry_id_for_remove = entry_id.clone();
        tokio::task::spawn_blocking(move || entries_for_remove.remove_entry(&entry_id_for_remove))
            .await
            .unwrap()
            .unwrap();
        assert!(
            registries
                .devices()
                .iter()
                .all(|device| device.config_entry_id.as_deref() != Some(entry_id.as_str()))
        );
        assert!(runtime.get_state(&entity.entity_id).is_none());
        host.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retained_config_entry_driver_cannot_reactivate_after_host_shutdown() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SystemSensors::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        let created = config_flow(entries.clone(), "time_date", Some(time_input("time"))).await;
        let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
        let entity = registries
            .entities()
            .into_iter()
            .find(|entity| entity.config_entry_id.as_deref() == Some(&entry_id))
            .unwrap();
        let entry = entries
            .list_entries(Some(TIME_DATE_DOMAIN))
            .into_iter()
            .find(|entry| entry.entry_id == entry_id)
            .unwrap();
        let retained_driver = entries.driver(TIME_DATE_DOMAIN).unwrap();
        assert!(runtime.get_state(&entity.entity_id).is_some());

        host.shutdown().await.unwrap();
        assert!(entries.driver(TIME_DATE_DOMAIN).is_none());
        assert!(entries.driver(UPTIME_DOMAIN).is_none());
        assert_eq!(
            runtime.get_state(&entity.entity_id).unwrap().state,
            "unavailable"
        );

        let error = retained_driver.setup(&runtime, &entry).unwrap_err();
        assert!(error.to_string().contains("no longer active"));

        let entries_for_setup = entries.clone();
        let entry_id_for_setup = entry_id.clone();
        let error =
            tokio::task::spawn_blocking(move || entries_for_setup.setup_entry(&entry_id_for_setup))
                .await
                .unwrap()
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no native integration driver registered")
        );
        assert_eq!(
            runtime.get_state(&entity.entity_id).unwrap().state,
            "unavailable"
        );
        assert!(
            registries
                .entities()
                .iter()
                .any(|registered| registered.config_entry_id.as_deref() == Some(entry_id.as_str()))
        );
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_entry_unload_and_host_shutdown_keep_the_runtime_responsive() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SystemSensors::start(
            runtime.clone(),
            registries,
            lifecycle.clone(),
            entries.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
        let created = config_flow(entries.clone(), "time_date", Some(time_input("time"))).await;
        let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let entries_for_unload = entries.clone();
        let entry_id_for_unload = entry_id.clone();
        let unload_barrier = Arc::clone(&barrier);
        let unload = tokio::task::spawn_blocking(move || {
            unload_barrier.wait();
            entries_for_unload.unload_entry(&entry_id_for_unload)
        });

        barrier.wait();
        let shutdown = tokio::spawn(host.shutdown());
        let (shutdown, unload) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(shutdown, unload)
        })
        .await
        .expect("concurrent unload and host shutdown should complete");
        shutdown.unwrap().unwrap();
        unload.unwrap().unwrap();
        assert_eq!(
            runtime.get_state("sensor.time").unwrap().state,
            "unavailable"
        );
        lifecycle.shutdown().await.unwrap();
    }
}
