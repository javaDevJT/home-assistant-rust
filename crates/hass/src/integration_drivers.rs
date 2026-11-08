//! Executable native config-entry drivers owned by the primary binary.

use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest,
    ConfigFlowResult, SetupOutcome,
};
use hass_core::{Error as CoreError, Runtime};
use hass_mqtt::{MqttConfig, MqttEntityMetadata, MqttIntegration};
#[cfg(test)]
use hass_registry::EntityUpdate;
use hass_registry::{DeviceRegistration, EntityRegistration, Registries};
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

#[cfg(test)]
#[path = "integration_driver_lifecycle_tests.rs"]
mod owned_lifecycle_tests;

const MQTT_DOMAIN: &str = "mqtt";
const MQTT_PLATFORM: &str = "mqtt";
const RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const REGISTRY_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const RETRY_BASE_SECS: u64 = 5;
const RETRY_MAX_SECS: u64 = 300;

/// Owns the MQTT driver and every MQTT client created through its config entries.
pub struct NativeDrivers {
    entries: Arc<ConfigEntries>,
    state: Arc<DriverState>,
    driver: Option<Arc<dyn ConfigEntryDriver>>,
    startup: Option<JoinHandle<Result<()>>>,
    cleanup_started: bool,
    stops: Vec<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
}

struct DriverState {
    entries: Weak<ConfigEntries>,
    registries: Registries,
    entity_lifecycle: Option<hass_entity::EntityLifecycle>,
    closed: AtomicBool,
    handle: Handle,
    mqtt_topics: Vec<String>,
    active: Mutex<HashMap<String, MqttIntegration>>,
    synced_entities: Mutex<HashMap<(String, String, String), MqttEntityMetadata>>,
    lifecycle: Mutex<()>,
}

struct MqttConfigEntryDriver {
    state: Arc<DriverState>,
}

impl NativeDrivers {
    /// Register and restore native drivers, including the real MQTT broker client.
    #[cfg(test)]
    pub async fn start(
        runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        mqtt_topics: Vec<String>,
        yaml_mqtt: Option<MqttConfig>,
    ) -> Result<Self> {
        Self::start_inner(runtime, entries, registries, mqtt_topics, yaml_mqtt, None).await
    }

    /// Use the application's existing lifecycle for SDK-backed MQTT entities.
    pub async fn start_with_entity_lifecycle(
        runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        mqtt_topics: Vec<String>,
        yaml_mqtt: Option<MqttConfig>,
        lifecycle: hass_entity::EntityLifecycle,
    ) -> Result<Self> {
        Self::start_inner(
            runtime,
            entries,
            registries,
            mqtt_topics,
            yaml_mqtt,
            Some(lifecycle),
        )
        .await
    }

    async fn start_inner(
        runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        mqtt_topics: Vec<String>,
        yaml_mqtt: Option<MqttConfig>,
        entity_lifecycle: Option<hass_entity::EntityLifecycle>,
    ) -> Result<Self> {
        // The runtime parameter is part of the public startup contract; config
        // entries carry the same clone into each synchronous driver callback.
        let _ = runtime;
        let entries = Arc::new(entries);
        let state = Arc::new(DriverState {
            entries: Arc::downgrade(&entries),
            registries,
            entity_lifecycle,
            closed: AtomicBool::new(false),
            handle: Handle::current(),
            mqtt_topics,
            active: Mutex::new(HashMap::new()),
            synced_entities: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
        });
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(MqttConfigEntryDriver {
            state: Arc::clone(&state),
        });
        entries
            .register_driver(Arc::clone(&driver))
            .context("register native MQTT config-entry driver")?;
        let mut host = Self {
            entries,
            state,
            driver: Some(driver),
            startup: None,
            cleanup_started: false,
            stops: Vec::new(),
            tasks: Vec::new(),
        };
        let startup_entries = Arc::clone(&host.entries);
        let startup_state = Arc::clone(&host.state);
        host.startup = Some(tokio::task::spawn_blocking(move || {
            if !startup_state.closed.load(Ordering::Acquire) {
                if let Some(config) = yaml_mqtt {
                    import_yaml_config(&startup_entries, config)?;
                }
                restore_mqtt_entries(&startup_entries, &startup_state);
            }
            Ok(())
        }));
        let startup_result = match host.startup.as_mut().expect("startup worker").await {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("join native MQTT startup: {error}")),
        };
        host.startup.take();
        if let Err(error) = startup_result {
            let _ = host.shutdown().await;
            return Err(error);
        }

        let (retry_stop, retry_rx) = oneshot::channel();
        let retry_entries = Arc::clone(&host.entries);
        let retry_state = Arc::clone(&host.state);
        let retry_task = tokio::spawn(async move {
            retry_scheduler(retry_entries, retry_state, retry_rx).await;
        });

        let (registry_stop, registry_rx) = oneshot::channel();
        let registry_state = Arc::clone(&host.state);
        let registry_task = tokio::spawn(async move {
            registry_reconciler(registry_state, registry_rx).await;
        });

        host.stops = vec![retry_stop, registry_stop];
        host.tasks = vec![retry_task, registry_task];
        Ok(host)
    }

    /// Stop background work, unload all MQTT entries, and join every MQTT task.
    pub async fn shutdown(mut self) -> Result<()> {
        self.begin_cleanup()
            .expect("native driver cleanup owner")
            .await
            .context("join native MQTT cleanup")?
    }

    fn begin_cleanup(&mut self) -> Option<JoinHandle<Result<()>>> {
        if self.cleanup_started {
            return None;
        }
        self.cleanup_started = true;
        self.state.closed.store(true, Ordering::Release);
        for stop in self.stops.drain(..) {
            let _ = stop.send(());
        }
        let startup = self.startup.take();
        let tasks = std::mem::take(&mut self.tasks);
        let entries = Arc::clone(&self.entries);
        let state = Arc::clone(&self.state);
        let driver = self.driver.take();
        Some(self.state.handle.spawn(async move {
            let mut errors = Vec::new();
            if let Some(task) = startup {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(error.to_string()),
                    Err(error) => errors.push(format!("join MQTT startup: {error}")),
                }
            }
            for task in tasks {
                if let Err(error) = task.await {
                    errors.push(format!("join native driver task: {error}"));
                }
            }
            let cleanup_entries = Arc::clone(&entries);
            let cleanup = tokio::task::spawn_blocking(move || {
                let mut unload_errors = Vec::new();
                for entry in cleanup_entries.list_entries(Some(MQTT_DOMAIN)) {
                    if entry.state == ConfigEntryState::Loaded
                        && let Err(error) = cleanup_entries.unload_entry(&entry.entry_id)
                    {
                        unload_errors.push(format!("{}: {error}", entry.entry_id));
                    }
                }
                if let Err(error) = state.shutdown_remaining() {
                    unload_errors.push(error.to_string());
                }
                if !unload_errors.is_empty() {
                    bail!(
                        "failed to unload MQTT config entries: {}",
                        unload_errors.join("; ")
                    );
                }
                Ok(())
            })
            .await;
            match cleanup {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(format!("join native MQTT shutdown: {error}")),
            }
            if let Some(driver) = driver {
                entries.unregister_driver(&driver);
            }
            if errors.is_empty() {
                Ok(())
            } else {
                bail!("native MQTT cleanup: {}", errors.join("; "))
            }
        }))
    }
}

impl Drop for NativeDrivers {
    fn drop(&mut self) {
        if let Some(task) = self.begin_cleanup() {
            drop(task);
        }
    }
}

impl DriverState {
    fn ensure_open(&self) -> hass_core::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(CoreError::Service("native MQTT driver is stopping".into()))
        } else {
            Ok(())
        }
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, HashMap<String, MqttIntegration>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_synced_entities(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<(String, String, String), MqttEntityMetadata>> {
        self.synced_entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn shutdown_remaining(&self) -> hass_core::Result<()> {
        let _lifecycle = self.lock_lifecycle();
        let remaining = std::mem::take(&mut *self.lock_active());
        let mut first_error = None;
        for (entry_id, integration) in remaining {
            if let Err(error) = self.handle.block_on(integration.shutdown()) {
                first_error.get_or_insert_with(|| {
                    CoreError::Service(format!(
                        "failed to stop MQTT client for config entry {entry_id}: {error}"
                    ))
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn reconcile_registries(&self) -> hass_core::Result<()> {
        let active = self.lock_active();
        for (entry_id, integration) in active.iter() {
            for mut metadata in integration.entity_metadata() {
                if metadata.entity_lifecycle_managed {
                    // The SDK lease owns platform identity and device metadata;
                    // legacy reconciliation remains for the other platforms.
                    continue;
                }
                let key = (
                    entry_id.clone(),
                    metadata.platform.clone(),
                    metadata.unique_id.clone(),
                );
                let registered_entity_id = self
                    .registries
                    .entities()
                    .into_iter()
                    .find(|entity| {
                        entity.platform == MQTT_PLATFORM
                            && entity.domain == metadata.platform
                            && entity.unique_id == metadata.unique_id
                    })
                    .map(|entity| entity.entity_id);
                if self
                    .lock_synced_entities()
                    .get(&key)
                    .is_some_and(|synced| synced == &metadata)
                    && registered_entity_id.as_deref() == Some(metadata.entity_id.as_str())
                {
                    continue;
                }
                let device_id =
                    register_device(&self.registries, entry_id, metadata.device.as_ref())?;
                let mut registration = EntityRegistration::new(
                    metadata.platform.clone(),
                    MQTT_PLATFORM,
                    metadata.unique_id.clone(),
                );
                registration.config_entry_id = Some(entry_id.clone());
                registration.device_id = device_id;
                registration.name = Some(metadata.name.clone());
                registration.original_name = Some(metadata.name.clone());
                registration.suggested_object_id = metadata
                    .entity_id
                    .split_once('.')
                    .map(|(_, object_id)| object_id.to_owned());
                let entity = self.registries.get_or_create_entity(registration)?;
                if entity.entity_id != metadata.entity_id
                    && let Err(error) =
                        integration.rebind_entity_id(&metadata.entity_id, &entity.entity_id)
                    && !matches!(error, CoreError::NotFound(_))
                {
                    return Err(error);
                }
                metadata.entity_id = entity.entity_id;
                self.lock_synced_entities().insert(key, metadata);
            }
        }
        Ok(())
    }
}

impl ConfigEntryDriver for MqttConfigEntryDriver {
    fn domain(&self) -> &str {
        MQTT_DOMAIN
    }

    fn integration_type(&self) -> &str {
        "hub"
    }

    fn supports_options(&self) -> bool {
        true
    }

    fn supports_reconfigure(&self) -> bool {
        true
    }

    fn setup(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        self.state.ensure_open()?;
        let config = effective_config(entry)?;
        let _lifecycle = self.state.lock_lifecycle();
        {
            let active = self.state.lock_active();
            if active.contains_key(&entry.entry_id) {
                return Ok(SetupOutcome::Ready);
            }
            if !active.is_empty() {
                return Err(CoreError::InvalidInput(
                    "only one MQTT config entry can be active".into(),
                ));
            }
        }

        let integration = match self.state.handle.block_on(async {
            if let Some(lifecycle) = &self.state.entity_lifecycle {
                MqttIntegration::start_with_entity_lifecycle(
                    runtime.clone(),
                    config.clone(),
                    lifecycle.clone(),
                    self.state.registries.clone(),
                    &entry.entry_id,
                )
                .await
            } else {
                MqttIntegration::start(runtime.clone(), config.clone()).await
            }
        }) {
            Ok(integration) => integration,
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

        if self.state.closed.load(Ordering::Acquire) {
            self.state.handle.block_on(integration.shutdown())?;
            return Err(CoreError::Service(
                "MQTT setup canceled during shutdown".into(),
            ));
        }
        for topic in &self.state.mqtt_topics {
            if let Err(error) = self
                .state
                .handle
                .block_on(integration.subscribe_topic(topic, config.qos))
            {
                let _ = self.state.handle.block_on(integration.shutdown());
                if matches!(error, CoreError::InvalidInput(_)) {
                    return Err(error);
                }
                return Ok(SetupOutcome::RetryAfter {
                    seconds: retry_delay(entry.retry_attempts),
                    reason: Some(error.to_string()),
                });
            }
        }

        let mut active = self.state.lock_active();
        if self.state.closed.load(Ordering::Acquire) {
            drop(active);
            self.state.handle.block_on(integration.shutdown())?;
            return Err(CoreError::Service(
                "MQTT setup canceled during shutdown".into(),
            ));
        }
        active.insert(entry.entry_id.clone(), integration);
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        let _lifecycle = self.state.lock_lifecycle();
        let integration = self.state.lock_active().remove(&entry.entry_id);
        if let Some(integration) = integration {
            self.state
                .handle
                .block_on(integration.shutdown())
                .map_err(|error| {
                    CoreError::Service(format!(
                        "failed to stop MQTT client for config entry {}: {error}",
                        entry.entry_id
                    ))
                })?;
        }
        Ok(())
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        self.state.ensure_open()?;
        let Some(input) = request.user_input.as_ref() else {
            return Ok(ConfigFlowResult::Form {
                step_id: "broker".into(),
                data_schema: Some(mqtt_config_schema(None)),
                errors: None,
                description_placeholders: None,
            });
        };
        let mut input = input.clone();
        let clear_password = input
            .remove("clear_password")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if clear_password {
            input.insert("password".into(), Value::Null);
        }
        let config = match config_from_map(&input) {
            Ok(config) => config,
            Err(error) => {
                let mut errors = Map::new();
                errors.insert("base".into(), json!("invalid_config"));
                return Ok(ConfigFlowResult::Form {
                    step_id: "broker".into(),
                    data_schema: Some(mqtt_config_schema(Some(&input))),
                    errors: Some(errors),
                    description_placeholders: Some(Map::from_iter([(
                        "error".into(),
                        json!(error.to_string()),
                    )])),
                });
            }
        };
        let mut config = config;
        let mut options = Map::new();
        let mut unique_id = request.unique_id.clone();
        if let Some(entry_id) = request.entry_id.as_deref()
            && let Some(entry) = self.current_entry(entry_id)
        {
            options = entry.options;
            unique_id = entry.unique_id;
            if request.context.get("yaml_import").and_then(Value::as_bool) != Some(true)
                && !clear_password
                && input
                    .get("password")
                    .is_none_or(|password| password.is_null() || password.as_str() == Some(""))
            {
                config.password = entry
                    .data
                    .get("password")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        } else if let Some(value) = request.context.get("native_options")
            && let Some(previous) = value.as_object()
        {
            options = previous.clone();
        }
        Ok(ConfigFlowResult::CreateEntry {
            title: config.host.clone(),
            data: config_to_map(&config)?,
            options,
            unique_id,
            version: 1,
            minor_version: 1,
        })
    }

    fn options_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        self.state.ensure_open()?;
        let entry_id = request.entry_id.as_deref().ok_or_else(|| {
            CoreError::InvalidInput("MQTT options flow is missing entry_id".into())
        })?;
        let entry = self
            .current_entry(entry_id)
            .ok_or_else(|| CoreError::NotFound(format!("config entry {entry_id}")))?;
        let mut options = entry.options.clone();
        if let Some(input) = request.user_input.as_ref() {
            for (key, value) in input {
                if !is_option_key(key) {
                    return Ok(option_form(
                        &entry,
                        Some(format!("unknown MQTT option {key}")),
                    ));
                }
                options.insert(key.clone(), value.clone());
            }
            let mut effective = entry.data.clone();
            for (key, value) in &options {
                effective.insert(key.clone(), value.clone());
            }
            if let Err(error) = config_from_map(&effective) {
                return Ok(option_form(&entry, Some(error.to_string())));
            }
            return Ok(ConfigFlowResult::CreateEntry {
                title: entry.title,
                data: entry.data,
                options,
                unique_id: entry.unique_id,
                version: entry.version,
                minor_version: entry.minor_version,
            });
        }
        Ok(option_form(&entry, None))
    }

    fn remove(&self, runtime: &Runtime, entry: &ConfigEntry) -> hass_core::Result<()> {
        self.state
            .lock_synced_entities()
            .retain(|(config_entry_id, _, _), _| config_entry_id != &entry.entry_id);
        let entities = self
            .state
            .registries
            .entities()
            .into_iter()
            .filter(|entity| entity.config_entry_id.as_deref() == Some(&entry.entry_id))
            .collect::<Vec<_>>();
        for entity in entities {
            self.state.registries.remove_entity(&entity.entity_id)?;
            runtime.remove_state(&entity.entity_id, hass_core::Context::default())?;
        }
        let devices = self
            .state
            .registries
            .devices()
            .into_iter()
            .filter(|device| {
                device.config_entry_id.as_deref() == Some(&entry.entry_id)
                    || device.config_entries.iter().any(|id| id == &entry.entry_id)
            })
            .collect::<Vec<_>>();
        for device in devices {
            let other_entries = device
                .config_entries
                .iter()
                .filter(|id| *id != &entry.entry_id)
                .count();
            if other_entries == 0 {
                self.state
                    .registries
                    .remove_config_entry_from_device(&device.id, &entry.entry_id)?;
            }
        }
        Ok(())
    }
}

impl MqttConfigEntryDriver {
    fn current_entry(&self, entry_id: &str) -> Option<ConfigEntry> {
        self.state.entries.upgrade()?.get_entry(entry_id)
    }
}

fn import_yaml_config(entries: &ConfigEntries, config: MqttConfig) -> hass_core::Result<()> {
    config.validate()?;
    let data = config_to_map(&config)?;
    let mqtt_entries = entries.list_entries(Some(MQTT_DOMAIN));
    if let Some(existing) = mqtt_entries.iter().find(|entry| entry.source == "import") {
        if existing.data == data {
            return Ok(());
        }
        let mut context = Map::new();
        context.insert("entry_id".into(), json!(existing.entry_id));
        context.insert(
            "native_options".into(),
            Value::Object(existing.options.clone()),
        );
        context.insert("yaml_import".into(), json!(true));
        let result =
            entries.init_config_flow_with_input(MQTT_DOMAIN, "reconfigure", context, Some(data))?;
        ensure_flow_completed(&result)?;
        return Ok(());
    }

    // A UI-created broker entry takes precedence over YAML. Creating a second
    // live MQTT client would conflict on the real MQTT services.
    if !mqtt_entries.is_empty() {
        return Ok(());
    }

    let result =
        entries.init_config_flow_with_input(MQTT_DOMAIN, "import", Map::new(), Some(data))?;
    ensure_flow_completed(&result)
}

fn ensure_flow_completed(result: &Value) -> hass_core::Result<()> {
    match result.get("type").and_then(Value::as_str) {
        Some("create_entry") => Ok(()),
        Some("abort") => Err(CoreError::InvalidInput(
            result
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("MQTT config flow aborted")
                .to_owned(),
        )),
        Some("form") => Err(CoreError::InvalidInput(
            "MQTT config flow did not complete with the supplied configuration".into(),
        )),
        _ => Err(CoreError::InvalidInput(
            "MQTT config flow returned an unsupported result".into(),
        )),
    }
}

fn restore_mqtt_entries(entries: &ConfigEntries, state: &DriverState) {
    for entry in entries.list_entries(Some(MQTT_DOMAIN)) {
        if state.closed.load(Ordering::Acquire) {
            break;
        }
        if entry.disabled_by.is_some() || entry.state == ConfigEntryState::Loaded {
            continue;
        }
        if entry.state == ConfigEntryState::SetupRetry {
            if entry
                .retry_at
                .is_some_and(|retry_at| retry_at <= Utc::now())
            {
                let _ = entries.retry_entry(&entry.entry_id);
            }
            continue;
        }
        if let Err(error) = entries.setup_entry(&entry.entry_id) {
            tracing::warn!(entry_id = %entry.entry_id, error = %error, "MQTT config entry setup did not complete");
        }
    }
}

async fn retry_scheduler(
    entries: Arc<ConfigEntries>,
    state: Arc<DriverState>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut interval = time::interval(RETRY_POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = interval.tick() => {
                let entries = Arc::clone(&entries);
                let state = Arc::clone(&state);
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    for entry in entries.list_entries(Some(MQTT_DOMAIN)) {
                        if state.closed.load(Ordering::Acquire) { break; }
                        if entry.state == ConfigEntryState::SetupRetry
                            && entry.retry_at.is_some_and(|retry_at| retry_at <= Utc::now())
                            && let Err(error) = entries.retry_entry(&entry.entry_id)
                        {
                            tracing::warn!(entry_id = %entry.entry_id, error = %error, "MQTT config entry retry failed");
                        }
                    }
                }).await {
                    tracing::warn!(%error, "MQTT retry scheduler worker failed");
                }
            }
        }
    }
}

async fn registry_reconciler(state: Arc<DriverState>, mut stop: oneshot::Receiver<()>) {
    let mut interval = time::interval(REGISTRY_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = interval.tick() => {
                let state = Arc::clone(&state);
                match tokio::task::spawn_blocking(move || state.reconcile_registries()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "MQTT registry reconciliation failed"),
                    Err(error) => tracing::warn!(%error, "MQTT registry reconciliation worker failed"),
                }
            }
        }
    }
}

fn effective_config(entry: &ConfigEntry) -> hass_core::Result<MqttConfig> {
    let mut config = entry.data.clone();
    for (key, value) in &entry.options {
        config.insert(key.clone(), value.clone());
    }
    config_from_map(&config)
}

fn config_from_map(input: &Map<String, Value>) -> hass_core::Result<MqttConfig> {
    let config: MqttConfig = serde_json::from_value(Value::Object(input.clone()))
        .map_err(|error| CoreError::InvalidInput(format!("invalid MQTT config: {error}")))?;
    config.validate()?;
    Ok(config)
}

fn config_to_map(config: &MqttConfig) -> hass_core::Result<Map<String, Value>> {
    serde_json::to_value(config)
        .map_err(|error| {
            CoreError::InvalidInput(format!("could not serialize MQTT config: {error}"))
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| CoreError::InvalidInput("MQTT config did not serialize as an object".into()))
}

fn mqtt_config_schema(current: Option<&Map<String, Value>>) -> Value {
    let default = |key: &str, fallback: Value| {
        current
            .and_then(|values| values.get(key))
            .cloned()
            .unwrap_or(fallback)
    };
    json!({
        "type": "object",
        "required": ["host"],
        "properties": {
            "host": {"type": "string", "default": default("host", json!(""))},
            "port": {"type": "integer", "minimum": 1, "maximum": 65535, "default": default("port", json!(1883))},
            "client_id": {"type": "string", "default": default("client_id", json!("home-assistant-rust"))},
            "username": {"type": ["string", "null"], "default": default("username", Value::Null)},
            "password": {"type": ["string", "null"], "format": "password", "default": Value::Null},
            "clear_password": {"type": "boolean", "default": false},
            "tls": {"type": "boolean", "default": default("tls", json!(false))},
            "keep_alive_secs": {"type": "integer", "minimum": 1, "default": default("keep_alive_secs", json!(30))},
            "qos": {"type": "integer", "minimum": 0, "maximum": 2, "default": default("qos", json!(0))},
            "discovery_prefix": {"type": "string", "default": default("discovery_prefix", json!("homeassistant"))},
            "discovery_enabled": {"type": "boolean", "default": default("discovery_enabled", json!(true))},
            "optimistic": {"type": "boolean", "default": default("optimistic", json!(false))},
            "max_payload_bytes": {"type": "integer", "minimum": 1, "default": default("max_payload_bytes", json!(262144))}
        }
    })
}

fn option_form(entry: &ConfigEntry, error: Option<String>) -> ConfigFlowResult {
    let mut values = entry.data.clone();
    for (key, value) in &entry.options {
        values.insert(key.clone(), value.clone());
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "discovery_enabled": {"type": "boolean", "default": values.get("discovery_enabled").cloned().unwrap_or(json!(true))},
            "discovery_prefix": {"type": "string", "default": values.get("discovery_prefix").cloned().unwrap_or(json!("homeassistant"))},
            "qos": {"type": "integer", "minimum": 0, "maximum": 2, "default": values.get("qos").cloned().unwrap_or(json!(0))},
            "optimistic": {"type": "boolean", "default": values.get("optimistic").cloned().unwrap_or(json!(false))},
            "max_payload_bytes": {"type": "integer", "minimum": 1, "default": values.get("max_payload_bytes").cloned().unwrap_or(json!(262144))}
        }
    });
    let errors = error
        .as_ref()
        .map(|_| Map::from_iter([("base".into(), json!("invalid_config"))]));
    let placeholders = error.map(|error| Map::from_iter([("error".into(), json!(error))]));
    ConfigFlowResult::Form {
        step_id: "init".into(),
        data_schema: Some(schema),
        errors,
        description_placeholders: placeholders,
    }
}

fn is_option_key(key: &str) -> bool {
    matches!(
        key,
        "discovery_enabled" | "discovery_prefix" | "qos" | "optimistic" | "max_payload_bytes"
    )
}

fn retry_delay(attempts: u32) -> u64 {
    RETRY_BASE_SECS
        .saturating_mul(1u64.checked_shl(attempts.min(6)).unwrap_or(u64::MAX))
        .min(RETRY_MAX_SECS)
}

fn register_device(
    registries: &Registries,
    entry_id: &str,
    value: Option<&Value>,
) -> hass_core::Result<Option<String>> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Ok(None);
    };
    let identifiers = string_set(object.get("identifiers"))
        .into_iter()
        .map(|identifier| (MQTT_DOMAIN.to_owned(), identifier))
        .collect::<BTreeSet<_>>();
    let connections = pair_set(object.get("connections"));
    if identifiers.is_empty() && connections.is_empty() {
        return Ok(None);
    }

    let via_device_id = object
        .get("via_device")
        .and_then(Value::as_str)
        .map(|identifier| {
            let mut parent = DeviceRegistration {
                config_entry_id: Some(entry_id.to_owned()),
                ..DeviceRegistration::default()
            };
            parent
                .identifiers
                .insert((MQTT_DOMAIN.to_owned(), identifier.to_owned()));
            registries
                .get_or_create_device(parent)
                .map(|device| device.id)
        })
        .transpose()?;

    let registration = DeviceRegistration {
        config_entry_id: Some(entry_id.to_owned()),
        identifiers,
        connections,
        name: string_value(object.get("name")),
        manufacturer: string_value(object.get("manufacturer")),
        model: string_value(object.get("model")),
        model_id: string_value(object.get("model_id")),
        serial_number: string_value(object.get("serial_number")),
        sw_version: string_value(object.get("sw_version")),
        hw_version: string_value(object.get("hw_version")),
        configuration_url: string_value(object.get("configuration_url")),
        via_device_id,
        ..DeviceRegistration::default()
    };
    Ok(Some(registries.get_or_create_device(registration)?.id))
}

fn string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn pair_set(value: Option<&Value>) -> BTreeSet<(String, String)> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .filter_map(|pair| {
            Some((
                pair.first()?.as_str()?.to_owned(),
                pair.get(1)?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{Context, MemoryStore, ServiceCall, ServiceResult};
    use std::{
        collections::HashSet,
        io,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    struct LocalBroker {
        address: String,
        subscriptions: tokio::sync::mpsc::UnboundedReceiver<String>,
        publications: tokio::sync::mpsc::UnboundedReceiver<String>,
        task: JoinHandle<()>,
    }

    impl LocalBroker {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            Self::start_listener(listener, address).await
        }

        async fn start_on(port: u16) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            Self::start_listener(listener, address).await
        }

        async fn start_listener(listener: TcpListener, address: String) -> Self {
            let (subscriptions_tx, subscriptions) = tokio::sync::mpsc::unbounded_channel();
            let (publications_tx, publications) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let subscriptions = subscriptions_tx.clone();
                    let publications = publications_tx.clone();
                    if serve_client(stream, subscriptions, publications)
                        .await
                        .is_err()
                    {
                        continue;
                    }
                }
            });
            Self {
                address,
                subscriptions,
                publications,
                task,
            }
        }

        async fn wait_for_subscriptions(&mut self, expected: &[&str]) {
            let deadline = time::Instant::now() + Duration::from_secs(5);
            let mut seen = HashSet::new();
            while !expected.iter().all(|topic| seen.contains(*topic)) {
                let topic = time::timeout_at(deadline, self.subscriptions.recv())
                    .await
                    .expect("timed out waiting for real MQTT subscriptions")
                    .expect("local broker task ended");
                seen.insert(topic);
            }
        }

        async fn wait_for_publication(&mut self, expected: &str) {
            let deadline = time::Instant::now() + Duration::from_secs(5);
            loop {
                let topic = time::timeout_at(deadline, self.publications.recv())
                    .await
                    .expect("timed out waiting for MQTT publish")
                    .expect("local broker task ended");
                if topic == expected {
                    return;
                }
            }
        }
    }

    impl Drop for LocalBroker {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve_client(
        mut stream: TcpStream,
        subscriptions: tokio::sync::mpsc::UnboundedSender<String>,
        publications: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> io::Result<()> {
        let (header, _) = read_packet(&mut stream).await?;
        if header >> 4 != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected CONNECT",
            ));
        }
        write_packet(&mut stream, 0x20, &[0, 0]).await?;
        loop {
            let (header, body) = match read_packet(&mut stream).await {
                Ok(packet) => packet,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            };
            match header >> 4 {
                8 => {
                    let packet_id = body.get(..2).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "short SUBSCRIBE")
                    })?;
                    let mut offset = 2;
                    let mut topics = Vec::new();
                    while offset < body.len() {
                        let topic_len = read_u16(&body, &mut offset)? as usize;
                        let end = offset.checked_add(topic_len).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "topic length overflow")
                        })?;
                        let topic =
                            std::str::from_utf8(body.get(offset..end).ok_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidData, "short topic filter")
                            })?)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                            .to_owned();
                        offset = end;
                        let qos = *body.get(offset).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "missing requested QoS")
                        })?;
                        offset += 1;
                        subscriptions.send(topic.clone()).ok();
                        topics.push((topic, qos.min(2)));
                    }
                    let mut ack = packet_id.to_vec();
                    ack.extend(topics.iter().map(|(_, qos)| *qos));
                    write_packet(&mut stream, 0x90, &ack).await?;
                    for (topic, _) in topics {
                        if topic == "homeassistant/#" {
                            write_publish(
                                &mut stream,
                                "homeassistant/sensor/boiler/temperature/config",
                                br#"{"name":"Boiler temperature","object_id":"boiler_temp","unique_id":"boiler-temp-unique","state_topic":"fixture/state","device":{"identifiers":["boiler-1"],"connections":[["mac","00:11:22:33:44:55"]],"name":"Boiler","manufacturer":"Northwind","model":"B1","via_device":"gateway-1"}}"#,
                                true,
                            )
                            .await?;
                            write_publish(
                                &mut stream,
                                "homeassistant/switch/boiler/relay/config",
                                br#"{"name":"Boiler relay","object_id":"boiler_relay","unique_id":"boiler-relay-unique","state_topic":"fixture/switch/state","command_topic":"fixture/switch/set","payload_on":"ON","payload_off":"OFF","state_on":"ON","state_off":"OFF"}"#,
                                true,
                            )
                            .await?;
                        }
                        if topic == "fixture/state" {
                            write_publish(&mut stream, "fixture/state", b"21.5", false).await?;
                        }
                        if topic == "fixture/switch/state" {
                            write_publish(&mut stream, "fixture/switch/state", b"OFF", false)
                                .await?;
                        }
                    }
                }
                3 => {
                    let mut offset = 0;
                    let topic_len = read_u16(&body, &mut offset)? as usize;
                    let end = offset.checked_add(topic_len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "topic length overflow")
                    })?;
                    let topic = std::str::from_utf8(body.get(offset..end).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "short published topic")
                    })?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .to_owned();
                    offset = end;
                    let qos = (header >> 1) & 0x03;
                    if qos > 0 {
                        let packet_id = body.get(offset..offset + 2).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "short PUBLISH packet ID")
                        })?;
                        write_packet(&mut stream, 0x40, packet_id).await?;
                    }
                    publications.send(topic).ok();
                }
                12 => write_packet(&mut stream, 0xD0, &[]).await?,
                14 => return Ok(()),
                _ => {}
            }
        }
    }

    async fn read_packet(reader: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
        let mut first = [0; 1];
        reader.read_exact(&mut first).await?;
        let mut multiplier = 1usize;
        let mut remaining = 0usize;
        loop {
            let mut digit = [0; 1];
            reader.read_exact(&mut digit).await?;
            remaining += ((digit[0] & 0x7f) as usize) * multiplier;
            if digit[0] & 0x80 == 0 {
                break;
            }
            multiplier *= 128;
            if multiplier > 128 * 128 * 128 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid MQTT remaining length",
                ));
            }
        }
        let mut body = vec![0; remaining];
        reader.read_exact(&mut body).await?;
        Ok((first[0], body))
    }

    async fn write_packet(writer: &mut TcpStream, header: u8, body: &[u8]) -> io::Result<()> {
        writer.write_all(&[header]).await?;
        let mut remaining = body.len();
        loop {
            let mut digit = (remaining % 128) as u8;
            remaining /= 128;
            if remaining > 0 {
                digit |= 0x80;
            }
            writer.write_all(&[digit]).await?;
            if remaining == 0 {
                break;
            }
        }
        writer.write_all(body).await?;
        writer.flush().await
    }

    async fn write_publish(
        writer: &mut TcpStream,
        topic: &str,
        payload: &[u8],
        retained: bool,
    ) -> io::Result<()> {
        let mut body = Vec::with_capacity(topic.len() + payload.len() + 2);
        body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
        body.extend_from_slice(topic.as_bytes());
        body.extend_from_slice(payload);
        write_packet(writer, 0x30 | u8::from(retained), &body).await
    }

    fn read_u16(body: &[u8], offset: &mut usize) -> io::Result<u16> {
        let bytes = body.get(*offset..*offset + 2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "short MQTT string length")
        })?;
        *offset += 2;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn mqtt_config(address: &str, client_id: &str) -> MqttConfig {
        let (host, port) = address.rsplit_once(':').unwrap();
        serde_json::from_value(json!({
            "host": host,
            "port": port.parse::<u16>().unwrap(),
            "client_id": client_id,
            "discovery_enabled": true
        }))
        .unwrap()
    }

    async fn wait_for_registered_entity(
        registries: &Registries,
        unique_id: &str,
    ) -> hass_registry::EntityEntry {
        time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(entity) = registries
                    .entities()
                    .into_iter()
                    .find(|entity| entity.unique_id == unique_id)
                {
                    return entity;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MQTT entity was not joined to the entity registry")
    }

    async fn wait_for_state(runtime: &Runtime, entity_id: &str) {
        time::timeout(Duration::from_secs(5), async {
            loop {
                if runtime
                    .get_state(entity_id)
                    .is_some_and(|state| state.state == "21.5")
                {
                    return;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MQTT state did not arrive under the registry entity ID");
    }

    #[test]
    fn mqtt_device_identity_uses_mqtt_domain_and_connection_pairs() {
        let registries =
            Registries::new(Runtime::new(Arc::new(hass_core::MemoryStore::default())).unwrap())
                .unwrap();
        let device = json!({
            "identifiers": ["node-17"],
            "connections": [["mac", "00:11:22:33:44:55"]],
            "name": "Boiler"
        });
        let id = register_device(&registries, "entry-1", Some(&device))
            .unwrap()
            .unwrap();
        let registered = registries.device(&id).unwrap();
        assert!(
            registered
                .identifiers
                .contains(&(MQTT_DOMAIN.into(), "node-17".into()))
        );
        assert!(
            registered
                .connections
                .contains(&("mac".into(), "00:11:22:33:44:55".into()))
        );
        assert_eq!(registered.config_entry_id.as_deref(), Some("entry-1"));
    }

    #[test]
    fn invalid_mqtt_config_does_not_become_a_ready_setup() {
        let result = config_from_map(&Map::from_iter([("host".into(), json!(""))]));
        assert!(matches!(result, Err(CoreError::InvalidInput(_))));
    }

    #[test]
    fn mqtt_config_form_does_not_echo_a_saved_password() {
        let config = Map::from_iter([
            ("host".into(), json!("broker.local")),
            ("password".into(), json!("private-secret")),
        ]);
        let schema = mqtt_config_schema(Some(&config));
        assert_eq!(schema["properties"]["password"]["default"], Value::Null);
        assert_eq!(schema["properties"]["clear_password"]["default"], false);
    }

    #[tokio::test]
    async fn yaml_import_joins_real_discovery_and_survives_reload_and_restart() {
        let mut broker = LocalBroker::start().await;
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let mut conflict = EntityRegistration::new("sensor", "fixture", "conflict");
        conflict.suggested_object_id = Some("boiler_temp".into());
        registries.get_or_create_entity(conflict).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let external_entries = entries.clone();
        let host = NativeDrivers::start(
            runtime.clone(),
            entries,
            registries.clone(),
            vec!["automation/+/event".into()],
            Some(mqtt_config(&broker.address, "driver-host-lifecycle")),
        )
        .await
        .unwrap();

        broker
            .wait_for_subscriptions(&["homeassistant/#", "fixture/state", "automation/+/event"])
            .await;
        let entry = external_entries
            .list_entries(Some(MQTT_DOMAIN))
            .pop()
            .unwrap();
        assert_eq!(entry.source, "import");
        assert_eq!(entry.state, ConfigEntryState::Loaded);
        let mut entity = wait_for_registered_entity(&registries, "boiler-temp-unique").await;
        assert_eq!(
            entity.config_entry_id.as_deref(),
            Some(entry.entry_id.as_str())
        );
        assert_ne!(entity.entity_id, "sensor.boiler_temp");
        wait_for_state(&runtime, &entity.entity_id).await;
        assert!(runtime.get_state("sensor.boiler_temp").is_none());
        let device_id = entity.device_id.as_deref().unwrap();
        let device = registries.device(device_id).unwrap();
        assert_eq!(
            device.config_entry_id.as_deref(),
            Some(entry.entry_id.as_str())
        );
        assert!(
            device
                .identifiers
                .contains(&(MQTT_DOMAIN.into(), "boiler-1".into()))
        );
        assert!(registries.devices().iter().any(|parent| {
            parent
                .identifiers
                .contains(&(MQTT_DOMAIN.into(), "gateway-1".into()))
                && device.via_device_id.as_deref() == Some(parent.id.as_str())
        }));

        let old_entity_id = entity.entity_id.clone();
        entity = registries
            .update_entity(
                &entity.entity_id,
                EntityUpdate {
                    new_entity_id: Some("sensor.boiler_temp_manual_name".into()),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_for_state(&runtime, &entity.entity_id).await;
        assert!(runtime.get_state(&old_entity_id).is_none());

        let start = tokio::task::spawn_blocking({
            let entries = external_entries.clone();
            let entry_id = entry.entry_id.clone();
            move || entries.init_options_flow(&entry_id)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(start["type"], "form");
        let flow_id = start["flow_id"].as_str().unwrap().to_owned();
        let option_result = tokio::task::spawn_blocking({
            let entries = external_entries.clone();
            move || entries.submit_flow(&flow_id, Map::from_iter([("qos".into(), json!(1))]))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(option_result["type"], "create_entry");
        assert_eq!(
            external_entries.get_entry(&entry.entry_id).unwrap().state,
            ConfigEntryState::Loaded
        );

        let unloaded = tokio::task::spawn_blocking({
            let entries = external_entries.clone();
            let entry_id = entry.entry_id.clone();
            move || entries.unload_entry(&entry_id)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(unloaded.state, ConfigEntryState::NotLoaded);
        assert!(runtime.services().as_object().unwrap().is_empty());
        let reloaded = tokio::task::spawn_blocking({
            let entries = external_entries.clone();
            let entry_id = entry.entry_id.clone();
            move || entries.setup_entry(&entry_id)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reloaded.state, ConfigEntryState::Loaded);
        assert!(!runtime.services().as_object().unwrap().is_empty());
        host.shutdown().await.unwrap();
        assert!(runtime.services().as_object().unwrap().is_empty());

        let restarted_entries = ConfigEntries::new(runtime.clone()).unwrap();
        let restarted_registries = Registries::new(runtime.clone()).unwrap();
        let restarted_host = NativeDrivers::start(
            runtime.clone(),
            restarted_entries.clone(),
            restarted_registries.clone(),
            vec!["automation/+/event".into()],
            None,
        )
        .await
        .unwrap();
        let restored = restarted_entries
            .list_entries(Some(MQTT_DOMAIN))
            .pop()
            .unwrap();
        assert_eq!(restored.entry_id, entry.entry_id);
        assert_eq!(restored.state, ConfigEntryState::Loaded);
        let restored_entity =
            wait_for_registered_entity(&restarted_registries, "boiler-temp-unique").await;
        assert_eq!(restored_entity.entity_id, entity.entity_id);
        assert_eq!(
            restored_entity.config_entry_id.as_deref(),
            Some(entry.entry_id.as_str())
        );
        restarted_host.shutdown().await.unwrap();
        assert!(runtime.services().as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mqtt_entity_services_coexist_with_and_remove_only_their_owner() {
        let mut broker = LocalBroker::start().await;
        let runtime = runtime();
        let template_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&template_calls);
        runtime
            .register_entity_service(
                "template-fixture",
                "switch",
                "turn_on",
                json!({"description": "template switch"}),
                Arc::new(|entity_id| entity_id == "switch.template_relay"),
                Arc::new(move |_runtime, call| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();

        let integration = MqttIntegration::start(
            runtime.clone(),
            mqtt_config(&broker.address, "driver-host-services"),
        )
        .await
        .unwrap();
        broker
            .wait_for_subscriptions(&["homeassistant/#", "fixture/switch/state"])
            .await;
        time::timeout(Duration::from_secs(5), async {
            loop {
                if runtime
                    .states()
                    .iter()
                    .any(|state| state.entity_id == "switch.boiler_relay")
                {
                    return;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MQTT switch entity was not discovered");

        let call = |entity_id: &str| ServiceCall {
            domain: "switch".into(),
            service: "turn_on".into(),
            data: Map::new(),
            target: Map::from_iter([("entity_id".into(), json!(entity_id))]),
            context: Context::default(),
        };
        runtime
            .call_service_async(call("switch.template_relay"))
            .await
            .unwrap();
        assert_eq!(template_calls.load(Ordering::SeqCst), 1);
        runtime
            .call_service_async(call("switch.boiler_relay"))
            .await
            .unwrap();
        broker.wait_for_publication("fixture/switch/set").await;

        integration.shutdown().await.unwrap();
        assert!(runtime.services()["switch"].get("turn_on").is_some());
        runtime
            .call_service_async(call("switch.template_relay"))
            .await
            .unwrap();
        assert_eq!(template_calls.load(Ordering::SeqCst), 2);
        assert!(
            runtime
                .call_service_async(call("switch.boiler_relay"))
                .await
                .is_err()
        );
        runtime
            .unregister_entity_services("template-fixture")
            .unwrap();
        assert!(runtime.services().as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_broker_setup_is_retryable_and_only_loads_after_connack() {
        let waiting = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = waiting.local_addr().unwrap().port();
        let address = format!("127.0.0.1:{port}");
        let runtime = runtime();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let host = NativeDrivers::start(
            runtime.clone(),
            entries.clone(),
            registries,
            Vec::new(),
            Some(mqtt_config(&address, "driver-host-retry")),
        )
        .await
        .unwrap();
        let pending = entries.list_entries(Some(MQTT_DOMAIN)).pop().unwrap();
        assert_eq!(pending.state, ConfigEntryState::SetupRetry);
        assert!(pending.retry_at.is_some());
        drop(waiting);

        let mut broker = LocalBroker::start_on(port).await;
        time::timeout(Duration::from_secs(20), async {
            loop {
                if entries
                    .get_entry(&pending.entry_id)
                    .is_some_and(|entry| entry.state == ConfigEntryState::Loaded)
                {
                    break;
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("retry scheduler did not load MQTT after a real CONNACK");
        broker.wait_for_subscriptions(&["homeassistant/#"]).await;
        host.shutdown().await.unwrap();
    }
}
