//! Native MQTT integration for Home Assistant style discovery and entities.
//!
//! Discovery covers the supported MQTT entity platforms. Reported state changes
//! arrive only from MQTT state/availability messages. Commands use a bounded
//! broker queue; optimism must be explicitly enabled in configuration.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use hass_button::{
    ButtonDescriptor, ButtonDeviceClass, ButtonPressFuture, ButtonPressHandler, ButtonPressOutcome,
    ButtonPressRequest, ButtonSnapshot, ButtonTimestamp, execute_button_press_at,
};
use hass_climate::{
    ClimateCommand, ClimateDescriptor, ClimateEffect, ClimateFeature, ClimateFeatures,
    ClimateService, ClimateSnapshot, ClimateTemperatureUnit, ClimateUnitContext, HvacAction,
    HvacMode,
};
use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult, State};
use hass_cover::{
    CoverAction, CoverCommand, CoverDescriptor, CoverDeviceClass, CoverFeatures, CoverSnapshot,
    CoverToggleState,
};
use hass_entity::{EntityHandle, EntityLifecycle, EntityPublication, EntitySnapshot, EntityStatus};
use hass_fan::{FanCommand, FanDescriptor, FanEffect, FanFeatures, FanSnapshot, FanState};
use hass_light::{
    LightAction, LightColorMode, LightCommand, LightDescriptor, LightFeature, LightFeatures,
    LightNormalizationContext, LightProfileCatalog, LightServiceDiagnostic, LightSnapshot,
    NormalizedLightCommand,
};
use hass_registry::{DeviceRegistration, Registries};
use hass_switch::{SwitchCommand, SwitchDescriptor, SwitchDeviceClass, SwitchSnapshot};
use hass_template::TemplateEngine;
use hass_units::{UnitFamily, UnitSystemKind, convert};
use regex::Regex;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{RwLock as AsyncRwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const CLIENT_QUEUE_CAPACITY: usize = 128;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const PUBLISH_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERED_ENTITIES: usize = 1024;
const MAX_TARGETS_PER_SERVICE: usize = 64;

fn default_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "homeassistant-rust".into()
}
fn default_keep_alive_secs() -> u64 {
    30
}
fn default_discovery_prefix() -> String {
    "homeassistant".into()
}
fn default_max_payload_bytes() -> usize {
    65_536
}
fn default_true() -> bool {
    true
}

/// Connection and resource settings for one MQTT broker.
///
/// Unknown fields are rejected. `password` is redacted from `Debug` output.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    /// Broker DNS name or IP address.
    pub host: String,
    /// Broker TCP port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// MQTT client identifier.
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Optional MQTT username.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional MQTT password. Never included in `Debug` output.
    #[serde(default)]
    pub password: Option<String>,
    /// Use certificate-validated TLS with the platform root store.
    #[serde(default)]
    pub tls: bool,
    /// MQTT keep-alive interval in seconds.
    #[serde(default = "default_keep_alive_secs")]
    pub keep_alive_secs: u64,
    /// Default QoS for subscriptions and commands (0, 1, or 2).
    #[serde(default)]
    pub qos: u8,
    /// Home Assistant discovery topic prefix.
    #[serde(default = "default_discovery_prefix")]
    pub discovery_prefix: String,
    /// Subscribe to discovery topics under `discovery_prefix`.
    #[serde(default = "default_true")]
    pub discovery_enabled: bool,
    /// Allow discovery entities to update runtime state after publishing a command.
    /// Defaults to false; per-entity `optimistic` can explicitly enable it too.
    #[serde(default)]
    pub optimistic: bool,
    /// Maximum MQTT packet and payload size accepted by this integration.
    #[serde(default = "default_max_payload_bytes")]
    pub max_payload_bytes: usize,
}

impl fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("tls", &self.tls)
            .field("keep_alive_secs", &self.keep_alive_secs)
            .field("qos", &self.qos)
            .field("discovery_prefix", &self.discovery_prefix)
            .field("discovery_enabled", &self.discovery_enabled)
            .field("optimistic", &self.optimistic)
            .field("max_payload_bytes", &self.max_payload_bytes)
            .finish()
    }
}

impl MqttConfig {
    /// Validate settings without connecting to the broker. Safe for config-check paths.
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::InvalidInput("mqtt host cannot be empty".into()));
        }
        if self.port == 0 {
            return Err(Error::InvalidInput(
                "mqtt port must be greater than zero".into(),
            ));
        }
        if self.client_id.trim().is_empty() || self.client_id.len() > 128 {
            return Err(Error::InvalidInput(
                "mqtt client_id must contain 1 to 128 bytes".into(),
            ));
        }
        if !(5..=3600).contains(&self.keep_alive_secs) {
            return Err(Error::InvalidInput(
                "mqtt keep_alive_secs must be between 5 and 3600".into(),
            ));
        }
        qos(self.qos)?;
        // The core signal bus has a 1 MiB serialized data cap. `payload_bytes`
        // is represented as an integer array and the UTF-8 payload is also
        // retained, so a 64 KiB wire payload remains well inside that bound.
        if !(256..=65_536).contains(&self.max_payload_bytes) {
            return Err(Error::InvalidInput(
                "mqtt max_payload_bytes must be between 256 and 65536".into(),
            ));
        }
        validate_topic(&self.discovery_prefix, "discovery_prefix")?;
        if self.username.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidInput(
                "mqtt username cannot be an empty string".into(),
            ));
        }
        if self.password.is_some() && self.username.is_none() {
            return Err(Error::InvalidInput(
                "mqtt password requires a username".into(),
            ));
        }
        Ok(())
    }
}

/// Identity information for one active MQTT discovery entity.
///
/// The root integration can use this readback to reconcile native
/// entity/device registry records without reaching into this crate's state.
/// Switches started through the shared lifecycle already register their own
/// descriptor, but remain visible here with their current registry-assigned ID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MqttEntityMetadata {
    pub entity_id: String,
    pub platform: String,
    pub unique_id: String,
    pub discovery_topic: String,
    pub name: String,
    pub device: Option<Value>,
    /// Whether the shared application entity lifecycle owns this MQTT entity.
    #[serde(default)]
    pub entity_lifecycle_managed: bool,
}

/// Running MQTT connection and entity/service bridge.
pub struct MqttIntegration {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
    subscription_tx: mpsc::Sender<SubscriptionRequest>,
    command_admission: Arc<AsyncRwLock<bool>>,
    service_owner: Option<String>,
    runtime: Runtime,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
}

#[derive(Clone)]
struct SharedEntityLifecycleContext {
    lifecycle: EntityLifecycle,
    registries: Registries,
    entry_id: String,
    provider_id: String,
}

struct EventLoopTaskGuard {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl EventLoopTaskGuard {
    async fn stop_and_join(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        tokio::spawn(join_event_loop_task(task))
            .await
            .map_err(|error| {
                Error::Service(format!("mqtt event loop reaper task failed: {error}"))
            })?
    }

    fn into_parts(mut self) -> (oneshot::Sender<()>, JoinHandle<Result<()>>) {
        (
            self.shutdown
                .take()
                .expect("event loop guard owns its shutdown signal"),
            self.task
                .take()
                .expect("event loop guard owns its worker task"),
        )
    }
}

impl Drop for EventLoopTaskGuard {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = join_event_loop_task(task).await;
                });
            } else {
                task.abort();
            }
        }
    }
}

async fn join_event_loop_task(task: JoinHandle<Result<()>>) -> Result<()> {
    match task.await {
        Ok(result) => result,
        Err(error) => Err(Error::Service(format!(
            "mqtt event loop task failed: {error}"
        ))),
    }
}

impl MqttIntegration {
    /// Connect to the broker and start processing incoming and outgoing MQTT.
    ///
    /// Startup waits at most five seconds for the first successful CONNACK. A
    /// connection that cannot be established is returned as an error instead
    /// of leaving a detached background retry loop behind.
    pub async fn start(runtime: Runtime, config: MqttConfig) -> Result<Self> {
        Self::start_inner(
            runtime,
            config,
            None,
            Arc::new(LightProfileCatalog::built_in()),
        )
        .await
    }

    /// Start MQTT using the application's shared entity lifecycle for native
    /// switch, button, light, cover, fan, and climate discoveries. Other MQTT
    /// platforms keep the standalone path.
    pub async fn start_with_entity_lifecycle(
        runtime: Runtime,
        config: MqttConfig,
        lifecycle: EntityLifecycle,
        registries: Registries,
        entry_id: &str,
    ) -> Result<Self> {
        if entry_id.trim().is_empty() {
            return Err(Error::InvalidInput(
                "MQTT entity lifecycle requires a config entry ID".into(),
            ));
        }
        let entry_id = entry_id.to_owned();
        Self::start_inner(
            runtime,
            config,
            Some(SharedEntityLifecycleContext {
                lifecycle,
                registries,
                provider_id: format!("mqtt:{entry_id}"),
                entry_id,
            }),
            Arc::new(LightProfileCatalog::built_in()),
        )
        .await
    }

    /// Start MQTT with the shared entity lifecycle and a host-loaded profile
    /// catalog for native light service normalization.
    pub async fn start_with_entity_lifecycle_and_profiles(
        runtime: Runtime,
        config: MqttConfig,
        lifecycle: EntityLifecycle,
        registries: Registries,
        entry_id: &str,
        profiles: Arc<LightProfileCatalog>,
    ) -> Result<Self> {
        if entry_id.trim().is_empty() {
            return Err(Error::InvalidInput(
                "MQTT entity lifecycle requires a config entry ID".into(),
            ));
        }
        let entry_id = entry_id.to_owned();
        Self::start_inner(
            runtime,
            config,
            Some(SharedEntityLifecycleContext {
                lifecycle,
                registries,
                provider_id: format!("mqtt:{entry_id}"),
                entry_id,
            }),
            profiles,
        )
        .await
    }

    async fn start_inner(
        runtime: Runtime,
        config: MqttConfig,
        entity_lifecycle: Option<SharedEntityLifecycleContext>,
        profiles: Arc<LightProfileCatalog>,
    ) -> Result<Self> {
        config.validate()?;

        let mut options =
            MqttOptions::new(config.client_id.clone(), config.host.clone(), config.port);
        options.set_keep_alive(Duration::from_secs(config.keep_alive_secs));
        options.set_clean_session(true);
        if let Some(username) = config.username.as_deref() {
            options.set_credentials(username, config.password.as_deref().unwrap_or_default());
        }
        if config.tls {
            options.set_transport(Transport::tls_with_default_config());
        }
        // Leave room for the topic name, packet identifier, and MQTT framing
        // while still bounding retained payloads before deserialization.
        let max_packet_size = config.max_payload_bytes.saturating_add(1100);
        options.set_max_packet_size(max_packet_size, max_packet_size);

        let (client, event_loop) = AsyncClient::new(options, CLIENT_QUEUE_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(CLIENT_QUEUE_CAPACITY);
        let command_admission = Arc::new(AsyncRwLock::new(true));
        let (subscription_tx, subscription_rx) = mpsc::channel(32);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let entities = Arc::new(RwLock::new(BTreeMap::new()));
        let metadata = Arc::new(RwLock::new(BTreeMap::new()));
        let additional_subscriptions = Arc::new(RwLock::new(BTreeMap::new()));
        let templates = TemplateEngine::new(runtime.clone());
        let core_events = runtime.subscribe();

        let task_entities = Arc::clone(&entities);
        let task_metadata = Arc::clone(&metadata);
        let task = tokio::spawn(run_event_loop(
            runtime.clone(),
            config.clone(),
            templates.clone(),
            client.clone(),
            event_loop,
            command_rx,
            subscription_rx,
            core_events,
            stop_rx,
            ready_tx,
            task_entities,
            task_metadata,
            Arc::clone(&additional_subscriptions),
            entity_lifecycle,
            Arc::clone(&command_admission),
        ));
        let mut worker = EventLoopTaskGuard {
            shutdown: Some(stop_tx),
            task: Some(task),
        };

        match tokio::time::timeout(STARTUP_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                let _ = worker.stop_and_join().await;
                return Err(Error::Service(
                    "mqtt connection task ended before connecting".into(),
                ));
            }
            Err(_) => {
                let _ = worker.stop_and_join().await;
                return Err(Error::Service(format!(
                    "mqtt broker {}:{} did not accept a connection within {} seconds",
                    config.host,
                    config.port,
                    STARTUP_TIMEOUT.as_secs()
                )));
            }
        }

        let service_owner = uuid::Uuid::new_v4().to_string();
        if let Err(error) = register_services(
            &runtime,
            &service_owner,
            command_tx,
            Arc::clone(&entities),
            templates,
            Arc::clone(&command_admission),
            profiles,
        ) {
            if let Err(cleanup_error) = runtime.unregister_entity_services(&service_owner) {
                warn!(owner = %service_owner, error = %cleanup_error, "failed to clean up partial MQTT service registration");
            }
            let _ = worker.stop_and_join().await;
            return Err(error);
        }

        let (shutdown, task) = worker.into_parts();
        let loaded_components = runtime.loaded_components();
        let integration = Self {
            shutdown: Some(shutdown),
            task: Some(task),
            subscription_tx,
            command_admission,
            service_owner: Some(service_owner),
            runtime,
            entities,
            metadata,
        };
        loaded_components.record_successful_component("mqtt");
        Ok(integration)
    }

    /// Return currently discovered entity identities in stable order.
    pub fn entity_metadata(&self) -> Vec<MqttEntityMetadata> {
        let entities = self
            .entities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut metadata = self
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|item| {
                let mut item = item.clone();
                if let Some(entity) = entities.get(&item.discovery_topic)
                    && let Some(handle) = entity_lifecycle_handle(entity)
                {
                    let status = handle.status();
                    if matches!(
                        &status,
                        EntityStatus::Removed { .. }
                            | EntityStatus::Released { .. }
                            | EntityStatus::Shutdown { .. }
                    ) {
                        return None;
                    }
                    item.entity_id = status.entity_id().to_owned();
                }
                item.entity_lifecycle_managed = entities
                    .get(&item.discovery_topic)
                    .is_some_and(|entity| entity_lifecycle_handle(entity).is_some());
                Some(item)
            })
            .collect::<Vec<_>>();
        metadata.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        metadata
    }

    /// Rebind a standalone MQTT entity to a registry-assigned entity ID.
    ///
    /// Shared-lifecycle entities resolve their ID from the registry handle and
    /// cannot be rebound through this compatibility method. The operation
    /// never publishes an MQTT command or changes any topic.
    pub fn rebind_entity_id(&self, old_entity_id: &str, new_entity_id: &str) -> Result<()> {
        if old_entity_id == new_entity_id {
            return Ok(());
        }
        let mut entities = self
            .entities
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(discovery_topic) = entities
            .iter()
            .find(|(_, entity)| mqtt_entity_id(entity) == old_entity_id)
            .map(|(topic, _)| topic.clone())
        else {
            return Err(Error::NotFound(format!(
                "MQTT entity {old_entity_id} is no longer active"
            )));
        };
        if entities
            .values()
            .any(|other| mqtt_entity_id(other) == new_entity_id)
        {
            return Err(Error::InvalidInput(format!(
                "MQTT entity ID {new_entity_id} is already in use"
            )));
        }

        let entity = entities
            .get_mut(&discovery_topic)
            .expect("discovery topic was found above");
        if let Some(handle) = entity_lifecycle_handle(entity) {
            if handle.status().entity_id() == new_entity_id {
                return Ok(());
            }
            return Err(Error::InvalidInput(
                "MQTT entity IDs are managed by the shared entity lifecycle".into(),
            ));
        }

        let previous_state = self.runtime.get_state(old_entity_id);
        if self.runtime.get_state(new_entity_id).is_none()
            && let Some(state) = previous_state.as_ref()
        {
            self.runtime.set_state(
                new_entity_id,
                &state.state,
                state.attributes.clone(),
                state.context.clone(),
                false,
            )?;
        }
        if previous_state.is_some() {
            self.runtime
                .remove_state(old_entity_id, Context::default())?;
        }
        entity.metadata.entity_id = new_entity_id.to_owned();
        drop(entities);

        let mut metadata = self
            .metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut item) = metadata.remove(old_entity_id) {
            item.entity_id = new_entity_id.to_owned();
            metadata.insert(new_entity_id.to_owned(), item);
        }
        Ok(())
    }

    /// Add an MQTT topic filter and preserve it for reconnects.
    pub async fn subscribe_topic(&self, topic: &str, qos_value: u8) -> Result<()> {
        validate_filter(topic)?;
        qos(qos_value)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.subscription_tx
            .send(SubscriptionRequest {
                topic: topic.to_owned(),
                qos: qos_value,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Service("MQTT event loop is shutting down".into()))?;
        reply_rx
            .await
            .map_err(|_| Error::Service("MQTT event loop ended before subscribing".into()))?
    }

    /// Stop MQTT processing, send DISCONNECT, and join the event-loop task.
    pub async fn shutdown(mut self) -> Result<()> {
        {
            let mut accepting = self.command_admission.write().await;
            *accepting = false;
        }
        let mut first_error = None;
        if let Some(owner) = self.service_owner.take()
            && let Err(error) = self.runtime.unregister_entity_services(&owner)
        {
            first_error = Some(error);
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            // Keep the join operation alive if the caller cancels this future.
            // The worker still receives shutdown, drains its reply channels,
            // and is joined by this owned reaper task.
            match tokio::spawn(join_event_loop_task(task)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => first_error = Some(error),
                Err(error) => {
                    first_error = Some(Error::Service(format!(
                        "mqtt event loop task failed: {error}"
                    )));
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for MqttIntegration {
    fn drop(&mut self) {
        // Closing the oneshot also wakes the task if callers drop the owner
        // without explicitly awaiting shutdown. The explicit method remains
        // necessary when the caller needs to observe completion/errors.
        if let Ok(mut accepting) = self.command_admission.try_write() {
            *accepting = false;
        }
        if let Some(owner) = self.service_owner.take()
            && let Err(error) = self.runtime.unregister_entity_services(&owner)
        {
            warn!(owner, %error, "failed to unregister MQTT entity services during drop");
        }
        self.shutdown.take();
    }
}

#[derive(Clone, Debug)]
struct AvailabilityTopic {
    topic: String,
    payload_available: String,
    payload_not_available: String,
    value_template: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum AvailabilityMode {
    All,
    Any,
}

#[derive(Clone, Debug)]
struct DiscoveredEntity {
    metadata: MqttEntityMetadata,
    shared_entity: Option<SharedMqttEntity>,
    climate: Option<MqttClimateEntity>,
    switch_device_class: Option<SwitchDeviceClass>,
    button_device_class: Option<ButtonDeviceClass>,
    button_last_value: Arc<Mutex<Option<LastValue>>>,
    state_topic: Option<String>,
    extra_state_topics: BTreeMap<String, String>,
    extra_command_topics: BTreeMap<String, String>,
    json_attributes_topic: Option<String>,
    availability: Vec<AvailabilityTopic>,
    availability_mode: AvailabilityMode,
    command_topic: Option<String>,
    payload_on: String,
    payload_off: String,
    state_on: String,
    state_off: String,
    value_template: Option<String>,
    attributes_template: Option<String>,
    command_template: Option<String>,
    optimistic: bool,
    retain: bool,
    qos: u8,
    max_payload_bytes: usize,
    attributes: Map<String, Value>,
    platform_config: Map<String, Value>,
    text_pattern: Option<Regex>,
}

#[derive(Clone, Debug)]
struct MqttClimateEntity {
    descriptor: ClimateDescriptor,
    state: Arc<Mutex<MqttClimateState>>,
    state_topics: BTreeMap<String, Vec<ClimateStateField>>,
}

#[derive(Clone, Copy, Debug)]
enum ClimateStateField {
    Action,
    CurrentHumidity,
    CurrentTemperature,
    TargetHumidity,
    HvacMode,
    FanMode,
    SwingHorizontalMode,
    SwingMode,
    PresetMode,
    TargetTemperature,
    TargetTemperatureLow,
    TargetTemperatureHigh,
}

#[derive(Clone, Debug)]
struct MqttClimateState {
    snapshot: ClimateSnapshot,
    parked_target_temperature: Option<f64>,
    parked_target_temperature_range: (Option<f64>, Option<f64>),
    single_and_range_setpoints: bool,
}

#[derive(Clone, Debug)]
struct ClimateStateUpdate {
    mutations: Vec<ClimateStateMutation>,
}

#[derive(Clone, Debug)]
enum ClimateStateMutation {
    HvacMode(HvacMode),
    HvacModeWithoutSetpointSwap(HvacMode),
    TargetTemperature(f64),
    TargetTemperatureLow(f64),
    TargetTemperatureHigh(f64),
    TargetHumidity(f64),
    FanMode(String),
    PresetMode(String),
    SwingMode(String),
    SwingHorizontalMode(String),
}

#[derive(Clone, Debug)]
enum SharedMqttEntity {
    Switch(EntityHandle),
    Button {
        handle: EntityHandle,
        descriptor: ButtonDescriptor,
    },
    Light {
        handle: EntityHandle,
        descriptor: LightDescriptor,
    },
    Cover {
        handle: EntityHandle,
        descriptor: CoverDescriptor,
        toggle_state: Arc<Mutex<CoverToggleState>>,
    },
    Fan {
        handle: EntityHandle,
        descriptor: FanDescriptor,
    },
    Climate {
        handle: EntityHandle,
        descriptor: ClimateDescriptor,
    },
}

impl SharedMqttEntity {
    fn handle(&self) -> &EntityHandle {
        match self {
            Self::Switch(handle)
            | Self::Button { handle, .. }
            | Self::Light { handle, .. }
            | Self::Cover { handle, .. }
            | Self::Fan { handle, .. }
            | Self::Climate { handle, .. } => handle,
        }
    }
}

#[derive(Debug)]
struct Command {
    topic: String,
    payload: Vec<u8>,
    publish: bool,
    qos: QoS,
    retain: bool,
    optimistic: Option<OptimisticState>,
    entity_handle: Option<EntityHandle>,
}

struct CommandRequest {
    command: Command,
    remaining_commands: VecDeque<Command>,
    changed_states: Vec<State>,
    reply: oneshot::Sender<Result<Vec<State>>>,
}

#[derive(Debug)]
struct OptimisticState {
    discovery_topic: String,
    state: Option<String>,
    attributes: Map<String, Value>,
    optimistic_updates: Map<String, Value>,
    context: Context,
    switch_state: bool,
    climate_state: Option<ClimateStateUpdate>,
    entity_handle: Option<EntityHandle>,
}

struct PendingPublish {
    qos: QoS,
    reply: Option<oneshot::Sender<Result<Vec<State>>>>,
    remaining_commands: VecDeque<Command>,
    changed_states: Vec<State>,
    optimistic: Option<OptimisticState>,
    deadline: Option<tokio::time::Instant>,
}

struct SubscriptionRequest {
    topic: String,
    qos: u8,
    reply: oneshot::Sender<Result<()>>,
}

#[derive(Clone, Debug)]
struct LastValue {
    state: String,
    attributes: Map<String, Value>,
    context: Context,
}

struct PublishCompletionContext<'a> {
    runtime: &'a Runtime,
    entities: &'a RwLock<BTreeMap<String, DiscoveredEntity>>,
    connected: bool,
    availability_status: &'a HashMap<(String, String), bool>,
    last_values: &'a mut HashMap<String, LastValue>,
    ready_continuations: &'a mut VecDeque<CommandRequest>,
}

#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    runtime: Runtime,
    config: MqttConfig,
    templates: TemplateEngine,
    client: AsyncClient,
    mut event_loop: rumqttc::EventLoop,
    mut command_rx: mpsc::Receiver<CommandRequest>,
    mut subscription_rx: mpsc::Receiver<SubscriptionRequest>,
    mut core_events: broadcast::Receiver<hass_core::Event>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: oneshot::Sender<()>,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
    additional_subscriptions: Arc<RwLock<BTreeMap<String, u8>>>,
    entity_lifecycle: Option<SharedEntityLifecycleContext>,
    command_admission: Arc<AsyncRwLock<bool>>,
) -> Result<()> {
    let mut connected = false;
    let mut ready_tx = Some(ready_tx);
    let mut subscribed = BTreeMap::new();
    let mut availability_status: HashMap<(String, String), bool> = HashMap::new();
    let mut last_values: HashMap<String, LastValue> = HashMap::new();
    let mut unassigned_publishes = VecDeque::new();
    let mut publishes_by_packet_id = HashMap::new();
    let mut ready_continuations = VecDeque::new();

    loop {
        enum Next {
            Stop,
            Command(Option<CommandRequest>),
            Continuation(Option<CommandRequest>),
            Subscription(Option<SubscriptionRequest>),
            CoreEvent(std::result::Result<hass_core::Event, broadcast::error::RecvError>),
            CompletionDeadline,
            Poll(std::result::Result<Event, rumqttc::ConnectionError>),
        }
        let next_deadline =
            earliest_publish_deadline(&unassigned_publishes, &publishes_by_packet_id);
        let next = tokio::select! {
            biased;
            _ = &mut stop_rx => Next::Stop,
            next = async {
                // Shutdown wins immediately; other ready sources share fair
                // polling so a command backlog cannot starve broker ACKs.
                tokio::select! {
                    continuation = async { ready_continuations.pop_front() }, if !ready_continuations.is_empty() => Next::Continuation(continuation),
                    command = command_rx.recv() => Next::Command(command),
                    subscription = subscription_rx.recv() => Next::Subscription(subscription),
                    event = core_events.recv() => Next::CoreEvent(event),
                    _ = wait_for_publish_deadline(next_deadline) => Next::CompletionDeadline,
                    event = event_loop.poll() => Next::Poll(event),
                }
            } => next,
        };
        refresh_shared_entity_ids(
            &entities,
            &metadata,
            &mut availability_status,
            &mut last_values,
        );

        match next {
            Next::Stop | Next::Command(None) | Next::Subscription(None) => {
                {
                    let mut accepting = command_admission.write().await;
                    *accepting = false;
                }
                command_rx.close();
                subscription_rx.close();
                let shutdown_error = || {
                    Error::Service("MQTT integration shut down before command completion".into())
                };
                while let Ok(request) = command_rx.try_recv() {
                    let _ = request.reply.send(Err(shutdown_error()));
                }
                while let Some(request) = ready_continuations.pop_front() {
                    let _ = request.reply.send(Err(shutdown_error()));
                }
                while let Ok(request) = subscription_rx.try_recv() {
                    let _ = request.reply.send(Err(shutdown_error()));
                }
                fail_pending_publishes(
                    &mut unassigned_publishes,
                    &mut publishes_by_packet_id,
                    shutdown_error(),
                );
                graceful_disconnect(&client, &mut event_loop).await;
                return Ok(());
            }
            Next::Command(Some(request)) => {
                if request.command.publish {
                    queue_publish_command(&client, request, &entities, &mut unassigned_publishes);
                } else {
                    let mut completion = PublishCompletionContext {
                        runtime: &runtime,
                        entities: &entities,
                        connected,
                        availability_status: &availability_status,
                        last_values: &mut last_values,
                        ready_continuations: &mut ready_continuations,
                    };
                    complete_local_command(&mut completion, request);
                }
            }
            Next::Continuation(Some(request)) => {
                if request.command.publish {
                    queue_publish_command(&client, request, &entities, &mut unassigned_publishes);
                } else {
                    let mut completion = PublishCompletionContext {
                        runtime: &runtime,
                        entities: &entities,
                        connected,
                        availability_status: &availability_status,
                        last_values: &mut last_values,
                        ready_continuations: &mut ready_continuations,
                    };
                    complete_local_command(&mut completion, request);
                }
            }
            Next::Continuation(None) => {}
            Next::Subscription(Some(request)) => {
                {
                    let mut desired = additional_subscriptions
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let value = desired.entry(request.topic).or_insert(request.qos);
                    *value = (*value).max(request.qos);
                }
                let result = reconcile_subscriptions(
                    &client,
                    &config,
                    &entities,
                    &additional_subscriptions,
                    &mut subscribed,
                )
                .await;
                if let Err(error) = &result {
                    warn!(error = %error, "failed to add MQTT topic subscription");
                }
                let _ = request.reply.send(result);
            }
            Next::CoreEvent(Ok(event)) if event.event_type == "core_config_updated" => {
                reproject_climate_entities(
                    &runtime,
                    &entities,
                    connected,
                    &availability_status,
                    &last_values,
                );
            }
            Next::CoreEvent(Err(broadcast::error::RecvError::Lagged(_))) => {
                reproject_climate_entities(
                    &runtime,
                    &entities,
                    connected,
                    &availability_status,
                    &last_values,
                );
            }
            Next::CoreEvent(Ok(_)) => {}
            Next::CoreEvent(Err(broadcast::error::RecvError::Closed)) => {
                debug!("runtime event stream closed while MQTT integration was active");
                return Ok(());
            }
            Next::CompletionDeadline => {
                let mut completion = PublishCompletionContext {
                    runtime: &runtime,
                    entities: &entities,
                    connected,
                    availability_status: &availability_status,
                    last_values: &mut last_values,
                    ready_continuations: &mut ready_continuations,
                };
                complete_expired_publishes(
                    &mut completion,
                    &mut unassigned_publishes,
                    &mut publishes_by_packet_id,
                    tokio::time::Instant::now(),
                );
            }
            Next::Poll(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                connected = true;
                subscribed.clear();
                if let Err(error) = reconcile_subscriptions(
                    &client,
                    &config,
                    &entities,
                    &additional_subscriptions,
                    &mut subscribed,
                )
                .await
                {
                    warn!(error = %error, "failed to subscribe to MQTT topics");
                }
                update_all_availability(
                    &runtime,
                    &entities,
                    connected,
                    &availability_status,
                    &last_values,
                );
                if let Some(sender) = ready_tx.take() {
                    let _ = sender.send(());
                }
            }
            Next::Poll(Ok(Event::Incoming(Packet::Publish(message)))) => {
                if message.payload.len() > config.max_payload_bytes {
                    warn!(topic = %message.topic, bytes = message.payload.len(), "discarding oversized MQTT payload");
                    continue;
                }
                handle_message(
                    &runtime,
                    &config,
                    &templates,
                    &client,
                    &entities,
                    &metadata,
                    &entity_lifecycle,
                    &additional_subscriptions,
                    &mut subscribed,
                    &mut availability_status,
                    &mut last_values,
                    connected,
                    &message.topic,
                    &message.payload,
                    qos_to_u8(message.qos),
                    message.retain,
                )
                .await;
            }
            Next::Poll(Ok(Event::Outgoing(rumqttc::Outgoing::Publish(packet_id)))) => {
                if packet_id != 0 && publishes_by_packet_id.contains_key(&packet_id) {
                    // rumqttc may emit this again when retransmitting an
                    // unacknowledged publish after reconnect. Keep its original
                    // completion attached to the packet ID.
                    continue;
                }
                let Some(mut pending) = unassigned_publishes.pop_front() else {
                    continue;
                };
                match (pending.qos, packet_id) {
                    (QoS::AtMostOnce, 0) => {
                        let mut completion = PublishCompletionContext {
                            runtime: &runtime,
                            entities: &entities,
                            connected,
                            availability_status: &availability_status,
                            last_values: &mut last_values,
                            ready_continuations: &mut ready_continuations,
                        };
                        complete_publish(&mut completion, &mut pending);
                    }
                    (QoS::AtLeastOnce | QoS::ExactlyOnce, id) if id != 0 => {
                        publishes_by_packet_id.insert(id, pending);
                    }
                    _ => {
                        warn!(
                            packet_id,
                            "rumqttc emitted a publish with an unexpected packet ID"
                        );
                        unassigned_publishes.push_front(pending);
                    }
                }
            }
            Next::Poll(Ok(Event::Incoming(Packet::PubAck(ack)))) => {
                if publishes_by_packet_id
                    .get(&ack.pkid)
                    .is_some_and(|pending| pending.qos == QoS::AtLeastOnce)
                    && let Some(mut pending) = publishes_by_packet_id.remove(&ack.pkid)
                {
                    let mut completion = PublishCompletionContext {
                        runtime: &runtime,
                        entities: &entities,
                        connected,
                        availability_status: &availability_status,
                        last_values: &mut last_values,
                        ready_continuations: &mut ready_continuations,
                    };
                    complete_publish(&mut completion, &mut pending);
                }
            }
            Next::Poll(Ok(Event::Incoming(Packet::PubComp(comp)))) => {
                if publishes_by_packet_id
                    .get(&comp.pkid)
                    .is_some_and(|pending| pending.qos == QoS::ExactlyOnce)
                    && let Some(mut pending) = publishes_by_packet_id.remove(&comp.pkid)
                {
                    let mut completion = PublishCompletionContext {
                        runtime: &runtime,
                        entities: &entities,
                        connected,
                        availability_status: &availability_status,
                        last_values: &mut last_values,
                        ready_continuations: &mut ready_continuations,
                    };
                    complete_publish(&mut completion, &mut pending);
                }
            }
            Next::Poll(Ok(_)) => {}
            Next::Poll(Err(error)) => {
                if connected {
                    connected = false;
                    availability_status
                        .values_mut()
                        .for_each(|value| *value = false);
                    update_all_availability(
                        &runtime,
                        &entities,
                        false,
                        &availability_status,
                        &last_values,
                    );
                }
                debug!(error = %error, "MQTT connection lost; rumqttc will retry");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn earliest_publish_deadline(
    unassigned: &VecDeque<PendingPublish>,
    awaiting_ack: &HashMap<u16, PendingPublish>,
) -> Option<tokio::time::Instant> {
    unassigned
        .iter()
        .chain(awaiting_ack.values())
        .filter_map(|pending| pending.deadline)
        .min()
}

async fn wait_for_publish_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn apply_climate_optimistic_update(
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    optimistic: Option<&OptimisticState>,
) {
    let Some(optimistic) = optimistic else {
        return;
    };
    let Some(update) = optimistic.climate_state.as_ref() else {
        return;
    };
    let entity = entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .find(|entity| entity.metadata.discovery_topic == optimistic.discovery_topic)
        .filter(|entity| mqtt_entity_is_active(entity))
        .cloned();
    let Some(entity) = entity else {
        return;
    };
    let Some(climate) = entity.climate.as_ref() else {
        return;
    };
    let mut current = climate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    apply_climate_state_update(&entity, &mut current, update, &optimistic.context);
}

fn queue_publish_command(
    client: &AsyncClient,
    request: CommandRequest,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    pending: &mut VecDeque<PendingPublish>,
) {
    let CommandRequest {
        mut command,
        remaining_commands,
        changed_states,
        reply,
    } = request;
    if command
        .entity_handle
        .as_ref()
        .is_some_and(|handle| !handle.status().is_active())
    {
        let _ = reply.send(Err(Error::Service(
            "MQTT entity is no longer active".into(),
        )));
        return;
    }
    let qos = command.qos;
    let optimistic = command.optimistic.take();
    match client.try_publish(command.topic, qos, command.retain, command.payload) {
        Ok(()) => {
            apply_climate_optimistic_update(entities, optimistic.as_ref());
            pending.push_back(PendingPublish {
                qos,
                reply: Some(reply),
                remaining_commands,
                changed_states,
                optimistic,
                deadline: Some(tokio::time::Instant::now() + PUBLISH_COMPLETION_TIMEOUT),
            });
        }
        Err(error) => {
            let _ = reply.send(Err(Error::Service(format!(
                "failed to queue MQTT command for publication: {error}"
            ))));
        }
    }
}

fn complete_local_command(completion: &mut PublishCompletionContext<'_>, request: CommandRequest) {
    if request
        .command
        .entity_handle
        .as_ref()
        .is_some_and(|handle| !handle.status().is_active())
    {
        let _ = request.reply.send(Err(Error::Service(
            "MQTT entity is no longer active".into(),
        )));
        return;
    }
    apply_climate_optimistic_update(completion.entities, request.command.optimistic.as_ref());
    let CommandRequest {
        command,
        remaining_commands,
        changed_states,
        reply,
    } = request;
    let mut pending = PendingPublish {
        qos: command.qos,
        reply: Some(reply),
        remaining_commands,
        changed_states,
        optimistic: command.optimistic,
        deadline: None,
    };
    complete_publish(completion, &mut pending);
}

fn complete_expired_publishes(
    completion: &mut PublishCompletionContext<'_>,
    unassigned: &mut VecDeque<PendingPublish>,
    awaiting_ack: &mut HashMap<u16, PendingPublish>,
    now: tokio::time::Instant,
) {
    for pending in unassigned.iter_mut().chain(awaiting_ack.values_mut()) {
        if pending.deadline.is_some_and(|deadline| deadline <= now) {
            // The pinned Paho callback timeout logs but returns normally. Keep
            // the request's FIFO/packet-ID tombstone until rumqttc surfaces or
            // acknowledges the packet, while resolving the service as success.
            pending.deadline = None;
            warn!(qos = ?pending.qos, "MQTT publish callback timed out; completing normally to match the pinned client behavior");
            complete_publish(completion, pending);
        }
    }
}

fn complete_publish(completion: &mut PublishCompletionContext<'_>, pending: &mut PendingPublish) {
    let PublishCompletionContext {
        runtime,
        entities,
        connected,
        availability_status,
        last_values,
        ready_continuations,
    } = completion;
    let mut changed_states = Vec::new();
    if let Some(mut update) = pending.optimistic.take()
        && update
            .entity_handle
            .as_ref()
            .is_none_or(|handle| handle.status().is_active())
    {
        let entity = entities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|entity| entity.metadata.discovery_topic == update.discovery_topic)
            .filter(|entity| mqtt_entity_is_active(entity))
            .cloned();
        if let Some(entity) = entity {
            let entity_id = mqtt_entity_id(&entity);
            let available = entity_available(&entity, *connected, availability_status);
            if update.climate_state.take().is_some()
                && let Some(climate) = entity.climate.as_ref()
            {
                let climate_state = climate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                match publish_climate_state(
                    runtime,
                    &entity,
                    &climate_state,
                    available,
                    update.context.clone(),
                ) {
                    Ok(Some((state, changed))) => {
                        if changed {
                            changed_states.push(state.clone());
                        }
                        last_values.insert(
                            state.entity_id.clone(),
                            LastValue {
                                state: state.state.clone(),
                                attributes: state.attributes.clone(),
                                context: update.context.clone(),
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(entity_id = %entity_id, error = %error, "failed to apply optimistic MQTT climate state");
                    }
                }
            } else {
                let cached_state = runtime.get_state(&entity_id);
                let fallback_state = cached_state
                    .as_ref()
                    .map(|state| state.state.as_str())
                    .filter(|state| *state != "unavailable")
                    .or_else(|| last_values.get(&entity_id).map(|last| last.state.as_str()))
                    .or_else(|| cached_state.as_ref().map(|state| state.state.as_str()))
                    .unwrap_or("unknown");
                let desired_state = update.state.as_deref().unwrap_or(fallback_state);
                let mut attributes = last_values
                    .get(&entity_id)
                    .map(|last| last.attributes.clone())
                    .or_else(|| cached_state.as_ref().map(|state| state.attributes.clone()))
                    .unwrap_or_else(|| update.attributes.clone());
                attributes.extend(update.optimistic_updates.clone());
                let (published, last_attributes, last_state) = if entity_lifecycle_handle(&entity)
                    .is_some()
                {
                    match shared_entity_projection(
                        &entity,
                        Some(desired_state),
                        available,
                        attributes.clone(),
                        update.context.clone(),
                    ) {
                        Ok(Some((publication, snapshot))) => {
                            let state = snapshot.state.clone().unwrap_or_else(|| "unknown".into());
                            (
                                entity_lifecycle_handle(&entity)
                                    .expect("shared projection has a lifecycle handle")
                                    .publish_publication(publication),
                                snapshot.attributes,
                                state,
                            )
                        }
                        Ok(None) => (Ok(None), attributes, desired_state.to_owned()),
                        Err(error) => {
                            warn!(entity_id = %entity_id, error = %error, "failed to project optimistic MQTT state");
                            (Err(error), attributes, desired_state.to_owned())
                        }
                    }
                } else if update.switch_state {
                    let snapshot = SwitchSnapshot::new(Some(desired_state == "on"))
                        .with_available(available)
                        .with_assumed_state(true)
                        .with_extra_attributes(attributes)
                        .with_context(update.context.clone());
                    let projected = snapshot.entity_snapshot();
                    let state = if available {
                        projected.state.as_deref().unwrap_or("unknown")
                    } else {
                        "unavailable"
                    };
                    (
                        runtime
                            .set_state(
                                &entity_id,
                                state,
                                projected.attributes.clone(),
                                projected.context.clone(),
                                false,
                            )
                            .map(Some),
                        projected.attributes,
                        desired_state.to_owned(),
                    )
                } else {
                    let state = if available {
                        desired_state
                    } else {
                        "unavailable"
                    };
                    let published = runtime
                        .set_state(
                            &entity_id,
                            state,
                            attributes.clone(),
                            update.context.clone(),
                            false,
                        )
                        .map(Some);
                    (published, attributes, desired_state.to_owned())
                };
                match published {
                    Ok(Some(change)) => {
                        if change.changed {
                            changed_states.push(change.state.clone());
                        }
                        last_values.insert(
                            change.state.entity_id.clone(),
                            LastValue {
                                state: last_state,
                                attributes: last_attributes,
                                context: update.context,
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(entity_id = %entity_id, error = %error, "failed to apply explicitly optimistic MQTT state")
                    }
                }
            }
        }
    }
    if let Some(reply) = pending.reply.take() {
        pending.changed_states.extend(changed_states);
        if let Some(command) = pending.remaining_commands.pop_front() {
            ready_continuations.push_back(CommandRequest {
                command,
                remaining_commands: std::mem::take(&mut pending.remaining_commands),
                changed_states: std::mem::take(&mut pending.changed_states),
                reply,
            });
        } else {
            let _ = reply.send(Ok(std::mem::take(&mut pending.changed_states)));
        }
    }
}

fn fail_pending_publishes(
    unassigned: &mut VecDeque<PendingPublish>,
    awaiting_ack: &mut HashMap<u16, PendingPublish>,
    error: Error,
) {
    for pending in unassigned.iter_mut().chain(awaiting_ack.values_mut()) {
        if let Some(reply) = pending.reply.take() {
            let _ = reply.send(Err(Error::Service(error.to_string())));
        }
        pending.optimistic = None;
    }
    unassigned.clear();
    awaiting_ack.clear();
}

async fn graceful_disconnect(client: &AsyncClient, event_loop: &mut rumqttc::EventLoop) {
    if client.try_disconnect().is_ok() {
        // Drain any already queued packets until DISCONNECT reaches the wire.
        // The bound keeps shutdown finite when the broker stops responding.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match event_loop.poll().await {
                    Ok(Event::Outgoing(rumqttc::Outgoing::Disconnect)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
    }
}

async fn reconcile_subscriptions(
    client: &AsyncClient,
    config: &MqttConfig,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    additional_subscriptions: &RwLock<BTreeMap<String, u8>>,
    active: &mut BTreeMap<String, u8>,
) -> Result<()> {
    let mut wanted = BTreeMap::new();
    if config.discovery_enabled {
        wanted.insert(format!("{}/#", config.discovery_prefix), config.qos);
    }
    for entity in entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
    {
        let entity_qos = entity.qos;
        if let Some(topic) = &entity.state_topic {
            wanted
                .entry(topic.clone())
                .and_modify(|current| *current = (*current).max(entity_qos))
                .or_insert(entity_qos);
        }
        if let Some(topic) = &entity.json_attributes_topic {
            wanted
                .entry(topic.clone())
                .and_modify(|current| *current = (*current).max(entity_qos))
                .or_insert(entity_qos);
        }
        for topic in entity.extra_state_topics.keys() {
            wanted
                .entry(topic.clone())
                .and_modify(|current| *current = (*current).max(entity_qos))
                .or_insert(entity_qos);
        }
        for availability in &entity.availability {
            wanted
                .entry(availability.topic.clone())
                .and_modify(|current| *current = (*current).max(entity_qos))
                .or_insert(entity_qos);
        }
    }
    for (topic, requested_qos) in additional_subscriptions
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
    {
        wanted
            .entry(topic.clone())
            .and_modify(|current| *current = (*current).max(*requested_qos))
            .or_insert(*requested_qos);
    }
    for (topic, active_qos) in active.iter() {
        if wanted.get(topic).copied() != Some(*active_qos)
            && let Err(error) = client.unsubscribe(topic.clone()).await
        {
            warn!(topic, error = %error, "failed to unsubscribe from stale MQTT topic");
        }
    }
    for (topic, qos_value) in &wanted {
        if active.get(topic) == Some(qos_value) {
            continue;
        }
        client
            .subscribe(topic.clone(), qos(*qos_value)?)
            .await
            .map_err(|error| Error::Service(format!("MQTT subscribe failed: {error}")))?;
    }
    *active = wanted;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    runtime: &Runtime,
    config: &MqttConfig,
    templates: &TemplateEngine,
    client: &AsyncClient,
    entities: &Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: &Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
    entity_lifecycle: &Option<SharedEntityLifecycleContext>,
    additional_subscriptions: &RwLock<BTreeMap<String, u8>>,
    subscribed: &mut BTreeMap<String, u8>,
    availability_status: &mut HashMap<(String, String), bool>,
    last_values: &mut HashMap<String, LastValue>,
    connected: bool,
    topic: &str,
    payload: &[u8],
    packet_qos: u8,
    retain: bool,
) {
    dispatch_mqtt_message(runtime, topic, payload, packet_qos, retain);
    if config.discovery_enabled && is_discovery_topic(topic, &config.discovery_prefix) {
        match process_discovery(
            runtime,
            config,
            topic,
            payload,
            entities,
            metadata,
            entity_lifecycle,
            availability_status,
        ) {
            Ok(true) => {
                if let Err(error) = reconcile_subscriptions(
                    client,
                    config,
                    entities,
                    additional_subscriptions,
                    subscribed,
                )
                .await
                {
                    warn!(error = %error, "failed to update MQTT subscriptions after discovery change");
                }
            }
            Ok(false) => {}
            Err(error) => warn!(topic, error = %error, "ignored invalid MQTT discovery payload"),
        }
        return;
    }

    let matched: Vec<_> = entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|entity| {
            entity.state_topic.as_deref() == Some(topic)
                || entity.json_attributes_topic.as_deref() == Some(topic)
                || entity.extra_state_topics.contains_key(topic)
                || entity.availability.iter().any(|item| item.topic == topic)
        })
        .cloned()
        .collect();
    if matched.is_empty() {
        return;
    }
    let Ok(payload_text) = std::str::from_utf8(payload) else {
        warn!(topic, "discarding non-UTF8 MQTT payload");
        return;
    };

    for entity in matched {
        if entity.availability.iter().any(|item| item.topic == topic) {
            let Some(availability) = entity.availability.iter().find(|item| item.topic == topic)
            else {
                continue;
            };
            let rendered = match render_string(
                runtime,
                templates,
                availability.value_template.as_deref(),
                payload_text,
                &entity,
            ) {
                Ok(value) => value,
                Err(error) => {
                    warn!(entity_id = entity.metadata.entity_id, error = %error, "availability template failed");
                    continue;
                }
            };
            let available = if rendered == availability.payload_available {
                Some(true)
            } else if rendered == availability.payload_not_available {
                Some(false)
            } else {
                None
            };
            if let Some(available) = available {
                availability_status
                    .insert((entity.metadata.entity_id.clone(), topic.into()), available);
                let current_available = entity_available(&entity, connected, availability_status);
                apply_availability(runtime, &entity, current_available, last_values);
            }
            continue;
        }

        if let Some(climate) = entity.climate.as_ref()
            && (climate.state_topics.contains_key(topic)
                || entity.json_attributes_topic.as_deref() == Some(topic))
        {
            let mut climate_state = climate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut updated = false;
            if climate.state_topics.contains_key(topic) {
                match update_climate_reported_state(
                    runtime,
                    &entity,
                    topic,
                    payload_text,
                    templates,
                    climate_state.clone(),
                ) {
                    Ok(Some(next)) => {
                        climate_state = next;
                        updated = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(entity_id = %entity.metadata.entity_id, error = %error, "ignored invalid MQTT climate state");
                    }
                }
            }
            if entity.json_attributes_topic.as_deref() == Some(topic) {
                match parse_json_attributes(templates, &entity, payload_text) {
                    Ok(extra) => {
                        climate_state.snapshot.extra_attributes.extend(extra);
                        updated = true;
                    }
                    Err(error) => {
                        warn!(entity_id = %entity.metadata.entity_id, error = %error, "ignored invalid MQTT climate JSON attributes");
                    }
                }
            }
            if updated {
                let context = Context::default();
                let available = entity_available(&entity, connected, availability_status);
                match publish_climate_state(
                    runtime,
                    &entity,
                    &climate_state,
                    available,
                    context.clone(),
                ) {
                    Ok(Some((state, _))) => {
                        last_values.insert(
                            state.entity_id.clone(),
                            LastValue {
                                state: state.state.clone(),
                                attributes: state.attributes.clone(),
                                context,
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(entity_id = %entity.metadata.entity_id, error = %error, "failed to publish MQTT climate state");
                    }
                }
            }
            continue;
        }
        let mut value = last_values.get(&entity.metadata.entity_id).cloned();
        if entity.state_topic.as_deref() == Some(topic)
            || entity.extra_state_topics.contains_key(topic)
        {
            let current = value
                .take()
                .or_else(|| {
                    runtime
                        .get_state(&entity.metadata.entity_id)
                        .map(|state| LastValue {
                            state: state.state,
                            attributes: state.attributes,
                            context: state.context,
                        })
                })
                .unwrap_or_else(|| LastValue {
                    state: "unknown".into(),
                    attributes: entity.attributes.clone(),
                    context: Context::default(),
                });
            match update_reported_state(runtime, &entity, topic, payload_text, templates, current) {
                Ok(mut next) => {
                    if entity.json_attributes_topic.as_deref() == Some(topic) {
                        match parse_json_attributes(templates, &entity, payload_text) {
                            Ok(extra) => next.attributes.extend(extra),
                            Err(error) => {
                                warn!(entity_id = entity.metadata.entity_id, error = %error, "ignored invalid MQTT JSON attributes")
                            }
                        }
                    }
                    value = Some(next);
                }
                Err(error) => {
                    warn!(entity_id = entity.metadata.entity_id, error = %error, "ignored invalid MQTT reported state")
                }
            }
        } else if entity.json_attributes_topic.as_deref() == Some(topic) {
            match parse_json_attributes(templates, &entity, payload_text) {
                Ok(extra) => {
                    let mut next = value.unwrap_or_else(|| LastValue {
                        state: "unknown".into(),
                        attributes: entity.attributes.clone(),
                        context: Context::default(),
                    });
                    next.attributes.extend(extra);
                    value = Some(next);
                }
                Err(error) => {
                    warn!(entity_id = entity.metadata.entity_id, error = %error, "ignored invalid MQTT JSON attributes")
                }
            }
        }
        if let Some(next) = value {
            last_values.insert(entity.metadata.entity_id.clone(), next.clone());
            if entity_available(&entity, connected, availability_status) {
                publish_discovered_state(
                    runtime,
                    &entity,
                    Some(&next.state),
                    true,
                    next.attributes,
                    Context::default(),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_discovery(
    runtime: &Runtime,
    config: &MqttConfig,
    discovery_topic: &str,
    payload: &[u8],
    entities: &Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: &Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
    entity_lifecycle: &Option<SharedEntityLifecycleContext>,
    availability_status: &mut HashMap<(String, String), bool>,
) -> Result<bool> {
    if payload.len() > config.max_payload_bytes {
        return Err(Error::InvalidInput(
            "discovery payload exceeds configured limit".into(),
        ));
    }
    if payload.is_empty() {
        let removed = entities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(discovery_topic)
            .cloned();
        if let Some(entity) = removed {
            // Discovery deletion is permanent, unlike provider unload. Remove
            // our registry identity before releasing the lease so the lifecycle
            // does not restore an enabled, unavailable placeholder afterward.
            // A reused entity ID must never delete another publisher's row.
            let entity_id = mqtt_entity_id(&entity);
            if let Some(context) = entity_lifecycle
                && let Some(handle) = entity_lifecycle_handle(&entity)
                && let Some(registry_id) = handle.registry_id()
                && let Some(registered) = context.registries.entity(&entity_id)
                && registered.id == registry_id
                && registered.platform == "mqtt"
                && registered.config_entry_id.as_deref() == Some(context.entry_id.as_str())
            {
                context.registries.remove_entity(&entity_id)?;
            }
            entities
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(discovery_topic);
            for item in &entity.availability {
                availability_status
                    .remove(&(entity.metadata.entity_id.clone(), item.topic.clone()));
            }
            if let Some(handle) = entity_lifecycle_handle(&entity) {
                let _ = handle.release();
            } else {
                let _ = runtime.remove_state(&entity.metadata.entity_id, Context::default());
            }
            metadata
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&entity.metadata.entity_id);
            return Ok(true);
        }
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(payload)
        .map_err(|error| Error::InvalidInput(format!("discovery JSON is invalid: {error}")))?;
    let config_object = value
        .as_object()
        .ok_or_else(|| Error::InvalidInput("discovery payload must be a JSON object".into()))?;
    let (platform, node_id, object_id) =
        discovery_parts(discovery_topic, &config.discovery_prefix)?;
    if !matches!(
        platform.as_str(),
        "sensor"
            | "binary_sensor"
            | "switch"
            | "light"
            | "cover"
            | "fan"
            | "button"
            | "climate"
            | "number"
            | "select"
            | "text"
    ) {
        return Ok(false);
    }
    let parsed = parse_discovered_entity(
        runtime.unit_system(),
        config,
        discovery_topic,
        &platform,
        &node_id,
        &object_id,
        config_object,
    )?;

    let mut entity_map = entities
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if entity_map.len() >= MAX_DISCOVERED_ENTITIES && !entity_map.contains_key(discovery_topic) {
        return Err(Error::InvalidInput(format!(
            "discovery entity limit ({MAX_DISCOVERED_ENTITIES}) reached"
        )));
    }
    let prior = entity_map.get(discovery_topic).cloned();
    let mut entity = parsed;
    if let Some(prior) = &prior {
        entity.metadata.entity_id = prior.metadata.entity_id.clone();
        entity.button_last_value = Arc::clone(&prior.button_last_value);
    } else {
        let uid_owner = entity_map
            .values()
            .find(|existing| existing.metadata.unique_id == entity.metadata.unique_id);
        if uid_owner.is_some() {
            return Err(Error::InvalidInput(format!(
                "duplicate MQTT unique_id {:?}",
                entity.metadata.unique_id
            )));
        }
        let mut entity_id = entity.metadata.entity_id.clone();
        let shared_entity = entity_lifecycle.is_some()
            && matches!(
                entity.metadata.platform.as_str(),
                "switch" | "button" | "light" | "cover" | "fan" | "climate"
            );
        let occupied_by_runtime = !shared_entity && runtime.get_state(&entity_id).is_some();
        let occupied_by_map = !shared_entity
            && entity_map
                .values()
                .any(|known| known.metadata.entity_id == entity_id);
        if occupied_by_runtime || occupied_by_map {
            let suffix = slug(&entity.metadata.unique_id);
            entity_id = format!(
                "{}.{}_{suffix}",
                entity.metadata.platform,
                slug(&entity.metadata.name)
            );
            let mut ordinal = 2;
            while runtime.get_state(&entity_id).is_some()
                || entity_map
                    .values()
                    .any(|known| known.metadata.entity_id == entity_id)
            {
                entity_id = format!(
                    "{}.{}_{}",
                    entity.metadata.platform,
                    slug(&entity.metadata.name),
                    ordinal
                );
                ordinal += 1;
            }
        }
        entity.metadata.entity_id = entity_id;
    }

    if let Some(entity_lifecycle) = entity_lifecycle.as_ref()
        && matches!(
            entity.metadata.platform.as_str(),
            "switch" | "button" | "light" | "cover" | "fan" | "climate"
        )
    {
        let climate_units = climate_unit_context(runtime.unit_system());
        let prior_handle = prior.as_ref().and_then(entity_lifecycle_handle).cloned();
        let prior_released = if let Some(handle) = prior_handle {
            handle.release()?;
            true
        } else {
            false
        };
        if let Err(registration_error) = register_shared_entity_into(
            &mut entity,
            prior.as_ref(),
            entity_lifecycle,
            climate_units,
        ) {
            if prior_released && let Some(previous) = prior.as_ref() {
                let mut restored = previous.clone();
                match register_shared_entity_into(
                    &mut restored,
                    Some(previous),
                    entity_lifecycle,
                    climate_units,
                ) {
                    Ok(()) => {
                        let mut entity_metadata = metadata
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        entity_metadata.remove(&previous.metadata.entity_id);
                        entity_metadata.insert(
                            restored.metadata.entity_id.clone(),
                            restored.metadata.clone(),
                        );
                        entity_map.insert(discovery_topic.to_owned(), restored);
                        return Err(registration_error);
                    }
                    Err(restore_error) => {
                        return Err(Error::Service(format!(
                            "MQTT discovery update failed: {registration_error}; restoring the prior shared entity failed: {restore_error}"
                        )));
                    }
                }
            }
            return Err(registration_error);
        }
    }
    entity.metadata.entity_lifecycle_managed = entity_lifecycle_handle(&entity).is_some();

    let is_new = prior.is_none();
    if let Some(prior) = prior {
        for item in &prior.availability {
            availability_status.remove(&(prior.metadata.entity_id.clone(), item.topic.clone()));
        }
        metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&prior.metadata.entity_id);
    }
    for item in &entity.availability {
        availability_status.insert(
            (entity.metadata.entity_id.clone(), item.topic.clone()),
            false,
        );
    }
    entity_map.insert(discovery_topic.to_owned(), entity.clone());
    metadata
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(entity.metadata.entity_id.clone(), entity.metadata.clone());
    let loaded_components = runtime.loaded_components();
    let platform = entity.metadata.platform.clone();
    loaded_components.record_successful_component(platform.clone());
    loaded_components.record_successful_component(format!("mqtt.{platform}"));

    if is_new {
        let available = entity.availability.is_empty();
        publish_discovered_state(
            runtime,
            &entity,
            None,
            available,
            entity.attributes.clone(),
            Context::default(),
        );
    } else if entity.climate.is_some() && entity.availability.is_empty() {
        publish_discovered_state(
            runtime,
            &entity,
            None,
            true,
            entity.attributes.clone(),
            Context::default(),
        );
    } else if !entity.availability.is_empty() {
        let previous = runtime.get_state(&entity.metadata.entity_id);
        let previous_state = previous.as_ref().map(|state| state.state.clone());
        let previous_attributes = previous
            .map(|state| state.attributes)
            .unwrap_or_else(|| entity.attributes.clone());
        publish_discovered_state(
            runtime,
            &entity,
            previous_state.as_deref(),
            false,
            previous_attributes,
            Context::default(),
        );
    }
    Ok(true)
}

fn register_switch_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
) -> Result<EntityHandle> {
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let suggested_object_id = entity
        .metadata
        .entity_id
        .split_once('.')
        .map(|(_, object_id)| object_id.to_owned());
    let descriptor = SwitchDescriptor::new(
        "mqtt",
        context.provider_id.clone(),
        entity.metadata.unique_id.clone(),
    )
    .with_unique_id(entity.metadata.unique_id.clone())
    .with_config_entry_id(context.entry_id.clone())
    .with_name(entity.metadata.name.clone())
    .with_suggested_object_id(suggested_object_id.unwrap_or_default());
    let descriptor = if let Some(device_class) = entity.switch_device_class {
        descriptor.with_device_class(device_class)
    } else {
        descriptor
    };
    let descriptor = if let Some(device_id) = device_id {
        descriptor.with_device_id(device_id)
    } else {
        descriptor
    };
    let descriptor = if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor.with_icon(icon.to_owned())
    } else {
        descriptor
    };
    let descriptor = if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor.with_entity_category(category.to_owned())
    } else {
        descriptor
    };
    context
        .lifecycle
        .register_batch(vec![descriptor.into_entity_spec()])?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Service("shared lifecycle returned no MQTT switch handle".into()))
}

fn register_button_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
) -> Result<(EntityHandle, ButtonDescriptor)> {
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let suggested_object_id = entity
        .metadata
        .entity_id
        .split_once('.')
        .map(|(_, object_id)| object_id.to_owned());
    let descriptor = ButtonDescriptor::new(
        "mqtt",
        context.provider_id.clone(),
        entity.metadata.unique_id.clone(),
    )
    .with_unique_id(entity.metadata.unique_id.clone())
    .with_config_entry_id(context.entry_id.clone())
    .with_name(entity.metadata.name.clone())
    .with_suggested_object_id(suggested_object_id.unwrap_or_default());
    let descriptor = if let Some(device_class) = entity.button_device_class {
        descriptor.with_device_class(device_class)
    } else {
        descriptor
    };
    let descriptor = if let Some(device_id) = device_id {
        descriptor.with_device_id(device_id)
    } else {
        descriptor
    };
    let descriptor = if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor.with_icon(icon.to_owned())
    } else {
        descriptor
    };
    let descriptor = if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor.with_entity_category(category.to_owned())
    } else {
        descriptor
    };
    let handle = context
        .lifecycle
        .register_batch(vec![descriptor.clone().into_entity_spec()])?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Service("shared lifecycle returned no MQTT button handle".into()))?;
    Ok((handle, descriptor))
}

fn register_shared_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
    climate_units: ClimateUnitContext,
) -> Result<SharedMqttEntity> {
    match entity.metadata.platform.as_str() {
        "switch" => Ok(SharedMqttEntity::Switch(register_switch_entity(
            entity, context,
        )?)),
        "button" => {
            let (handle, descriptor) = register_button_entity(entity, context)?;
            Ok(SharedMqttEntity::Button { handle, descriptor })
        }
        "light" => {
            let (handle, descriptor) = register_light_entity(entity, context)?;
            Ok(SharedMqttEntity::Light { handle, descriptor })
        }
        "cover" => {
            let (handle, descriptor) = register_cover_entity(entity, context)?;
            Ok(SharedMqttEntity::Cover {
                handle,
                descriptor,
                toggle_state: Arc::new(Mutex::new(CoverToggleState::new())),
            })
        }
        "fan" => {
            let (handle, descriptor) = register_fan_entity(entity, context)?;
            Ok(SharedMqttEntity::Fan { handle, descriptor })
        }
        "climate" => {
            let (handle, descriptor) = register_climate_entity(entity, context, climate_units)?;
            Ok(SharedMqttEntity::Climate { handle, descriptor })
        }
        platform => Err(Error::Service(format!(
            "MQTT {platform} does not use the shared entity lifecycle"
        ))),
    }
}

fn register_shared_entity_into(
    entity: &mut DiscoveredEntity,
    previous: Option<&DiscoveredEntity>,
    context: &SharedEntityLifecycleContext,
    climate_units: ClimateUnitContext,
) -> Result<()> {
    let mut shared = register_shared_entity(entity, context, climate_units)?;
    if let SharedMqttEntity::Cover { toggle_state, .. } = &mut shared
        && let Some(SharedMqttEntity::Cover {
            toggle_state: previous_toggle_state,
            ..
        }) = previous.and_then(|previous| previous.shared_entity.as_ref())
    {
        *toggle_state = Arc::clone(previous_toggle_state);
    }
    let climate_descriptor = match &shared {
        SharedMqttEntity::Climate { descriptor, .. } => Some(descriptor.clone()),
        _ => None,
    };
    let entity_id = shared.handle().status().entity_id().to_owned();
    entity.shared_entity = Some(shared);
    if let (Some(descriptor), Some(climate)) = (climate_descriptor, entity.climate.as_mut()) {
        climate.descriptor = descriptor;
    }
    entity.metadata.entity_id = entity_id;
    entity.metadata.entity_lifecycle_managed = true;
    Ok(())
}

fn register_light_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
) -> Result<(EntityHandle, LightDescriptor)> {
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let modes = entity
        .attributes
        .get(hass_light::attr::SUPPORTED_COLOR_MODES)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| {
                            Error::InvalidInput(
                                "MQTT light supported_color_modes must contain strings".into(),
                            )
                        })?
                        .parse::<LightColorMode>()
                        .map_err(|error| Error::InvalidInput(error.to_string()))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_else(|| {
            vec![
                if config_bool(&entity.platform_config, "brightness", false) {
                    LightColorMode::Brightness
                } else {
                    LightColorMode::OnOff
                },
            ]
        });

    let mut features = LightFeatures::NONE;
    if config_bool(&entity.platform_config, "effect", false) {
        features = features.with(LightFeature::Effect);
    }
    if config_bool(&entity.platform_config, "flash", false) {
        features = features.with(LightFeature::Flash);
    }
    if config_bool(&entity.platform_config, "transition", false) {
        features = features.with(LightFeature::Transition);
    }
    let mut descriptor = LightDescriptor::new(
        "mqtt",
        context.provider_id.clone(),
        entity.metadata.unique_id.clone(),
    )
    .with_unique_id(entity.metadata.unique_id.clone())
    .with_config_entry_id(context.entry_id.clone())
    .with_name(entity.metadata.name.clone())
    .with_suggested_object_id(mqtt_suggested_object_id(entity))
    .with_supported_color_modes(modes)
    .with_supported_features(features)
    .with_min_color_temp_kelvin(config_u32(
        &entity.platform_config,
        "min_kelvin",
        hass_light::LIGHT_DEFAULT_MIN_KELVIN,
    ))
    .with_max_color_temp_kelvin(config_u32(
        &entity.platform_config,
        "max_kelvin",
        hass_light::LIGHT_DEFAULT_MAX_KELVIN,
    ));
    if let Some(device_id) = device_id {
        descriptor = descriptor.with_device_id(device_id);
    }
    if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor = descriptor.with_icon(icon.to_owned());
    }
    if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor = descriptor.with_entity_category(category.to_owned());
    }
    if let Some(effects) = entity
        .platform_config
        .get("effect_list")
        .and_then(Value::as_array)
    {
        descriptor = descriptor
            .with_effect_list(effects.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    let spec = descriptor
        .try_entity_spec()
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    let handle = register_shared_spec(&context.lifecycle, spec, "light")?;
    Ok((handle, descriptor))
}

fn register_cover_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
) -> Result<(EntityHandle, CoverDescriptor)> {
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let mut features = CoverFeatures::EMPTY;
    if entity.command_topic.is_some() {
        features |= CoverFeatures::OPEN | CoverFeatures::CLOSE | CoverFeatures::STOP;
    }
    if entity
        .extra_command_topics
        .contains_key("set_position_topic")
    {
        features |= CoverFeatures::SET_POSITION;
    }
    if entity
        .extra_command_topics
        .contains_key("tilt_command_topic")
    {
        features |= CoverFeatures::OPEN_TILT
            | CoverFeatures::CLOSE_TILT
            | CoverFeatures::STOP_TILT
            | CoverFeatures::SET_TILT_POSITION;
    }
    let mut descriptor = CoverDescriptor::new(
        "mqtt",
        context.provider_id.clone(),
        entity.metadata.unique_id.clone(),
    )
    .with_unique_id(entity.metadata.unique_id.clone())
    .with_config_entry_id(context.entry_id.clone())
    .with_name(entity.metadata.name.clone())
    .with_suggested_object_id(mqtt_suggested_object_id(entity))
    .with_supported_features(features);
    if let Some(device_class) = entity
        .attributes
        .get("device_class")
        .and_then(Value::as_str)
    {
        descriptor = descriptor.with_device_class(
            device_class
                .parse::<CoverDeviceClass>()
                .map_err(|error| Error::InvalidInput(error.to_string()))?,
        );
    }
    if let Some(device_id) = device_id {
        descriptor = descriptor.with_device_id(device_id);
    }
    if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor = descriptor.with_icon(icon.to_owned());
    }
    if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor = descriptor.with_entity_category(category.to_owned());
    }
    let handle = register_shared_spec(&context.lifecycle, descriptor.entity_spec(), "cover")?;
    Ok((handle, descriptor))
}

fn register_fan_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
) -> Result<(EntityHandle, FanDescriptor)> {
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let mut features = FanFeatures::default();
    if entity.command_topic.is_some() {
        features |= FanFeatures::TURN_ON | FanFeatures::TURN_OFF;
    }
    if entity
        .extra_command_topics
        .contains_key("percentage_command_topic")
    {
        features |= FanFeatures::SET_SPEED;
    }
    if entity
        .extra_command_topics
        .contains_key("preset_mode_command_topic")
    {
        features |= FanFeatures::PRESET_MODE;
    }
    if entity
        .extra_command_topics
        .contains_key("oscillation_command_topic")
    {
        features |= FanFeatures::OSCILLATE;
    }
    if entity
        .extra_command_topics
        .contains_key("direction_command_topic")
    {
        features |= FanFeatures::DIRECTION;
    }
    let preset_modes = entity
        .extra_command_topics
        .contains_key("preset_mode_command_topic")
        .then(|| config_strings(&entity.platform_config, "preset_modes"));
    let speed_count = if features.contains(FanFeatures::SET_SPEED) {
        let min = config_number(&entity.platform_config, "speed_range_min", 1.0);
        let max = config_number(&entity.platform_config, "speed_range_max", 100.0);
        ((max - min + 1.0) as i64).min(100)
    } else {
        100
    };
    let mut descriptor = FanDescriptor::new(
        "mqtt",
        context.provider_id.clone(),
        entity.metadata.unique_id.clone(),
    )
    .with_unique_id(entity.metadata.unique_id.clone())
    .with_config_entry_id(context.entry_id.clone())
    .with_name(entity.metadata.name.clone())
    .with_suggested_object_id(mqtt_suggested_object_id(entity))
    .with_features(features)
    .with_speed_count(speed_count)
    .with_preset_modes(preset_modes);
    if let Some(device_id) = device_id {
        descriptor = descriptor.with_device_id(device_id);
    }
    if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor = descriptor.with_icon(icon.to_owned());
    }
    if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor = descriptor.with_entity_category(category.to_owned());
    }
    let handle = register_shared_spec(&context.lifecycle, descriptor.entity_spec(), "fan")?;
    Ok((handle, descriptor))
}

fn register_shared_spec(
    lifecycle: &EntityLifecycle,
    spec: hass_entity::EntitySpec,
    platform: &str,
) -> Result<EntityHandle> {
    lifecycle
        .register_batch(vec![spec])?
        .into_iter()
        .next()
        .ok_or_else(|| {
            Error::Service(format!(
                "shared lifecycle returned no MQTT {platform} handle"
            ))
        })
}

fn register_climate_entity(
    entity: &DiscoveredEntity,
    context: &SharedEntityLifecycleContext,
    units: ClimateUnitContext,
) -> Result<(EntityHandle, ClimateDescriptor)> {
    if entity.climate.is_none() {
        return Err(Error::Service(
            "MQTT climate discovery did not produce a climate descriptor".into(),
        ));
    }
    let device_id = register_mqtt_device(
        &context.registries,
        &context.entry_id,
        entity.metadata.device.as_ref(),
    )?;
    let descriptor = mqtt_climate_descriptor(
        entity,
        &context.provider_id,
        Some(&context.entry_id),
        device_id,
    )?;
    let spec = descriptor
        .entity_spec_with_units(units)
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    let handle = register_shared_spec(&context.lifecycle, spec, "climate")?;
    Ok((handle, descriptor))
}
fn mqtt_suggested_object_id(entity: &DiscoveredEntity) -> String {
    entity
        .metadata
        .entity_id
        .split_once('.')
        .map(|(_, object_id)| object_id.to_owned())
        .unwrap_or_default()
}

fn register_mqtt_device(
    registries: &Registries,
    entry_id: &str,
    value: Option<&Value>,
) -> Result<Option<String>> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Ok(None);
    };
    let identifiers = mqtt_string_set(object.get("identifiers"))
        .into_iter()
        .map(|identifier| ("mqtt".to_owned(), identifier))
        .collect::<BTreeSet<_>>();
    let connections = mqtt_pair_set(object.get("connections"));
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
                .insert(("mqtt".to_owned(), identifier.to_owned()));
            registries
                .get_or_create_device(parent)
                .map(|device| device.id)
        })
        .transpose()?;

    let registration = DeviceRegistration {
        config_entry_id: Some(entry_id.to_owned()),
        identifiers,
        connections,
        name: mqtt_string_value(object.get("name")),
        manufacturer: mqtt_string_value(object.get("manufacturer")),
        model: mqtt_string_value(object.get("model")),
        model_id: mqtt_string_value(object.get("model_id")),
        serial_number: mqtt_string_value(object.get("serial_number")),
        sw_version: mqtt_string_value(object.get("sw_version")),
        hw_version: mqtt_string_value(object.get("hw_version")),
        configuration_url: mqtt_string_value(object.get("configuration_url")),
        via_device_id,
        ..DeviceRegistration::default()
    };
    Ok(Some(registries.get_or_create_device(registration)?.id))
}

fn mqtt_string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn mqtt_pair_set(value: Option<&Value>) -> BTreeSet<(String, String)> {
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

fn mqtt_string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn parse_discovered_entity(
    unit_system: UnitSystemKind,
    config: &MqttConfig,
    discovery_topic: &str,
    platform: &str,
    node_id: &str,
    object_id: &str,
    object: &Map<String, Value>,
) -> Result<DiscoveredEntity> {
    let get_string = |key: &str| -> Result<Option<String>> {
        match object.get(key) {
            Some(Value::String(value)) if value.len() <= 8192 => Ok(Some(value.clone())),
            Some(Value::String(_)) => Err(Error::InvalidInput(format!(
                "discovery field {key} exceeds 8192 bytes"
            ))),
            Some(Value::Null) | None => Ok(None),
            Some(_) => Err(Error::InvalidInput(format!(
                "discovery field {key} must be a string"
            ))),
        }
    };
    let get_bool = |key: &str, default: bool| -> Result<bool> {
        match object.get(key) {
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::Null) | None => Ok(default),
            Some(_) => Err(Error::InvalidInput(format!(
                "discovery field {key} must be a boolean"
            ))),
        }
    };
    let get_number = |key: &str, default: f64| -> Result<f64> {
        match object.get(key) {
            Some(value) => value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| {
                    Error::InvalidInput(format!("discovery field {key} must be a finite number"))
                }),
            None => Ok(default),
        }
    };
    let get_string_list = |key: &str| -> Result<Option<Vec<String>>> {
        match object.get(key) {
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        Error::InvalidInput(format!(
                            "discovery field {key} must contain only strings"
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()
                .map(Some),
            Some(Value::Null) | None => Ok(None),
            Some(_) => Err(Error::InvalidInput(format!(
                "discovery field {key} must be a list of strings"
            ))),
        }
    };
    let name = get_string("name")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| object_id.to_owned());
    let unique_id = get_string("unique_id")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("mqtt:{node_id}:{object_id}"));
    let base_topic = get_string("~")?;
    let topic = |key: &str| -> Result<Option<String>> {
        if object.get(key).is_some_and(Value::is_null) {
            return Err(Error::InvalidInput(format!(
                "discovery field {key} cannot be null"
            )));
        }
        let Some(raw) = get_string(key)? else {
            return Ok(None);
        };
        let expanded = expand_topic(&raw, base_topic.as_deref(), key)?;
        validate_topic(&expanded, key)?;
        Ok(Some(expanded))
    };
    let state_topic = if platform == "climate" {
        None
    } else {
        topic("state_topic")?
    };
    let command_topic = if platform == "climate" {
        None
    } else {
        topic("command_topic")?
    };
    let json_attributes_topic = topic("json_attributes_topic")?;
    let schema = if platform == "light" {
        get_string("schema")?.unwrap_or_else(|| "basic".into())
    } else {
        String::new()
    };
    if platform == "light" && !matches!(schema.as_str(), "basic" | "json") {
        return Err(Error::InvalidInput(format!(
            "unsupported MQTT light schema {schema:?}"
        )));
    }
    if matches!(platform, "sensor" | "binary_sensor") && state_topic.is_none() {
        return Err(Error::InvalidInput(format!(
            "{platform} discovery requires state_topic"
        )));
    }
    if matches!(
        platform,
        "switch" | "light" | "button" | "number" | "select" | "text" | "fan"
    ) && command_topic.is_none()
    {
        return Err(Error::InvalidInput(format!(
            "{platform} discovery requires command_topic"
        )));
    }
    if platform == "light" && schema == "json" && get_string("command_template")?.is_some() {
        return Err(Error::InvalidInput(
            "MQTT JSON lights use their JSON command schema, not command_template".into(),
        ));
    }
    if platform == "cover" && object.contains_key("value_template") && state_topic.is_none() {
        return Err(Error::InvalidInput(
            "MQTT cover value_template requires state_topic".into(),
        ));
    }
    if platform == "cover"
        && object.contains_key("set_position_topic")
        && topic("position_topic")?.is_none()
    {
        return Err(Error::InvalidInput(
            "MQTT cover set_position_topic requires position_topic".into(),
        ));
    }
    if platform == "fan" {
        for (template, required_topic) in [
            ("direction_command_template", "direction_command_topic"),
            ("direction_value_template", "direction_state_topic"),
            ("percentage_command_template", "percentage_command_topic"),
            ("percentage_value_template", "percentage_state_topic"),
            ("preset_mode_command_template", "preset_mode_command_topic"),
            ("preset_mode_value_template", "preset_mode_state_topic"),
            ("oscillation_command_template", "oscillation_command_topic"),
            ("oscillation_value_template", "oscillation_state_topic"),
        ] {
            if object.contains_key(template) && topic(required_topic)?.is_none() {
                return Err(Error::InvalidInput(format!(
                    "MQTT fan {template} requires {required_topic}"
                )));
            }
        }
    }
    for (template, required_topic) in [
        ("position_template", "position_topic"),
        ("get_position_template", "position_topic"),
        ("set_position_template", "set_position_topic"),
        ("tilt_command_template", "tilt_command_topic"),
        ("tilt_status_template", "tilt_status_topic"),
    ] {
        if object.contains_key(template) && !object.contains_key(required_topic) {
            return Err(Error::InvalidInput(format!(
                "MQTT cover {template} requires {required_topic}"
            )));
        }
    }
    let mut extra_state_topics = BTreeMap::new();
    let extra_topic_keys: &[&str] = match platform {
        "cover" => &["position_topic", "tilt_status_topic"],
        "fan" => &[
            "percentage_state_topic",
            "preset_mode_state_topic",
            "oscillation_state_topic",
            "direction_state_topic",
        ],
        "climate" => &[
            "action_topic",
            "current_humidity_topic",
            "current_temperature_topic",
            "fan_mode_state_topic",
            "target_humidity_state_topic",
            "mode_state_topic",
            "preset_mode_state_topic",
            "swing_horizontal_mode_state_topic",
            "swing_mode_state_topic",
            "temperature_state_topic",
            "temperature_low_state_topic",
            "temperature_high_state_topic",
        ],
        _ => &[],
    };
    for key in extra_topic_keys {
        if let Some(expanded) = topic(key)? {
            extra_state_topics.insert(expanded, (*key).to_owned());
        }
    }
    let mut extra_command_topics = BTreeMap::new();
    let extra_command_topic_keys: &[&str] = match platform {
        "cover" => &["set_position_topic", "tilt_command_topic"],
        "fan" => &[
            "percentage_command_topic",
            "preset_mode_command_topic",
            "oscillation_command_topic",
            "direction_command_topic",
        ],
        _ => &[],
    };
    for key in extra_command_topic_keys {
        if let Some(expanded) = topic(key)? {
            extra_command_topics.insert((*key).to_owned(), expanded);
        }
    }
    let payload_on = get_string("payload_on")?.unwrap_or_else(|| "ON".into());
    let payload_off = get_string("payload_off")?.unwrap_or_else(|| "OFF".into());
    let state_on = get_string("state_on")?.unwrap_or_else(|| payload_on.clone());
    let state_off = get_string("state_off")?.unwrap_or_else(|| payload_off.clone());
    let switch_device_class = if platform == "switch" {
        get_string("device_class")?
            .map(|value| {
                value.parse::<SwitchDeviceClass>().map_err(|error| {
                    Error::InvalidInput(format!("invalid MQTT switch device_class: {error}"))
                })
            })
            .transpose()?
    } else {
        None
    };
    let button_device_class = if platform == "button" {
        get_string("device_class")?
            .map(|value| {
                value.parse::<ButtonDeviceClass>().map_err(|error| {
                    Error::InvalidInput(format!("invalid MQTT button device_class: {error}"))
                })
            })
            .transpose()?
    } else {
        None
    };
    for key in [
        "state_value_template",
        "position_template",
        "get_position_template",
        "set_position_template",
        "tilt_command_template",
        "tilt_status_template",
        "percentage_command_template",
        "percentage_value_template",
        "preset_mode_command_template",
        "preset_mode_value_template",
        "oscillation_command_template",
        "oscillation_value_template",
        "direction_command_template",
        "direction_value_template",
        "action_template",
        "current_humidity_template",
        "current_temperature_template",
        "fan_mode_command_template",
        "fan_mode_state_template",
        "target_humidity_command_template",
        "target_humidity_state_template",
        "mode_command_template",
        "mode_state_template",
        "power_command_template",
        "preset_mode_command_template",
        "preset_mode_value_template",
        "swing_horizontal_mode_command_template",
        "swing_horizontal_mode_state_template",
        "swing_mode_command_template",
        "swing_mode_state_template",
        "temperature_command_template",
        "temperature_high_command_template",
        "temperature_high_state_template",
        "temperature_low_command_template",
        "temperature_low_state_template",
        "temperature_state_template",
    ] {
        let _ = get_string(key)?;
    }
    for key in [
        "payload_open",
        "payload_close",
        "payload_stop",
        "payload_stop_tilt",
        "payload_press",
        "payload_reset",
        "payload_reset_percentage",
        "payload_reset_preset_mode",
        "payload_oscillation_on",
        "payload_oscillation_off",
    ] {
        let _ = get_string(key)?;
    }
    let value_template = if platform == "light" {
        get_string("state_value_template")?.or(get_string("value_template")?)
    } else {
        get_string("value_template")?
    };
    let command_template = get_string("command_template")?;
    let attributes_template = get_string("json_attributes_template")?;
    let retain = get_bool("retain", false)?;
    let optimistic = get_bool("optimistic", config.optimistic)?;
    let entity_qos = match object.get("qos") {
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= 2)
            .ok_or_else(|| Error::InvalidInput("discovery qos must be 0, 1, or 2".into()))?
            as u8,
        None => config.qos,
    };
    qos(entity_qos)?;

    let availability_mode = match object
        .get("availability_mode")
        .and_then(Value::as_str)
        .unwrap_or("all")
    {
        "all" => AvailabilityMode::All,
        "any" => AvailabilityMode::Any,
        other => {
            return Err(Error::InvalidInput(format!(
                "unsupported availability_mode {other:?}"
            )));
        }
    };
    let mut availability = Vec::new();
    if let Some(configs) = object.get("availability") {
        let entries = match configs {
            Value::Array(entries) => entries.as_slice(),
            Value::Object(_) => std::slice::from_ref(configs),
            _ => {
                return Err(Error::InvalidInput(
                    "availability must be an object or list".into(),
                ));
            }
        };
        for item in entries {
            let item = item.as_object().ok_or_else(|| {
                Error::InvalidInput("availability entries must be objects".into())
            })?;
            let raw_topic = item
                .get("topic")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidInput("availability entry requires topic".into()))?;
            let expanded = expand_topic(raw_topic, base_topic.as_deref(), "availability.topic")?;
            validate_topic(&expanded, "availability.topic")?;
            availability.push(AvailabilityTopic {
                topic: expanded,
                payload_available: item
                    .get("payload_available")
                    .and_then(Value::as_str)
                    .unwrap_or("online")
                    .to_owned(),
                payload_not_available: item
                    .get("payload_not_available")
                    .and_then(Value::as_str)
                    .unwrap_or("offline")
                    .to_owned(),
                value_template: item
                    .get("value_template")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    } else if let Some(raw_topic) = get_string("availability_topic")? {
        let expanded = expand_topic(&raw_topic, base_topic.as_deref(), "availability_topic")?;
        validate_topic(&expanded, "availability_topic")?;
        availability.push(AvailabilityTopic {
            topic: expanded,
            payload_available: get_string("payload_available")?.unwrap_or_else(|| "online".into()),
            payload_not_available: get_string("payload_not_available")?
                .unwrap_or_else(|| "offline".into()),
            value_template: get_string("availability_template")?,
        });
    }

    let mut attributes = Map::new();
    attributes.insert("friendly_name".into(), Value::String(name.clone()));
    attributes.insert("unique_id".into(), Value::String(unique_id.clone()));
    attributes.insert("platform".into(), Value::String(platform.into()));
    attributes.insert("optimistic".into(), Value::Bool(optimistic));
    for key in [
        "unit_of_measurement",
        "device_class",
        "state_class",
        "icon",
        "entity_category",
        "entity_picture",
        "suggested_display_precision",
        "options",
    ] {
        if let Some(value) = object.get(key) {
            if key == "device_class" {
                if platform == "switch"
                    && let Some(device_class) = switch_device_class
                {
                    attributes.insert(key.to_owned(), Value::String(device_class.as_str().into()));
                } else if platform == "button"
                    && let Some(device_class) = button_device_class
                {
                    attributes.insert(key.to_owned(), Value::String(device_class.as_str().into()));
                } else {
                    attributes.insert(key.to_owned(), value.clone());
                }
            } else {
                attributes.insert(key.to_owned(), value.clone());
            }
        }
    }
    let mut platform_config = object.clone();
    platform_config.insert("schema".into(), Value::String(schema.clone()));
    let mut climate_state_topics = BTreeMap::new();
    if platform == "climate" {
        for (key, field) in climate_state_topic_fields() {
            if let Some(expanded) = topic(key)? {
                platform_config.insert((*key).to_owned(), Value::String(expanded.clone()));
                climate_state_topics
                    .entry(expanded)
                    .or_insert_with(Vec::new)
                    .push(*field);
            }
        }
        for key in climate_command_topic_keys() {
            if let Some(expanded) = topic(key)? {
                platform_config.insert((*key).to_owned(), Value::String(expanded));
            }
        }
        let native_unit = match object.get("temperature_unit") {
            Some(Value::String(value)) => parse_mqtt_climate_temperature_unit(value)?,
            Some(_) => {
                return Err(Error::InvalidInput(
                    "discovery field temperature_unit must be a string".into(),
                ));
            }
            None => climate_unit_for_system(unit_system),
        };
        platform_config.insert(
            "temperature_unit".into(),
            Value::String(native_unit.as_str().into()),
        );
        insert_climate_discovery_defaults(&mut platform_config, config.optimistic, retain);
        validate_climate_discovery_config(&mut platform_config)?;
    }
    if platform == "light" && schema == "json" {
        let supported_color_modes = get_string_list("supported_color_modes")?;
        if let Some(modes) = &supported_color_modes {
            const VALID_MODES: &[&str] = &[
                "onoff",
                "brightness",
                "color_temp",
                "hs",
                "rgb",
                "rgbw",
                "rgbww",
                "white",
                "xy",
            ];
            if modes.is_empty()
                || modes
                    .iter()
                    .any(|mode| !VALID_MODES.contains(&mode.as_str()))
                || modes.iter().collect::<std::collections::HashSet<_>>().len() != modes.len()
            {
                return Err(Error::InvalidInput(
                    "supported_color_modes must be a non-empty unique list of supported Home Assistant color modes".into(),
                ));
            }
        }
        let brightness = get_bool("brightness", false)?;
        let color_temp_kelvin = get_bool("color_temp_kelvin", false)?;
        let effect = get_bool("effect", false)?;
        let flash = get_bool("flash", true)?;
        let transition = get_bool("transition", true)?;
        let brightness_scale = get_number("brightness_scale", 255.0)?;
        let white_scale = get_number("white_scale", 255.0)?;
        for (key, value) in [
            ("brightness", brightness),
            ("color_temp_kelvin", color_temp_kelvin),
            ("effect", effect),
            ("flash", flash),
            ("transition", transition),
        ] {
            platform_config.insert(key.into(), Value::Bool(value));
        }
        if brightness_scale < 1.0 || brightness_scale > 65_535.0 || brightness_scale.fract() != 0.0
        {
            return Err(Error::InvalidInput(
                "brightness_scale must be a positive integer no greater than 65535".into(),
            ));
        }
        if white_scale < 1.0 || white_scale > 65_535.0 || white_scale.fract() != 0.0 {
            return Err(Error::InvalidInput(
                "white_scale must be a positive integer no greater than 65535".into(),
            ));
        }
        let flash_time_short = get_number("flash_time_short", 2.0)?;
        let flash_time_long = get_number("flash_time_long", 10.0)?;
        if flash_time_short <= 0.0 || flash_time_long <= 0.0 {
            return Err(Error::InvalidInput(
                "flash_time_short and flash_time_long must be positive".into(),
            ));
        }
        if flash_time_short.fract() != 0.0 || flash_time_long.fract() != 0.0 {
            return Err(Error::InvalidInput(
                "flash_time_short and flash_time_long must be positive integers".into(),
            ));
        }
        let effect_list = get_string_list("effect_list")?;
        if effect_list
            .as_ref()
            .is_some_and(|effects| !effect || effects.iter().any(String::is_empty))
        {
            return Err(Error::InvalidInput(
                "effect_list requires effect: true and non-empty effect names".into(),
            ));
        }
        let min_mireds = object
            .contains_key("min_mireds")
            .then(|| get_number("min_mireds", 0.0))
            .transpose()?;
        let max_mireds = object
            .contains_key("max_mireds")
            .then(|| get_number("max_mireds", 0.0))
            .transpose()?;
        let min_kelvin = if let Some(max_mireds) = max_mireds {
            if max_mireds <= 0.0 {
                return Err(Error::InvalidInput("max_mireds must be positive".into()));
            }
            1_000_000.0 / max_mireds
        } else {
            get_number("min_kelvin", 1500.0)?
        };
        let max_kelvin = if let Some(min_mireds) = min_mireds {
            if min_mireds <= 0.0 {
                return Err(Error::InvalidInput("min_mireds must be positive".into()));
            }
            1_000_000.0 / min_mireds
        } else {
            get_number("max_kelvin", 9000.0)?
        };
        if min_kelvin <= 0.0
            || max_kelvin <= 0.0
            || min_kelvin > max_kelvin
            || min_kelvin.fract() != 0.0
            || max_kelvin.fract() != 0.0
            || min_mireds.is_some_and(|value| value.fract() != 0.0)
            || max_mireds.is_some_and(|value| value.fract() != 0.0)
        {
            return Err(Error::InvalidInput(
                "MQTT light color temperature limits are invalid".into(),
            ));
        }
        if let Some(max_mireds) = max_mireds {
            platform_config.insert("max_mireds".into(), json!(max_mireds));
        }
        if let Some(min_mireds) = min_mireds {
            platform_config.insert("min_mireds".into(), json!(min_mireds));
        }
        platform_config.insert("min_kelvin".into(), json!(min_kelvin));
        platform_config.insert("max_kelvin".into(), json!(max_kelvin));
        attributes.insert("brightness_supported".into(), Value::Bool(brightness));
        attributes.insert(
            "color_temp_kelvin_supported".into(),
            Value::Bool(color_temp_kelvin),
        );
        attributes.insert("effect_supported".into(), Value::Bool(effect));
        attributes.insert("flash_supported".into(), Value::Bool(flash));
        attributes.insert("transition_supported".into(), Value::Bool(transition));
        attributes.insert("brightness_scale".into(), json!(brightness_scale as u32));
        if let Some(modes) = supported_color_modes {
            attributes.insert("supported_color_modes".into(), json!(modes));
        } else {
            let mode = if brightness { "brightness" } else { "onoff" };
            attributes.insert("supported_color_modes".into(), json!([mode]));
        }
        if let Some(effects) = effect_list {
            attributes.insert("effect_list".into(), json!(effects));
        }
    }
    if platform == "number" {
        let min = get_number("min", 0.0)?;
        let max = get_number("max", 100.0)?;
        let step = get_number("step", 1.0)?;
        let mode = get_string("mode")?.unwrap_or_else(|| "auto".into());
        if min > max || step < 0.001 {
            return Err(Error::InvalidInput(
                "MQTT number requires min <= max and step >= 0.001".into(),
            ));
        }
        if !matches!(mode.as_str(), "auto" | "slider" | "box") {
            return Err(Error::InvalidInput(format!(
                "unsupported MQTT number mode {mode:?}"
            )));
        }
        platform_config.insert("min".into(), json!(min));
        platform_config.insert("max".into(), json!(max));
        platform_config.insert("step".into(), json!(step));
        platform_config.insert("mode".into(), Value::String(mode));
        for key in ["unit_of_measurement", "device_class"] {
            if let Some(value) = object.get(key) {
                attributes.insert(key.into(), value.clone());
            }
        }
    } else if platform == "select" {
        let options = get_string_list("options")?
            .ok_or_else(|| Error::InvalidInput("MQTT select discovery requires options".into()))?;
        if options.is_empty()
            || options
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != options.len()
        {
            return Err(Error::InvalidInput(
                "MQTT select options must be a non-empty list of unique strings".into(),
            ));
        }
        attributes.insert("options".into(), json!(options));
    } else if platform == "text" {
        let min = get_number("min", 0.0)?;
        let max = get_number("max", 255.0)?;
        let mode = get_string("mode")?.unwrap_or_else(|| "text".into());
        if min < 0.0 || max < min || max > 255.0 || min.fract() != 0.0 || max.fract() != 0.0 {
            return Err(Error::InvalidInput(
                "MQTT text requires integer limits with 0 <= min <= max <= 255".into(),
            ));
        }
        if !matches!(mode.as_str(), "text" | "password") {
            return Err(Error::InvalidInput(format!(
                "unsupported MQTT text mode {mode:?}"
            )));
        }
        platform_config.insert("min".into(), json!(min));
        platform_config.insert("max".into(), json!(max));
        platform_config.insert("mode".into(), Value::String(mode));
    } else if platform == "fan" {
        let min = get_number("speed_range_min", 1.0)?;
        let max = get_number("speed_range_max", 100.0)?;
        let presets = get_string_list("preset_modes")?.unwrap_or_default();
        let reset_preset =
            get_string("payload_reset_preset_mode")?.unwrap_or_else(|| "None".into());
        if min < 1.0 || max <= min || min.fract() != 0.0 || max.fract() != 0.0 {
            return Err(Error::InvalidInput(
                "MQTT fan requires 1 <= speed_range_min < speed_range_max".into(),
            ));
        }
        if presets
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != presets.len()
            || (object.contains_key("preset_mode_command_topic") && presets.is_empty())
            || presets.iter().any(|preset| preset == &reset_preset)
        {
            return Err(Error::InvalidInput(
                "MQTT fan preset_modes must be unique, exclude the reset payload, and be present with preset_mode_command_topic".into(),
            ));
        }
        platform_config.insert("speed_range_min".into(), json!(min));
        platform_config.insert("speed_range_max".into(), json!(max));
        platform_config.insert("preset_modes".into(), json!(presets));
    } else if platform == "cover" {
        if object.contains_key("set_position_topic") && !object.contains_key("position_topic") {
            return Err(Error::InvalidInput(
                "MQTT cover set_position_topic requires position_topic".into(),
            ));
        }
        let position_closed = get_number("position_closed", 0.0)?;
        let position_open = get_number("position_open", 100.0)?;
        let tilt_min = get_number("tilt_min", 0.0)?;
        let tilt_max = get_number("tilt_max", 100.0)?;
        let tilt_closed = get_number("tilt_closed_position", 0.0)?;
        let tilt_open = get_number("tilt_open_position", 100.0)?;
        if position_closed >= position_open
            || tilt_min >= tilt_max
            || [
                position_closed,
                position_open,
                tilt_min,
                tilt_max,
                tilt_closed,
                tilt_open,
            ]
            .iter()
            .any(|value| value.fract() != 0.0)
        {
            return Err(Error::InvalidInput(
                "MQTT cover position ranges are invalid".into(),
            ));
        }
        for (key, value) in [
            ("position_closed", position_closed),
            ("position_open", position_open),
            ("tilt_min", tilt_min),
            ("tilt_max", tilt_max),
            ("tilt_closed_position", tilt_closed),
            ("tilt_open_position", tilt_open),
        ] {
            platform_config.entry(key).or_insert(json!(value));
        }
    }
    let device = object.get("device").cloned();
    let entity_id = format!(
        "{platform}.{}",
        slug(
            object
                .get("object_id")
                .and_then(Value::as_str)
                .unwrap_or(&name)
        )
    );
    let mut entity = DiscoveredEntity {
        metadata: MqttEntityMetadata {
            entity_id,
            platform: platform.to_owned(),
            unique_id,
            discovery_topic: discovery_topic.to_owned(),
            name,
            device,
            entity_lifecycle_managed: false,
        },
        shared_entity: None,
        climate: None,
        switch_device_class,
        button_device_class,
        button_last_value: Arc::new(Mutex::new(None)),
        state_topic,
        extra_state_topics,
        extra_command_topics,
        json_attributes_topic,
        availability,
        availability_mode,
        command_topic,
        payload_on,
        payload_off,
        state_on,
        state_off,
        value_template,
        attributes_template,
        command_template,
        optimistic,
        retain,
        qos: entity_qos,
        max_payload_bytes: config.max_payload_bytes,
        attributes,
        text_pattern: get_string("pattern")?
            .map(|pattern| {
                Regex::new(&pattern).map_err(|error| {
                    Error::InvalidInput(format!("MQTT text pattern is invalid: {error}"))
                })
            })
            .transpose()?,
        platform_config,
    };
    if platform == "climate" {
        let descriptor = mqtt_climate_descriptor(&entity, "mqtt:standalone", None, None)?;
        let state = initial_mqtt_climate_state(&entity, &descriptor, unit_system)?;
        entity.climate = Some(MqttClimateEntity {
            descriptor,
            state: Arc::new(Mutex::new(state)),
            state_topics: climate_state_topics,
        });
    }
    Ok(entity)
}

fn climate_state_topic_fields() -> &'static [(&'static str, ClimateStateField)] {
    &[
        ("action_topic", ClimateStateField::Action),
        ("current_humidity_topic", ClimateStateField::CurrentHumidity),
        (
            "target_humidity_state_topic",
            ClimateStateField::TargetHumidity,
        ),
        ("mode_state_topic", ClimateStateField::HvacMode),
        ("fan_mode_state_topic", ClimateStateField::FanMode),
        (
            "swing_horizontal_mode_state_topic",
            ClimateStateField::SwingHorizontalMode,
        ),
        ("swing_mode_state_topic", ClimateStateField::SwingMode),
        ("preset_mode_state_topic", ClimateStateField::PresetMode),
        (
            "current_temperature_topic",
            ClimateStateField::CurrentTemperature,
        ),
        (
            "temperature_state_topic",
            ClimateStateField::TargetTemperature,
        ),
        (
            "temperature_low_state_topic",
            ClimateStateField::TargetTemperatureLow,
        ),
        (
            "temperature_high_state_topic",
            ClimateStateField::TargetTemperatureHigh,
        ),
    ]
}

fn climate_command_topic_keys() -> &'static [&'static str] {
    &[
        "fan_mode_command_topic",
        "target_humidity_command_topic",
        "mode_command_topic",
        "power_command_topic",
        "preset_mode_command_topic",
        "swing_horizontal_mode_command_topic",
        "swing_mode_command_topic",
        "temperature_command_topic",
        "temperature_high_command_topic",
        "temperature_low_command_topic",
    ]
}

fn climate_unit_for_system(kind: UnitSystemKind) -> ClimateTemperatureUnit {
    match kind {
        UnitSystemKind::Metric => ClimateTemperatureUnit::Celsius,
        UnitSystemKind::UsCustomary => ClimateTemperatureUnit::Fahrenheit,
    }
}

fn parse_mqtt_climate_temperature_unit(value: &str) -> Result<ClimateTemperatureUnit> {
    match value.to_ascii_uppercase().as_str() {
        "C" => Ok(ClimateTemperatureUnit::Celsius),
        "F" => Ok(ClimateTemperatureUnit::Fahrenheit),
        _ => Err(Error::InvalidInput(format!(
            "invalid MQTT climate temperature_unit {value:?}; expected \"C\" or \"F\""
        ))),
    }
}

fn climate_unit_context(kind: UnitSystemKind) -> ClimateUnitContext {
    ClimateUnitContext::new(climate_unit_for_system(kind))
}

fn climate_topic_configured(entity: &DiscoveredEntity, key: &str) -> bool {
    entity
        .platform_config
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|topic| !topic.is_empty())
}

fn insert_climate_discovery_defaults(
    config: &mut Map<String, Value>,
    optimistic: bool,
    retain: bool,
) {
    for (key, value) in [
        (
            "modes",
            json!(["auto", "off", "cool", "heat", "dry", "fan_only"]),
        ),
        ("fan_modes", json!(["auto", "low", "medium", "high"])),
        ("preset_modes", json!([])),
        ("swing_modes", json!(["on", "off"])),
        ("swing_horizontal_modes", json!(["on", "off"])),
        ("min_humidity", json!(30.0)),
        ("max_humidity", json!(99.0)),
        ("temp_step", json!(1.0)),
        ("payload_on", json!("ON")),
        ("payload_off", json!("OFF")),
    ] {
        config.entry(key.to_owned()).or_insert(value);
    }
    config.insert("optimistic".into(), Value::Bool(optimistic));
    config.insert("retain".into(), Value::Bool(retain));
}

fn climate_string_list(config: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let values = config
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidInput(format!("MQTT climate {key} must be a list")))?;
    let values = values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                Error::InvalidInput(format!("MQTT climate {key} must contain only strings"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if values
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != values.len()
    {
        return Err(Error::InvalidInput(format!(
            "MQTT climate {key} must not contain duplicates"
        )));
    }
    Ok(values)
}

fn climate_number(config: &Map<String, Value>, key: &str, default: Option<f64>) -> Result<f64> {
    let value = match config.get(key) {
        Some(Value::Number(value)) => value.as_f64(),
        Some(Value::String(value)) => value.parse::<f64>().ok(),
        Some(Value::Bool(value)) => Some(if *value { 1.0 } else { 0.0 }),
        Some(Value::Null) | None => default,
        Some(_) => None,
    }
    .filter(|value| value.is_finite())
    .ok_or_else(|| Error::InvalidInput(format!("MQTT climate {key} must be a finite number")))?;
    Ok(value)
}

fn validate_climate_discovery_config(config: &mut Map<String, Value>) -> Result<()> {
    for key in [
        "initial",
        "min_temp",
        "max_temp",
        "temp_step",
        "precision",
        "min_humidity",
        "max_humidity",
    ] {
        if config.contains_key(key) {
            let value = climate_number(config, key, None)?;
            config.insert(key.into(), json!(value));
        }
    }
    let modes = climate_string_list(config, "modes")?;
    for mode in &modes {
        mode.parse::<HvacMode>().map_err(|error| {
            Error::InvalidInput(format!("invalid MQTT climate mode {mode:?}: {error}"))
        })?;
    }
    for key in [
        "fan_modes",
        "swing_modes",
        "swing_horizontal_modes",
        "preset_modes",
    ] {
        let modes = climate_string_list(config, key)?;
        if key == "preset_modes" && modes.iter().any(|mode| mode == "none") {
            return Err(Error::InvalidInput(
                "preset_modes must not include preset mode 'none'".into(),
            ));
        }
    }
    let min_humidity = climate_number(config, "min_humidity", Some(30.0))?;
    let max_humidity = climate_number(config, "max_humidity", Some(99.0))?;
    if min_humidity <= 0.0 || max_humidity <= 0.0 || min_humidity >= max_humidity {
        return Err(Error::InvalidInput(
            "MQTT climate humidity range must be positive with minimum below maximum".into(),
        ));
    }
    if max_humidity > 100.0 {
        return Err(Error::InvalidInput(
            "MQTT climate max_humidity must be <= 100".into(),
        ));
    }
    if config.get("target_humidity_state_topic").is_some()
        && config.get("target_humidity_command_topic").is_none()
    {
        return Err(Error::InvalidInput(
            "target_humidity_state_topic cannot be used without target_humidity_command_topic"
                .into(),
        ));
    }
    if config.get("preset_mode_command_topic").is_none()
        && config
            .get("preset_modes")
            .and_then(Value::as_array)
            .is_some_and(|modes| !modes.is_empty())
    {
        return Err(Error::InvalidInput(
            "preset_modes requires preset_mode_command_topic".into(),
        ));
    }
    if config.contains_key("precision") {
        let precision = climate_number(config, "precision", None)?;
        if ![0.1, 0.5, 1.0].contains(&precision) {
            return Err(Error::InvalidInput(
                "MQTT climate precision must be 0.1, 0.5, or 1".into(),
            ));
        }
        config.insert("precision".into(), json!(precision));
    }
    Ok(())
}

fn mqtt_climate_descriptor(
    entity: &DiscoveredEntity,
    provider_id: &str,
    config_entry_id: Option<&str>,
    device_id: Option<String>,
) -> Result<ClimateDescriptor> {
    let config = &entity.platform_config;
    let native_unit = config_string(config, "temperature_unit", "°C")
        .parse::<ClimateTemperatureUnit>()
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    let modes = climate_string_list(config, "modes")?
        .into_iter()
        .map(|mode| {
            mode.parse::<HvacMode>()
                .map_err(|error| Error::InvalidInput(error.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut descriptor =
        ClimateDescriptor::new("mqtt", provider_id, entity.metadata.unique_id.clone())
            .with_unique_id(entity.metadata.unique_id.clone())
            .with_name(entity.metadata.name.clone())
            .with_suggested_object_id(mqtt_suggested_object_id(entity))
            .with_native_temperature_unit(native_unit)
            .with_hvac_modes(modes)
            .with_fan_modes(Some(climate_string_list(config, "fan_modes")?))
            .with_swing_modes(Some(climate_string_list(config, "swing_modes")?))
            .with_swing_horizontal_modes(Some(climate_string_list(
                config,
                "swing_horizontal_modes",
            )?))
            .with_target_temperature_step(Some(config_number(config, "temp_step", 1.0)))
            .with_humidity_limits(
                config_number(config, "min_humidity", 30.0),
                config_number(config, "max_humidity", 99.0),
            );
    if let Some(config_entry_id) = config_entry_id {
        descriptor = descriptor.with_config_entry_id(config_entry_id.to_owned());
    }
    if let Some(device_id) = device_id {
        descriptor = descriptor.with_device_id(device_id);
    }
    if let Some(icon) = entity.attributes.get("icon").and_then(Value::as_str) {
        descriptor = descriptor.with_icon(icon.to_owned());
    }
    if let Some(category) = entity
        .attributes
        .get("entity_category")
        .and_then(Value::as_str)
    {
        descriptor = descriptor.with_entity_category(category.to_owned());
    }
    if config.contains_key("min_temp") {
        descriptor = descriptor.with_min_temperature(climate_number(config, "min_temp", None)?);
    }
    if config.contains_key("max_temp") {
        descriptor = descriptor.with_max_temperature(climate_number(config, "max_temp", None)?);
    }
    if config.contains_key("precision") {
        descriptor = descriptor.with_precision(climate_number(config, "precision", None)?);
    }
    let mut features = ClimateFeatures::TURN_ON.union(ClimateFeatures::TURN_OFF);
    if climate_topic_configured(entity, "temperature_state_topic")
        || climate_topic_configured(entity, "temperature_command_topic")
    {
        features = features.with(ClimateFeature::TargetTemperature);
    }
    if climate_topic_configured(entity, "temperature_low_state_topic")
        || climate_topic_configured(entity, "temperature_low_command_topic")
        || climate_topic_configured(entity, "temperature_high_state_topic")
        || climate_topic_configured(entity, "temperature_high_command_topic")
    {
        features = features.with(ClimateFeature::TargetTemperatureRange);
    }
    if climate_topic_configured(entity, "target_humidity_command_topic") {
        features = features.with(ClimateFeature::TargetHumidity);
    }
    if climate_topic_configured(entity, "fan_mode_state_topic")
        || climate_topic_configured(entity, "fan_mode_command_topic")
    {
        features = features.with(ClimateFeature::FanMode);
    }
    if climate_topic_configured(entity, "preset_mode_command_topic") {
        features = features.with(ClimateFeature::PresetMode);
        let mut presets = climate_string_list(config, "preset_modes")?;
        if !presets.is_empty() {
            presets.insert(0, hass_climate::PRESET_NONE.into());
        }
        descriptor = descriptor.with_preset_modes(Some(presets));
    } else {
        descriptor = descriptor.with_preset_modes(None);
    }
    if climate_topic_configured(entity, "swing_mode_state_topic")
        || climate_topic_configured(entity, "swing_mode_command_topic")
    {
        features = features.with(ClimateFeature::SwingMode);
    }
    if climate_topic_configured(entity, "swing_horizontal_mode_state_topic")
        || climate_topic_configured(entity, "swing_horizontal_mode_command_topic")
    {
        features = features.with(ClimateFeature::SwingHorizontalMode);
    }
    descriptor = descriptor.with_supported_features(features);
    descriptor
        .try_entity_spec()
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    Ok(descriptor)
}

fn climate_default_initial_temperature(native_unit: ClimateTemperatureUnit) -> Result<f64> {
    convert(
        UnitFamily::Temperature,
        21.0,
        Some(ClimateTemperatureUnit::Celsius.as_str()),
        Some(native_unit.as_str()),
    )
    .map_err(|error| {
        Error::InvalidInput(format!(
            "MQTT climate temperature conversion failed: {error}"
        ))
    })
}

fn initial_mqtt_climate_state(
    entity: &DiscoveredEntity,
    descriptor: &ClimateDescriptor,
    _unit_system: UnitSystemKind,
) -> Result<MqttClimateState> {
    let config = &entity.platform_config;
    let optimistic = entity.optimistic;
    let initial_temperature = config
        .get("initial")
        .map(|_| climate_number(config, "initial", None))
        .transpose()?
        .unwrap_or(climate_default_initial_temperature(
            config_string(config, "temperature_unit", "°C")
                .parse::<ClimateTemperatureUnit>()
                .map_err(|error| Error::InvalidInput(error.to_string()))?,
        )?);
    let mut snapshot = ClimateSnapshot::new(
        (optimistic || !climate_topic_configured(entity, "mode_state_topic"))
            .then_some(HvacMode::Off),
    );
    if optimistic || !climate_topic_configured(entity, "temperature_state_topic") {
        snapshot.target_temperature = Some(initial_temperature);
    }
    if optimistic || !climate_topic_configured(entity, "temperature_low_state_topic") {
        snapshot.target_temperature_low = Some(initial_temperature);
    }
    if optimistic || !climate_topic_configured(entity, "temperature_high_state_topic") {
        snapshot.target_temperature_high = Some(initial_temperature);
    }
    if optimistic || !climate_topic_configured(entity, "fan_mode_state_topic") {
        snapshot.fan_mode = Some("low".into());
    }
    if optimistic || !climate_topic_configured(entity, "swing_horizontal_mode_state_topic") {
        snapshot.swing_horizontal_mode = Some("off".into());
    }
    if optimistic || !climate_topic_configured(entity, "swing_mode_state_topic") {
        snapshot.swing_mode = Some("off".into());
    }
    if climate_topic_configured(entity, "preset_mode_command_topic") {
        snapshot.preset_mode = Some(hass_climate::PRESET_NONE.into());
    }
    snapshot.extra_attributes = climate_extra_attributes(&entity.attributes);
    let modes = descriptor.hvac_modes().unwrap_or_default();
    let single_and_range_setpoints = modes.contains(&HvacMode::HeatCool)
        && (modes.contains(&HvacMode::Heat) || modes.contains(&HvacMode::Cool));
    Ok(MqttClimateState {
        snapshot,
        parked_target_temperature: None,
        parked_target_temperature_range: (None, None),
        single_and_range_setpoints,
    })
}

fn climate_extra_attributes(attributes: &Map<String, Value>) -> Map<String, Value> {
    let mut extra = attributes.clone();
    for key in [
        "friendly_name",
        "unique_id",
        "platform",
        "optimistic",
        hass_climate::attr::CURRENT_HUMIDITY,
        hass_climate::attr::CURRENT_TEMPERATURE,
        hass_climate::attr::FAN_MODE,
        hass_climate::attr::FAN_MODES,
        hass_climate::attr::HVAC_ACTION,
        hass_climate::attr::HVAC_MODES,
        hass_climate::attr::MAX_HUMIDITY,
        hass_climate::attr::MAX_TEMP,
        hass_climate::attr::MIN_HUMIDITY,
        hass_climate::attr::MIN_TEMP,
        hass_climate::attr::PRESET_MODE,
        hass_climate::attr::PRESET_MODES,
        hass_climate::attr::SUPPORTED_FEATURES,
        hass_climate::attr::SWING_HORIZONTAL_MODE,
        hass_climate::attr::SWING_HORIZONTAL_MODES,
        hass_climate::attr::SWING_MODE,
        hass_climate::attr::SWING_MODES,
        hass_climate::attr::TARGET_HUMIDITY,
        hass_climate::attr::TARGET_HUMIDITY_STEP,
        hass_climate::attr::TARGET_TEMPERATURE,
        hass_climate::attr::TARGET_TEMP_HIGH,
        hass_climate::attr::TARGET_TEMP_LOW,
        hass_climate::attr::TARGET_TEMP_STEP,
        hass_climate::attr::TEMPERATURE_UNIT,
    ] {
        extra.remove(key);
    }
    extra
}

fn register_services(
    runtime: &Runtime,
    owner: &str,
    command_tx: mpsc::Sender<CommandRequest>,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    templates: TemplateEngine,
    command_admission: Arc<AsyncRwLock<bool>>,
    profiles: Arc<LightProfileCatalog>,
) -> Result<()> {
    let serial = Arc::new(tokio::sync::Mutex::new(()));
    let climate_services = ClimateService::ALL.map(|service| ("climate", service.as_str()));
    for (domain, service) in [
        ("switch", "turn_on"),
        ("switch", "turn_off"),
        ("switch", "toggle"),
        ("light", "turn_on"),
        ("light", "turn_off"),
        ("light", "toggle"),
        ("cover", "open_cover"),
        ("cover", "close_cover"),
        ("cover", "stop_cover"),
        ("cover", "toggle"),
        ("cover", "set_cover_position"),
        ("cover", "open_cover_tilt"),
        ("cover", "close_cover_tilt"),
        ("cover", "stop_cover_tilt"),
        ("cover", "set_cover_tilt_position"),
        ("cover", "toggle_cover_tilt"),
        ("fan", "turn_on"),
        ("fan", "turn_off"),
        ("fan", "toggle"),
        ("fan", "increase_speed"),
        ("fan", "decrease_speed"),
        ("fan", "set_percentage"),
        ("fan", "set_preset_mode"),
        ("fan", "oscillate"),
        ("fan", "set_direction"),
        ("button", "press"),
        ("number", "set_value"),
        ("select", "select_option"),
        ("text", "set_value"),
    ]
    .into_iter()
    .chain(climate_services)
    {
        let tx = command_tx.clone();
        let entities = Arc::clone(&entities);
        let templates = templates.clone();
        let serial = Arc::clone(&serial);
        let command_admission = Arc::clone(&command_admission);
        let profiles = Arc::clone(&profiles);
        let matcher_entities = Arc::clone(&entities);
        let matcher_domain = domain.to_owned();
        let description = if domain == "switch" {
            service
                .parse::<SwitchCommand>()
                .expect("registered switch service is part of the switch SDK")
                .description()
                .metadata_json()
        } else if domain == "climate" {
            json!({"description": "Publish a typed MQTT climate command for discovered entities"})
        } else {
            json!({"description": "Publish an MQTT command for discovered entities"})
        };
        let handler: hass_core::AsyncServiceHandler = Arc::new(move |runtime, call| {
            let tx = tx.clone();
            let entities = Arc::clone(&entities);
            let templates = templates.clone();
            let serial = Arc::clone(&serial);
            let command_admission = Arc::clone(&command_admission);
            let profiles = Arc::clone(&profiles);
            Box::pin(async move {
                enqueue_service_commands(
                    &runtime,
                    &call,
                    &tx,
                    &entities,
                    &templates,
                    &serial,
                    &command_admission,
                    &profiles,
                )
                .await
            })
        });
        runtime.register_entity_async_service(
            owner,
            domain,
            service,
            description,
            Arc::new(move |entity_id| {
                matcher_entities
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .any(|entity| {
                        entity.metadata.platform == matcher_domain
                            && mqtt_entity_is_active(entity)
                            && mqtt_entity_id(entity) == entity_id
                    })
            }),
            handler,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_service_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    tx: &mpsc::Sender<CommandRequest>,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    templates: &TemplateEngine,
    serial: &tokio::sync::Mutex<()>,
    command_admission: &AsyncRwLock<bool>,
    profiles: &LightProfileCatalog,
) -> Result<ServiceResult> {
    let guard = serial.lock().await;
    let ids = requested_entity_ids(call)?;
    if ids.len() > MAX_TARGETS_PER_SERVICE {
        return Err(Error::Service(format!(
            "MQTT service target limit is {MAX_TARGETS_PER_SERVICE}"
        )));
    }
    let mut light_diagnostics = Vec::new();
    let targets = {
        let discovered = entities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut targets = Vec::with_capacity(ids.len());
        for entity_id in ids {
            let mut entity = discovered
                .values()
                .find(|entity| mqtt_entity_is_active(entity) && mqtt_entity_id(entity) == entity_id)
                .cloned()
                .ok_or_else(|| Error::Service(format!("unknown MQTT entity {entity_id}")))?;
            entity.metadata.entity_id = mqtt_entity_id(&entity);
            if entity.metadata.platform != call.domain {
                return Err(Error::Service(format!(
                    "{} is not a {} entity",
                    entity_id, call.domain
                )));
            }
            let commands = build_entity_commands(
                runtime,
                call,
                &entity,
                templates,
                profiles,
                &mut light_diagnostics,
            )?;
            if commands.len() != 1 && call.domain == "button" {
                return Err(Error::Service(
                    "MQTT button press must produce exactly one command".into(),
                ));
            }
            targets.push((entity, commands));
        }
        targets
    };

    for diagnostic in light_diagnostics {
        if runtime.claim_service_diagnostic(diagnostic.code, &diagnostic.original_value) {
            warn!(
                code = diagnostic.code,
                field = diagnostic.original_field,
                value = %diagnostic.original_value,
                "Got unknown color {}, falling back to white",
                diagnostic.original_value
            );
        }
    }

    let mut changed_states = Vec::new();
    if call.domain == "button" {
        let request = ButtonPressRequest::try_from(call.clone())
            .map_err(|error| Error::Service(error.to_string()))?;
        drop(guard);
        for (entity, mut commands) in targets {
            let command = commands
                .pop()
                .expect("button command count was validated above");
            let timestamp = ButtonTimestamp::now();
            let pressed_state = ButtonSnapshot::pressed_at(timestamp.clone())
                .with_context(call.context.clone())
                .state()
                .expect("pressed button snapshot has a timestamp")
                .to_owned();
            let last_value = LastValue {
                state: pressed_state,
                attributes: entity.attributes.clone(),
                context: call.context.clone(),
            };
            if let Some(SharedMqttEntity::Button { handle, descriptor }) =
                entity.shared_entity.as_ref()
            {
                let handler = MqttButtonPressPublisher {
                    tx,
                    admission: command_admission,
                    command: Mutex::new(Some(command)),
                    button_last_value: Arc::clone(&entity.button_last_value),
                    last_value: last_value.clone(),
                    changed_state: Arc::new(Mutex::new(None)),
                    state_events: Mutex::new(Some(runtime.subscribe_with_states().1)),
                    entity_id: mqtt_entity_id(&entity),
                };
                match execute_button_press_at(descriptor, handle, &request, timestamp, &handler)
                    .await?
                {
                    ButtonPressOutcome::Invoked => {
                        if let Some(state) = handler
                            .changed_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            changed_states.push(state);
                        }
                    }
                    ButtonPressOutcome::NotPublished => {
                        return Err(Error::Service(
                            "MQTT button became inactive before press".into(),
                        ));
                    }
                }
            } else {
                let state = ButtonSnapshot::from_restored_state(Some(&last_value.state))
                    .unwrap_or_else(ButtonSnapshot::unknown)
                    .with_context(call.context.clone());
                let change = runtime.set_state(
                    &mqtt_entity_id(&entity),
                    state.state().unwrap_or("unknown"),
                    entity.attributes.clone(),
                    call.context.clone(),
                    false,
                )?;
                if change.changed {
                    changed_states.push(change.state);
                }
                *entity
                    .button_last_value
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(last_value);
                changed_states.extend(send_command_and_wait(tx, command_admission, command).await?);
            }
        }
    } else {
        let chains = targets
            .into_iter()
            .map(|(_, commands)| commands)
            .filter(|commands| !commands.is_empty())
            .collect::<Vec<_>>();
        let replies = queue_command_chains(tx, command_admission, chains).await?;
        drop(guard);
        changed_states.extend(await_command_replies(replies).await?);
    }

    Ok(ServiceResult {
        changed_states,
        response: None,
        context: call.context.clone(),
    })
}

struct MqttButtonPressPublisher<'a> {
    tx: &'a mpsc::Sender<CommandRequest>,
    admission: &'a AsyncRwLock<bool>,
    command: Mutex<Option<Command>>,
    button_last_value: Arc<Mutex<Option<LastValue>>>,
    last_value: LastValue,
    changed_state: Arc<Mutex<Option<State>>>,
    state_events: Mutex<Option<broadcast::Receiver<hass_core::Event>>>,
    entity_id: String,
}

impl ButtonPressHandler for MqttButtonPressPublisher<'_> {
    fn async_press(&self) -> ButtonPressFuture<'_> {
        let command = self
            .command
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Box::pin(async move {
            let command = command
                .ok_or_else(|| Error::Service("MQTT button action was already invoked".into()))?;
            *self
                .button_last_value
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(self.last_value.clone());
            if let Some(mut events) = self
                .state_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                && let Some(state) = take_button_state_change(
                    &mut events,
                    &self.entity_id,
                    &self.last_value.state,
                    &self.last_value.context,
                )
            {
                *self
                    .changed_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
            }
            send_command_and_wait(self.tx, self.admission, command)
                .await
                .map(|_| ())
        })
    }
}

fn take_button_state_change(
    events: &mut broadcast::Receiver<hass_core::Event>,
    entity_id: &str,
    expected_state: &str,
    context: &Context,
) -> Option<State> {
    while let Ok(event) = events.try_recv() {
        if event.event_type != "state_changed"
            || event.context != *context
            || event.data.get("entity_id").and_then(Value::as_str) != Some(entity_id)
        {
            continue;
        }
        let Some(new_state) = event.data.get("new_state") else {
            continue;
        };
        let Ok(state) = serde_json::from_value::<State>(new_state.clone()) else {
            continue;
        };
        if state.state == expected_state && state.context == *context {
            return Some(state);
        }
    }
    None
}

async fn queue_command_chains(
    tx: &mpsc::Sender<CommandRequest>,
    admission: &AsyncRwLock<bool>,
    chains: Vec<Vec<Command>>,
) -> Result<Vec<oneshot::Receiver<Result<Vec<State>>>>> {
    let chains = chains
        .into_iter()
        .filter(|commands| !commands.is_empty())
        .collect::<Vec<_>>();
    if chains.is_empty() {
        return Ok(Vec::new());
    }
    let accepting = admission.read().await;
    if !*accepting {
        return Err(Error::Service("MQTT connection is shutting down".into()));
    }
    let mut permits = tx
        .try_reserve_many(chains.len())
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => {
                Error::Service("MQTT command queue is full".into())
            }
            mpsc::error::TrySendError::Closed(()) => {
                Error::Service("MQTT connection is shutting down".into())
            }
        })?;
    let mut replies = Vec::with_capacity(chains.len());
    for commands in chains {
        let mut commands = commands.into_iter();
        let command = commands
            .next()
            .expect("empty MQTT command chains were filtered above");
        let remaining_commands = commands.collect();
        let (reply_tx, reply_rx) = oneshot::channel();
        permits
            .next()
            .expect("reserved MQTT command permit count matches the batch")
            .send(CommandRequest {
                command,
                remaining_commands,
                changed_states: Vec::new(),
                reply: reply_tx,
            });
        replies.push(reply_rx);
    }
    drop(accepting);
    Ok(replies)
}

async fn queue_command_batch(
    tx: &mpsc::Sender<CommandRequest>,
    admission: &AsyncRwLock<bool>,
    commands: Vec<Command>,
) -> Result<Vec<oneshot::Receiver<Result<Vec<State>>>>> {
    queue_command_chains(
        tx,
        admission,
        commands.into_iter().map(|command| vec![command]).collect(),
    )
    .await
}

async fn send_command_and_wait(
    tx: &mpsc::Sender<CommandRequest>,
    admission: &AsyncRwLock<bool>,
    command: Command,
) -> Result<Vec<State>> {
    let mut replies = queue_command_batch(tx, admission, vec![command]).await?;
    let reply = replies.pop().expect("one command has one reply");
    await_command_replies(vec![reply]).await
}

async fn await_command_replies(
    replies: Vec<oneshot::Receiver<Result<Vec<State>>>>,
) -> Result<Vec<State>> {
    let mut first_error = None;
    let mut changed_states = Vec::new();
    for reply in replies {
        match reply.await {
            Ok(Ok(states)) => changed_states.extend(states),
            Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
            Ok(Err(_)) => {}
            Err(_) if first_error.is_none() => {
                first_error = Some(Error::Service(
                    "MQTT event loop ended before command completion".into(),
                ));
            }
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(changed_states), Err)
}

fn light_has_unmodeled_service_fields(call: &ServiceCall) -> bool {
    call.data.keys().any(|key| key != "entity_id")
}

fn build_entity_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    profiles: &LightProfileCatalog,
    light_diagnostics: &mut Vec<LightServiceDiagnostic>,
) -> Result<Vec<Command>> {
    let platform = entity.metadata.platform.as_str();
    let service = call.service.as_str();
    let entity_id = mqtt_entity_id(entity);
    let current = runtime.get_state(&entity_id);
    let current_state = current
        .as_ref()
        .map(|state| state.state.as_str())
        .unwrap_or("unknown");
    let current_available = current
        .as_ref()
        .is_none_or(|state| state.state != "unavailable");
    let current_attributes = current
        .as_ref()
        .map(|state| state.attributes.clone())
        .unwrap_or_else(|| entity.attributes.clone());
    let climate_plan = if let Some(climate) = entity.climate.as_ref() {
        let request = ClimateCommand::parse_service_call(call)
            .map_err(|error| Error::Service(error.to_string()))?;
        let descriptor = match entity.shared_entity.as_ref() {
            Some(SharedMqttEntity::Climate { descriptor, .. }) => descriptor,
            _ => &climate.descriptor,
        };
        Some(
            request
                .resolve(descriptor, climate_unit_context(runtime.unit_system()))
                .map_err(|error| Error::Service(error.to_string()))?,
        )
    } else {
        None
    };
    let normalized_light = match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Light { descriptor, .. })
            if !(config_string(&entity.platform_config, "schema", "basic") == "basic"
                && entity.command_template.is_some()
                && light_has_unmodeled_service_fields(call)) =>
        {
            let snapshot = light_snapshot(
                entity,
                descriptor,
                Some(current_state),
                current_available,
                current_attributes.clone(),
                current
                    .as_ref()
                    .map(|state| state.context.clone())
                    .unwrap_or_default(),
            )?;
            let request = LightCommand::parse_service_call(call)
                .map_err(|error| Error::Service(error.to_string()))?;
            let context = LightNormalizationContext::new(&entity_id, profiles);
            let command = request
                .normalize_with_context(descriptor, &snapshot, &context)
                .map_err(|error| Error::Service(error.to_string()))?;
            light_diagnostics.extend(command.diagnostics.iter().cloned());
            Some(command)
        }
        _ => None,
    };
    let cover_action = match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Cover {
            descriptor,
            toggle_state,
            ..
        }) => {
            let snapshot = cover_snapshot(
                entity,
                Some(current_state),
                current_available,
                current_attributes.clone(),
                current
                    .as_ref()
                    .map(|state| state.context.clone())
                    .unwrap_or_default(),
            )?;
            let command = CoverCommand::parse_service_call(call)
                .map_err(|error| Error::Service(error.to_string()))?;
            Some(
                command
                    .resolve(
                        descriptor,
                        &snapshot,
                        &mut toggle_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    )
                    .map_err(|error| Error::Service(error.to_string()))?,
            )
        }
        _ => None,
    };
    let fan_plan = match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Fan { descriptor, .. }) => {
            let snapshot = fan_snapshot(
                entity,
                Some(current_state),
                current_available,
                current_attributes,
                current
                    .as_ref()
                    .map(|state| state.context.clone())
                    .unwrap_or_default(),
            )?;
            let request = FanCommand::parse_service_call(call)
                .map_err(|error| Error::Service(error.to_string()))?;
            Some(
                request
                    .command
                    .resolve(descriptor, &snapshot)
                    .map_err(|error| Error::Service(error.to_string()))?,
            )
        }
        _ => None,
    };
    if let Some(plan) = fan_plan {
        return fan_effect_commands(runtime, call, entity, templates, plan.effects);
    }
    if let Some(plan) = climate_plan {
        return climate_effect_commands(
            runtime,
            call,
            entity,
            templates,
            plan.effects,
            plan.context,
        );
    }
    let service = match cover_action.as_ref() {
        Some(CoverAction::Open { .. }) => "open_cover",
        Some(CoverAction::Close { .. }) => "close_cover",
        Some(CoverAction::Stop) => "stop_cover",
        Some(CoverAction::SetPosition { .. }) => "set_cover_position",
        Some(CoverAction::OpenTilt) => "open_cover_tilt",
        Some(CoverAction::CloseTilt) => "close_cover_tilt",
        Some(CoverAction::StopTilt) => "stop_cover_tilt",
        Some(CoverAction::SetTiltPosition { .. }) => "set_cover_tilt_position",
        None => service,
    };
    let mut output = Vec::new();
    match (platform, service) {
        ("switch" | "light", "turn_on" | "turn_off" | "toggle") => {
            if platform == "light"
                && config_string(&entity.platform_config, "schema", "basic") == "json"
            {
                let (payload, attrs, requested_state) =
                    if let Some(command) = normalized_light.as_ref() {
                        let (payload, attrs) = normalized_json_light_command(entity, command)?;
                        (payload, attrs, command.action == LightAction::On)
                    } else {
                        let requested_state = requested_on_off(current_state, service);
                        let (payload, attrs) = json_light_command(entity, call, requested_state)?;
                        (payload, attrs, requested_state)
                    };
                output.push(make_command(
                    runtime,
                    entity,
                    call,
                    entity.command_topic.clone().ok_or_else(|| {
                        Error::Service("MQTT JSON light has no command_topic".into())
                    })?,
                    payload,
                    Some(if requested_state { "on" } else { "off" }.into()),
                    attrs,
                    true,
                )?);
            } else {
                let requested_state = if platform == "switch" {
                    let command = SwitchCommand::parse_service_call(call)
                        .map_err(|error| Error::Service(error.to_string()))?;
                    let is_on = match current_state {
                        "on" => Some(true),
                        "off" => Some(false),
                        _ => None,
                    };
                    command.resolve(is_on).is_on()
                } else if let Some(command) = normalized_light.as_ref() {
                    command.action == LightAction::On
                } else {
                    requested_on_off(current_state, service)
                };
                let raw = if requested_state {
                    &entity.payload_on
                } else {
                    &entity.payload_off
                };
                if platform == "light"
                    && call.data.keys().any(|key| key != "entity_id")
                    && entity.command_template.is_none()
                {
                    return Err(Error::Service(
                        "MQTT light options require a command_template or schema: json".into(),
                    ));
                }
                let payload =
                    render_command(runtime, call, entity, templates, "command_template", raw)?;
                output.push(make_command(
                    runtime,
                    entity,
                    call,
                    entity.command_topic.clone().ok_or_else(|| {
                        Error::Service(format!(
                            "MQTT entity {} has no command_topic",
                            entity.metadata.entity_id
                        ))
                    })?,
                    payload,
                    Some(if requested_state { "on" } else { "off" }.into()),
                    Map::new(),
                    true,
                )?);
            }
        }
        ("cover", "open_cover" | "close_cover" | "stop_cover") => {
            if cover_action.is_none() {
                check_service_options(call, &[])?;
            }
            let (payload_key, fallback, state) = match cover_action.as_ref() {
                Some(CoverAction::Open { .. }) | None if service == "open_cover" => {
                    ("payload_open", "OPEN", Some("open"))
                }
                Some(CoverAction::Close { .. }) | None if service == "close_cover" => {
                    ("payload_close", "CLOSE", Some("closed"))
                }
                _ => ("payload_stop", "STOP", Some("stopped")),
            };
            let payload = config_string(&entity.platform_config, payload_key, fallback).to_owned();
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .command_topic
                    .clone()
                    .ok_or_else(|| Error::Service("MQTT cover has no command_topic".into()))?,
                payload,
                state.map(str::to_owned),
                Map::new(),
                true,
            )?);
        }
        ("cover", "set_cover_position") => {
            if cover_action.is_none() {
                check_service_options(call, &["position"])?;
            }
            let percent = match cover_action.as_ref() {
                Some(CoverAction::SetPosition { position, .. }) => f64::from(position.get()),
                _ => numeric_arg(call, "position", 0.0, 100.0)?,
            };
            let closed = config_number(&entity.platform_config, "position_closed", 0.0);
            let open = config_number(&entity.platform_config, "position_open", 100.0);
            let ranged = (closed + (open - closed) * percent / 100.0).round();
            let payload = render_typed_command(
                runtime,
                call,
                entity,
                templates,
                "set_position_template",
                &formatted_number(ranged),
                "position",
                json!(percent),
            )?;
            let mut attrs = Map::new();
            attrs.insert("current_position".into(), json!(percent.round() as u8));
            let state = if percent == 0.0 {
                Some("closed")
            } else {
                Some("open")
            };
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .extra_command_topics
                    .get("set_position_topic")
                    .cloned()
                    .ok_or_else(|| Error::Service("MQTT cover has no set_position_topic".into()))?,
                payload,
                state.map(str::to_owned),
                attrs,
                true,
            )?);
        }
        (
            "cover",
            "open_cover_tilt" | "close_cover_tilt" | "stop_cover_tilt" | "set_cover_tilt_position",
        ) => {
            let setting_position = matches!(
                cover_action.as_ref(),
                Some(CoverAction::SetTiltPosition { .. })
            ) || (cover_action.is_none()
                && service == "set_cover_tilt_position");
            if cover_action.is_none() {
                if setting_position {
                    check_service_options(call, &["tilt_position"])?;
                } else {
                    check_service_options(call, &[])?;
                }
            }
            let min = config_number(&entity.platform_config, "tilt_min", 0.0);
            let max = config_number(&entity.platform_config, "tilt_max", 100.0);
            let (ranged, percent) = match service {
                "open_cover_tilt" => {
                    let raw = config_number(&entity.platform_config, "tilt_open_position", 100.0);
                    (raw, (raw - min) / (max - min) * 100.0)
                }
                "close_cover_tilt" => {
                    let raw = config_number(&entity.platform_config, "tilt_closed_position", 0.0);
                    (raw, (raw - min) / (max - min) * 100.0)
                }
                "stop_cover_tilt" => (0.0, 0.0),
                _ => {
                    let percent = match cover_action.as_ref() {
                        Some(CoverAction::SetTiltPosition { position }) => {
                            f64::from(position.get())
                        }
                        _ => numeric_arg(call, "tilt_position", 0.0, 100.0)?,
                    };
                    (min + (max - min) * percent / 100.0, percent)
                }
            };
            let ranged = ranged.round();
            let payload = if service == "stop_cover_tilt" {
                config_string(&entity.platform_config, "payload_stop_tilt", "STOP").to_owned()
            } else {
                let template_tilt = if setting_position {
                    call.data
                        .get("tilt_position")
                        .cloned()
                        .unwrap_or_else(|| json!(percent))
                } else {
                    json!(ranged as i64)
                };
                render_typed_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "tilt_command_template",
                    &formatted_number(ranged),
                    "tilt_position",
                    template_tilt,
                )?
            };
            let mut attrs = Map::new();
            if service != "stop_cover_tilt" {
                attrs.insert("current_tilt_position".into(), json!(percent.round() as u8));
            }
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .extra_command_topics
                    .get("tilt_command_topic")
                    .cloned()
                    .ok_or_else(|| Error::Service("MQTT cover has no tilt_command_topic".into()))?,
                payload,
                None,
                attrs,
                true,
            )?);
        }
        ("fan", "turn_on" | "turn_off") => {
            let allowed = if service == "turn_on" {
                &["percentage", "preset_mode"][..]
            } else {
                &[][..]
            };
            check_service_options(call, allowed)?;
            let on = service == "turn_on";
            let payload = render_command(
                runtime,
                call,
                entity,
                templates,
                "command_template",
                if on { "ON" } else { "OFF" },
            )?;
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .command_topic
                    .clone()
                    .ok_or_else(|| Error::Service("MQTT fan has no command_topic".into()))?,
                payload,
                Some(if on { "on" } else { "off" }.into()),
                Map::new(),
                true,
            )?);
            if on {
                if let Some(value) = call.data.get("percentage") {
                    output.push(fan_percentage_command(
                        runtime, call, entity, templates, value, false,
                    )?);
                }
                if let Some(value) = call.data.get("preset_mode") {
                    output.push(fan_preset_command(runtime, call, entity, templates, value)?);
                }
            }
        }
        ("fan", "set_percentage") => {
            check_service_options(call, &["percentage"])?;
            let value = call
                .data
                .get("percentage")
                .ok_or_else(|| Error::Service("fan.set_percentage requires percentage".into()))?;
            output.push(fan_percentage_command(
                runtime, call, entity, templates, value, false,
            )?);
        }
        ("fan", "set_preset_mode") => {
            check_service_options(call, &["preset_mode"])?;
            let value = call
                .data
                .get("preset_mode")
                .ok_or_else(|| Error::Service("fan.set_preset_mode requires preset_mode".into()))?;
            output.push(fan_preset_command(runtime, call, entity, templates, value)?);
        }
        ("fan", "oscillate") => {
            check_service_options(call, &["oscillating"])?;
            let on = call
                .data
                .get("oscillating")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    Error::Service("fan.oscillate requires boolean oscillating".into())
                })?;
            let payload_key = if on {
                "payload_oscillation_on"
            } else {
                "payload_oscillation_off"
            };
            let fallback = if on { "oscillate_on" } else { "oscillate_off" };
            let payload = render_command(
                runtime,
                call,
                entity,
                templates,
                "oscillation_command_template",
                config_string(&entity.platform_config, payload_key, fallback),
            )?;
            let mut attrs = Map::new();
            attrs.insert("oscillating".into(), Value::Bool(on));
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .extra_command_topics
                    .get("oscillation_command_topic")
                    .cloned()
                    .ok_or_else(|| {
                        Error::Service("MQTT fan has no oscillation_command_topic".into())
                    })?,
                payload,
                None,
                attrs,
                true,
            )?);
        }
        ("fan", "set_direction") => {
            check_service_options(call, &["direction"])?;
            let direction = call
                .data
                .get("direction")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Service("fan.set_direction requires direction".into()))?;
            if !matches!(direction, "forward" | "reverse") {
                return Err(Error::Service(
                    "fan direction must be forward or reverse".into(),
                ));
            }
            let payload = render_command(
                runtime,
                call,
                entity,
                templates,
                "direction_command_template",
                direction,
            )?;
            let mut attrs = Map::new();
            attrs.insert("direction".into(), Value::String(direction.into()));
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .extra_command_topics
                    .get("direction_command_topic")
                    .cloned()
                    .ok_or_else(|| {
                        Error::Service("MQTT fan has no direction_command_topic".into())
                    })?,
                payload,
                None,
                attrs,
                true,
            )?);
        }
        ("button", "press") => {
            let payload = render_command(
                runtime,
                call,
                entity,
                templates,
                "command_template",
                config_string(&entity.platform_config, "payload_press", "PRESS"),
            )?;
            output.push(make_command(
                runtime,
                entity,
                call,
                entity
                    .command_topic
                    .clone()
                    .ok_or_else(|| Error::Service("MQTT button has no command_topic".into()))?,
                payload,
                None,
                Map::new(),
                false,
            )?);
        }
        ("number", "set_value") => {
            check_service_options(call, &["value"])?;
            let number = numeric_arg(
                call,
                "value",
                config_number(&entity.platform_config, "min", 0.0),
                config_number(&entity.platform_config, "max", 100.0),
            )?;
            let step = config_number(&entity.platform_config, "step", 1.0);
            let min = config_number(&entity.platform_config, "min", 0.0);
            if ((number - min) / step - ((number - min) / step).round()).abs() > 0.000_001 {
                return Err(Error::Service(
                    "number value is not aligned with configured step".into(),
                ));
            }
            let raw = formatted_number(number);
            let payload =
                render_command(runtime, call, entity, templates, "command_template", &raw)?;
            let mut attrs = Map::new();
            attrs.insert("value".into(), json!(number));
            output.push(make_command(
                runtime,
                entity,
                call,
                entity.command_topic.clone().unwrap(),
                payload,
                Some(raw),
                attrs,
                true,
            )?);
        }
        ("select", "select_option") => {
            check_service_options(call, &["option"])?;
            let option = call
                .data
                .get("option")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Service("select.select_option requires option".into()))?;
            if !config_strings(&entity.platform_config, "options")
                .iter()
                .any(|known| known == option)
            {
                return Err(Error::Service(
                    "select option is not in the configured options list".into(),
                ));
            }
            let payload =
                render_command(runtime, call, entity, templates, "command_template", option)?;
            output.push(make_command(
                runtime,
                entity,
                call,
                entity.command_topic.clone().unwrap(),
                payload,
                Some(option.into()),
                Map::new(),
                true,
            )?);
        }
        ("text", "set_value") => {
            check_service_options(call, &["value"])?;
            let text = call
                .data
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Service("text.set_value requires string value".into()))?;
            validate_text_value(entity, text)?;
            let payload =
                render_command(runtime, call, entity, templates, "command_template", text)?;
            output.push(make_command(
                runtime,
                entity,
                call,
                entity.command_topic.clone().unwrap(),
                payload,
                Some(text.into()),
                Map::new(),
                true,
            )?);
        }
        _ => {
            return Err(Error::Service(format!(
                "unsupported MQTT service {}.{}",
                call.domain, call.service
            )));
        }
    }
    Ok(output)
}

fn requested_on_off(current: &str, service: &str) -> bool {
    match service {
        "turn_on" => true,
        "turn_off" => false,
        _ => current != "on",
    }
}

fn check_service_options(call: &ServiceCall, allowed: &[&str]) -> Result<()> {
    for key in call
        .data
        .keys()
        .chain(call.target.keys())
        .filter(|key| key.as_str() != "entity_id")
    {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::Service(format!(
                "unsupported {}.{} option {key:?}",
                call.domain, call.service
            )));
        }
    }
    Ok(())
}

fn numeric_arg(call: &ServiceCall, key: &str, min: f64, max: f64) -> Result<f64> {
    let value = call.data.get(key).ok_or_else(|| {
        Error::Service(format!("{}.{} requires {key}", call.domain, call.service))
    })?;
    let number = number_value(value, key).map_err(|error| Error::Service(error.to_string()))?;
    if !(min..=max).contains(&number) {
        return Err(Error::Service(format!(
            "{key} must be between {min} and {max}"
        )));
    }
    Ok(number)
}

fn render_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    template_key: &str,
    value: &str,
) -> Result<String> {
    let template = entity
        .platform_config
        .get(template_key)
        .and_then(Value::as_str)
        .or_else(|| {
            (template_key == "command_template")
                .then_some(entity.command_template.as_deref())
                .flatten()
        });
    let Some(template) = template else {
        return Ok(value.to_owned());
    };
    let variables = command_variables(runtime, call, value, entity);
    templates
        .render(template, &variables)
        .map_err(|error| Error::Service(format!("MQTT command template failed: {error}")))
}

#[allow(clippy::too_many_arguments)]
fn render_typed_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    template_key: &str,
    value: &str,
    variable_key: &str,
    variable_value: Value,
) -> Result<String> {
    let Some(template) = entity
        .platform_config
        .get(template_key)
        .and_then(Value::as_str)
    else {
        return Ok(value.to_owned());
    };
    let mut variables = command_variables(runtime, call, value, entity);
    variables.insert(variable_key.into(), variable_value);
    for key in ["position_open", "position_closed", "tilt_min", "tilt_max"] {
        if let Some(value) = entity.platform_config.get(key) {
            variables.insert(key.into(), value.clone());
        }
    }
    templates
        .render(template, &variables)
        .map_err(|error| Error::Service(format!("MQTT command template failed: {error}")))
}

#[allow(clippy::too_many_arguments)]
fn make_command(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    call: &ServiceCall,
    topic: String,
    payload: String,
    optimistic_state: Option<String>,
    optimistic_updates: Map<String, Value>,
    allow_optimistic: bool,
) -> Result<Command> {
    if payload.len() > entity.max_payload_bytes {
        return Err(Error::Service(format!(
            "MQTT command payload exceeds configured limit of {} bytes",
            entity.max_payload_bytes
        )));
    }
    let optimistic = (allow_optimistic && entity.optimistic).then(|| {
        let entity_id = mqtt_entity_id(entity);
        let attributes = runtime
            .get_state(&entity_id)
            .map(|state| state.attributes)
            .unwrap_or_else(|| entity.attributes.clone());
        OptimisticState {
            discovery_topic: entity.metadata.discovery_topic.clone(),
            state: optimistic_state,
            attributes,
            optimistic_updates,
            context: call.context.clone(),
            switch_state: entity.metadata.platform == "switch",
            climate_state: None,
            entity_handle: entity_lifecycle_handle(entity).cloned(),
        }
    });
    Ok(Command {
        topic,
        payload: payload.into_bytes(),
        publish: true,
        qos: qos(entity.qos)?,
        retain: entity.retain,
        optimistic,
        entity_handle: entity_lifecycle_handle(entity).cloned(),
    })
}

fn fan_effect_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    effects: Vec<FanEffect>,
) -> Result<Vec<Command>> {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            FanEffect::TurnOn {
                percentage,
                preset_mode,
            } => {
                commands.push(fan_power_command(runtime, call, entity, templates, true)?);
                if let Some(percentage) = percentage.filter(|value| *value != 0) {
                    commands.push(fan_percentage_command(
                        runtime,
                        call,
                        entity,
                        templates,
                        &json!(percentage),
                        true,
                    )?);
                }
                if let Some(preset_mode) = preset_mode.filter(|value| !value.is_empty()) {
                    commands.push(fan_preset_command(
                        runtime,
                        call,
                        entity,
                        templates,
                        &Value::String(preset_mode),
                    )?);
                }
            }
            FanEffect::TurnOff => {
                commands.push(fan_power_command(runtime, call, entity, templates, false)?);
            }
            FanEffect::SetPercentage { percentage } => {
                commands.push(fan_percentage_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    &json!(percentage),
                    true,
                )?);
            }
            FanEffect::SetPresetMode { preset_mode } => {
                commands.push(fan_preset_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    &Value::String(preset_mode),
                )?);
            }
            FanEffect::Oscillate { oscillating } => commands.push(fan_oscillation_command(
                runtime,
                call,
                entity,
                templates,
                oscillating,
            )?),
            FanEffect::SetDirection { direction } => {
                let direction = direction.ok_or_else(|| {
                    Error::Service("MQTT fan set_direction requires a direction argument".into())
                })?;
                commands.push(fan_direction_command(
                    runtime, call, entity, templates, &direction,
                )?);
            }
        }
    }
    Ok(commands)
}

fn climate_effect_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    effects: Vec<ClimateEffect>,
    context: Context,
) -> Result<Vec<Command>> {
    let climate = entity
        .climate
        .as_ref()
        .ok_or_else(|| Error::Service("MQTT climate entity has no descriptor".into()))?;
    let descriptor = match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Climate { descriptor, .. }) => descriptor.clone(),
        _ => climate.descriptor.clone(),
    };
    let mut state = climate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            ClimateEffect::TurnOn => commands.push(climate_turn_on(
                runtime,
                call,
                entity,
                templates,
                &descriptor,
                &mut state,
                context.clone(),
            )?),
            ClimateEffect::TurnOff => commands.push(climate_turn_off(
                runtime,
                call,
                entity,
                templates,
                &descriptor,
                &mut state,
                context.clone(),
            )?),
            ClimateEffect::Toggle => {
                if state.snapshot.hvac_mode == Some(HvacMode::Off) {
                    commands.push(climate_turn_on(
                        runtime,
                        call,
                        entity,
                        templates,
                        &descriptor,
                        &mut state,
                        context.clone(),
                    )?);
                } else {
                    commands.push(climate_turn_off(
                        runtime,
                        call,
                        entity,
                        templates,
                        &descriptor,
                        &mut state,
                        context.clone(),
                    )?);
                }
            }
            ClimateEffect::SetHvacMode { hvac_mode } => commands.push(climate_mode_command(
                runtime,
                call,
                entity,
                templates,
                &mut state,
                hvac_mode,
                context.clone(),
            )?),
            ClimateEffect::SetTemperature {
                temperature,
                target_temp_high,
                target_temp_low,
                hvac_mode,
            } => commands.extend(climate_temperature_commands(
                runtime,
                call,
                entity,
                templates,
                &mut state,
                temperature,
                target_temp_low,
                target_temp_high,
                hvac_mode,
                context.clone(),
            )?),
            ClimateEffect::SetHumidity { humidity } => {
                let update = climate_should_update(entity, "target_humidity_state_topic");
                if update {
                    state.snapshot.target_humidity = Some(humidity as f64);
                }
                commands.push(climate_topic_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "target_humidity_command_topic",
                    "target_humidity_command_template",
                    &humidity.to_string(),
                    climate_mutations(
                        entity,
                        "target_humidity_state_topic",
                        ClimateStateMutation::TargetHumidity(humidity as f64),
                    ),
                    context.clone(),
                )?);
            }
            ClimateEffect::SetFanMode { fan_mode } => {
                let update = climate_should_update(entity, "fan_mode_state_topic");
                if update {
                    state.snapshot.fan_mode = Some(fan_mode.clone());
                }
                commands.push(climate_topic_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "fan_mode_command_topic",
                    "fan_mode_command_template",
                    &fan_mode,
                    climate_mutations(
                        entity,
                        "fan_mode_state_topic",
                        ClimateStateMutation::FanMode(fan_mode.clone()),
                    ),
                    context.clone(),
                )?);
            }
            ClimateEffect::SetPresetMode { preset_mode } => {
                let update = entity.optimistic
                    || !climate_topic_configured(entity, "preset_mode_state_topic");
                if update {
                    state.snapshot.preset_mode = Some(preset_mode.clone());
                }
                commands.push(climate_topic_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "preset_mode_command_topic",
                    "preset_mode_command_template",
                    &preset_mode,
                    climate_mutations(
                        entity,
                        "preset_mode_state_topic",
                        ClimateStateMutation::PresetMode(preset_mode.clone()),
                    ),
                    context.clone(),
                )?);
            }
            ClimateEffect::SetSwingMode { swing_mode } => {
                let update = climate_should_update(entity, "swing_mode_state_topic");
                if update {
                    state.snapshot.swing_mode = Some(swing_mode.clone());
                }
                commands.push(climate_topic_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "swing_mode_command_topic",
                    "swing_mode_command_template",
                    &swing_mode,
                    climate_mutations(
                        entity,
                        "swing_mode_state_topic",
                        ClimateStateMutation::SwingMode(swing_mode.clone()),
                    ),
                    context.clone(),
                )?);
            }
            ClimateEffect::SetSwingHorizontalMode {
                swing_horizontal_mode,
            } => {
                let update = climate_should_update(entity, "swing_horizontal_mode_state_topic");
                if update {
                    state.snapshot.swing_horizontal_mode = Some(swing_horizontal_mode.clone());
                }
                commands.push(climate_topic_command(
                    runtime,
                    call,
                    entity,
                    templates,
                    "swing_horizontal_mode_command_topic",
                    "swing_horizontal_mode_command_template",
                    &swing_horizontal_mode,
                    climate_mutations(
                        entity,
                        "swing_horizontal_mode_state_topic",
                        ClimateStateMutation::SwingHorizontalMode(swing_horizontal_mode.clone()),
                    ),
                    context.clone(),
                )?);
            }
        }
    }
    Ok(commands)
}
fn climate_turn_on_mode(descriptor: &ClimateDescriptor) -> Result<HvacMode> {
    let modes = descriptor.hvac_modes().unwrap_or_default();
    if modes.len() == 2 && modes.contains(&HvacMode::Off) {
        return modes
            .iter()
            .copied()
            .find(|mode| *mode != HvacMode::Off)
            .ok_or_else(|| Error::Service("MQTT climate has no non-off HVAC mode".into()));
    }
    [HvacMode::HeatCool, HvacMode::Heat, HvacMode::Cool]
        .into_iter()
        .find(|mode| modes.contains(mode))
        .ok_or_else(|| Error::Service("MQTT climate has no mode to turn on".into()))
}
fn climate_turn_on(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    descriptor: &ClimateDescriptor,
    state: &mut MqttClimateState,
    context: Context,
) -> Result<Command> {
    if climate_topic_configured(entity, "power_command_topic") {
        return climate_topic_command(
            runtime,
            call,
            entity,
            templates,
            "power_command_topic",
            "power_command_template",
            config_string(&entity.platform_config, "payload_on", "ON"),
            Vec::new(),
            context,
        );
    }
    climate_mode_command(
        runtime,
        call,
        entity,
        templates,
        state,
        climate_turn_on_mode(descriptor)?,
        context,
    )
}
fn climate_turn_off(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    descriptor: &ClimateDescriptor,
    state: &mut MqttClimateState,
    context: Context,
) -> Result<Command> {
    if climate_topic_configured(entity, "power_command_topic") {
        let update = entity.optimistic;
        if update {
            state.snapshot.hvac_mode = Some(HvacMode::Off);
        }
        return climate_topic_command(
            runtime,
            call,
            entity,
            templates,
            "power_command_topic",
            "power_command_template",
            config_string(&entity.platform_config, "payload_off", "OFF"),
            if update {
                vec![ClimateStateMutation::HvacModeWithoutSetpointSwap(
                    HvacMode::Off,
                )]
            } else {
                Vec::new()
            },
            context,
        );
    }
    if !descriptor
        .hvac_modes()
        .unwrap_or_default()
        .contains(&HvacMode::Off)
    {
        return Err(Error::Service(
            "MQTT climate cannot turn off because 'off' is not an advertised mode".into(),
        ));
    }
    climate_mode_command(
        runtime,
        call,
        entity,
        templates,
        state,
        HvacMode::Off,
        context,
    )
}
#[allow(clippy::too_many_arguments)]
fn climate_temperature_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    state: &mut MqttClimateState,
    temperature: Option<f64>,
    target_temp_low: Option<f64>,
    target_temp_high: Option<f64>,
    hvac_mode: Option<HvacMode>,
    context: Context,
) -> Result<Vec<Command>> {
    let mut commands = Vec::new();
    if let Some(hvac_mode) = hvac_mode {
        commands.push(climate_mode_command(
            runtime,
            call,
            entity,
            templates,
            state,
            hvac_mode,
            context.clone(),
        )?);
    }
    if let Some(value) = temperature {
        if climate_should_update(entity, "temperature_state_topic") {
            state.snapshot.target_temperature = Some(value);
            climate_park_range(state);
        }
        commands.push(climate_topic_command(
            runtime,
            call,
            entity,
            templates,
            "temperature_command_topic",
            "temperature_command_template",
            &format!("{value:?}"),
            climate_mutations(
                entity,
                "temperature_state_topic",
                ClimateStateMutation::TargetTemperature(value),
            ),
            context.clone(),
        )?);
    }
    if let Some(value) = target_temp_low {
        if climate_should_update(entity, "temperature_low_state_topic") {
            state.snapshot.target_temperature_low = Some(value);
            climate_park_single(state);
        }
        commands.push(climate_topic_command(
            runtime,
            call,
            entity,
            templates,
            "temperature_low_command_topic",
            "temperature_low_command_template",
            &format!("{value:?}"),
            climate_mutations(
                entity,
                "temperature_low_state_topic",
                ClimateStateMutation::TargetTemperatureLow(value),
            ),
            context.clone(),
        )?);
    }
    if let Some(value) = target_temp_high {
        if climate_should_update(entity, "temperature_high_state_topic") {
            state.snapshot.target_temperature_high = Some(value);
            climate_park_single(state);
        }
        commands.push(climate_topic_command(
            runtime,
            call,
            entity,
            templates,
            "temperature_high_command_topic",
            "temperature_high_command_template",
            &format!("{value:?}"),
            climate_mutations(
                entity,
                "temperature_high_state_topic",
                ClimateStateMutation::TargetTemperatureHigh(value),
            ),
            context.clone(),
        )?);
    }
    Ok(commands)
}
fn climate_should_update(entity: &DiscoveredEntity, state_topic: &str) -> bool {
    entity.optimistic || !climate_topic_configured(entity, state_topic)
}

fn climate_mutations(
    entity: &DiscoveredEntity,
    state_topic: &str,
    mutation: ClimateStateMutation,
) -> Vec<ClimateStateMutation> {
    // The operation is intentional even when its value equals the preflight
    // cache. A prior command can await an ACK while telemetry changes it.
    climate_should_update(entity, state_topic)
        .then_some(mutation)
        .into_iter()
        .collect()
}

fn climate_mode_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    state: &mut MqttClimateState,
    mode: HvacMode,
    context: Context,
) -> Result<Command> {
    let update = climate_should_update(entity, "mode_state_topic");
    if update {
        state.snapshot.hvac_mode = Some(mode);
        climate_swap_setpoints(entity, state, mode);
    }
    climate_topic_command(
        runtime,
        call,
        entity,
        templates,
        "mode_command_topic",
        "mode_command_template",
        mode.as_str(),
        climate_mutations(
            entity,
            "mode_state_topic",
            ClimateStateMutation::HvacMode(mode),
        ),
        context,
    )
}
fn climate_swap_setpoints(entity: &DiscoveredEntity, state: &mut MqttClimateState, mode: HvacMode) {
    if !state.single_and_range_setpoints
        || !matches!(mode, HvacMode::Cool | HvacMode::Heat | HvacMode::HeatCool)
    {
        return;
    }
    let single_is_optimistic = climate_should_update(entity, "temperature_state_topic");
    let range_is_optimistic = entity.optimistic
        || (!climate_topic_configured(entity, "temperature_low_state_topic")
            && !climate_topic_configured(entity, "temperature_high_state_topic"));
    if mode == HvacMode::HeatCool {
        if single_is_optimistic {
            climate_park_single(state);
        }
        if range_is_optimistic
            && state.snapshot.target_temperature_low.is_none()
            && state.snapshot.target_temperature_high.is_none()
        {
            (
                state.snapshot.target_temperature_low,
                state.snapshot.target_temperature_high,
            ) = state.parked_target_temperature_range;
        }
    } else {
        if single_is_optimistic && state.snapshot.target_temperature.is_none() {
            state.snapshot.target_temperature = state.parked_target_temperature;
        }
        if range_is_optimistic {
            climate_park_range(state);
        }
    }
}
fn climate_park_single(state: &mut MqttClimateState) {
    if !state.single_and_range_setpoints {
        return;
    }
    if let Some(temperature) = state.snapshot.target_temperature {
        state.parked_target_temperature = Some(temperature);
        state.snapshot.target_temperature = None;
    }
}
fn climate_park_range(state: &mut MqttClimateState) {
    if !state.single_and_range_setpoints
        || (state.snapshot.target_temperature_low.is_none()
            && state.snapshot.target_temperature_high.is_none())
    {
        return;
    }
    state.parked_target_temperature_range = (
        state.snapshot.target_temperature_low,
        state.snapshot.target_temperature_high,
    );
    state.snapshot.target_temperature_low = None;
    state.snapshot.target_temperature_high = None;
}
#[allow(clippy::too_many_arguments)]
fn climate_topic_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    topic_key: &str,
    template_key: &str,
    value: &str,
    mutations: Vec<ClimateStateMutation>,
    context: Context,
) -> Result<Command> {
    let topic = entity
        .platform_config
        .get(topic_key)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let payload = if topic.is_some() {
        render_command(runtime, call, entity, templates, template_key, value)?
    } else {
        String::new()
    };
    let publish = topic.is_some();
    let mut command = make_command(
        runtime,
        entity,
        call,
        topic.unwrap_or_default(),
        payload,
        None,
        Map::new(),
        false,
    )?;
    command.publish = publish;
    if !mutations.is_empty() {
        attach_climate_state(runtime, entity, &mut command, mutations, context);
    }
    Ok(command)
}
fn attach_climate_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    command: &mut Command,
    mutations: Vec<ClimateStateMutation>,
    context: Context,
) {
    let entity_id = mqtt_entity_id(entity);
    let attributes = runtime
        .get_state(&entity_id)
        .map(|state| state.attributes)
        .unwrap_or_else(|| entity.attributes.clone());
    command.optimistic = Some(OptimisticState {
        discovery_topic: entity.metadata.discovery_topic.clone(),
        state: None,
        attributes,
        optimistic_updates: Map::new(),
        context,
        switch_state: false,
        climate_state: Some(ClimateStateUpdate { mutations }),
        entity_handle: entity_lifecycle_handle(entity).cloned(),
    });
}

fn apply_climate_state_update(
    entity: &DiscoveredEntity,
    current: &mut MqttClimateState,
    update: &ClimateStateUpdate,
    context: &Context,
) {
    for mutation in &update.mutations {
        match mutation {
            ClimateStateMutation::HvacMode(mode) => {
                current.snapshot.hvac_mode = Some(*mode);
                climate_swap_setpoints(entity, current, *mode);
            }
            ClimateStateMutation::HvacModeWithoutSetpointSwap(mode) => {
                current.snapshot.hvac_mode = Some(*mode);
            }
            ClimateStateMutation::TargetTemperature(value) => {
                current.snapshot.target_temperature = Some(*value);
                climate_park_range(current);
            }
            ClimateStateMutation::TargetTemperatureLow(value) => {
                current.snapshot.target_temperature_low = Some(*value);
                climate_park_single(current);
            }
            ClimateStateMutation::TargetTemperatureHigh(value) => {
                current.snapshot.target_temperature_high = Some(*value);
                climate_park_single(current);
            }
            ClimateStateMutation::TargetHumidity(value) => {
                current.snapshot.target_humidity = Some(*value);
            }
            ClimateStateMutation::FanMode(value) => {
                current.snapshot.fan_mode = Some(value.clone());
            }
            ClimateStateMutation::PresetMode(value) => {
                current.snapshot.preset_mode = Some(value.clone());
            }
            ClimateStateMutation::SwingMode(value) => {
                current.snapshot.swing_mode = Some(value.clone());
            }
            ClimateStateMutation::SwingHorizontalMode(value) => {
                current.snapshot.swing_horizontal_mode = Some(value.clone());
            }
        }
    }
    current.snapshot.context = context.clone();
}

fn fan_power_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    on: bool,
) -> Result<Command> {
    let payload = render_command(
        runtime,
        call,
        entity,
        templates,
        "command_template",
        if on { "ON" } else { "OFF" },
    )?;
    make_command(
        runtime,
        entity,
        call,
        entity
            .command_topic
            .clone()
            .ok_or_else(|| Error::Service("MQTT fan has no command_topic".into()))?,
        payload,
        Some(if on { "on" } else { "off" }.into()),
        Map::new(),
        true,
    )
}

fn fan_oscillation_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    on: bool,
) -> Result<Command> {
    let payload_key = if on {
        "payload_oscillation_on"
    } else {
        "payload_oscillation_off"
    };
    let fallback = if on { "oscillate_on" } else { "oscillate_off" };
    let payload = render_command(
        runtime,
        call,
        entity,
        templates,
        "oscillation_command_template",
        config_string(&entity.platform_config, payload_key, fallback),
    )?;
    let mut attributes = Map::new();
    attributes.insert("oscillating".into(), Value::Bool(on));
    make_command(
        runtime,
        entity,
        call,
        entity
            .extra_command_topics
            .get("oscillation_command_topic")
            .cloned()
            .ok_or_else(|| Error::Service("MQTT fan has no oscillation_command_topic".into()))?,
        payload,
        None,
        attributes,
        true,
    )
}

fn fan_direction_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    direction: &str,
) -> Result<Command> {
    let payload = render_command(
        runtime,
        call,
        entity,
        templates,
        "direction_command_template",
        direction,
    )?;
    let mut attributes = Map::new();
    attributes.insert("direction".into(), Value::String(direction.into()));
    make_command(
        runtime,
        entity,
        call,
        entity
            .extra_command_topics
            .get("direction_command_topic")
            .cloned()
            .ok_or_else(|| Error::Service("MQTT fan has no direction_command_topic".into()))?,
        payload,
        None,
        attributes,
        true,
    )
}

fn fan_percentage_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    value: &Value,
    allow_zero: bool,
) -> Result<Command> {
    let percent =
        number_value(value, "fan percentage").map_err(|error| Error::Service(error.to_string()))?;
    if !(if allow_zero { 0.0..=100.0 } else { 1.0..=100.0 }).contains(&percent) {
        return Err(Error::Service(
            "fan percentage must be between 1 and 100".into(),
        ));
    }
    let min = config_number(&entity.platform_config, "speed_range_min", 1.0);
    let max = config_number(&entity.platform_config, "speed_range_max", 100.0);
    let ranged = hass_fan::percentage_to_ranged_value((min, max), percent)
        .map_err(|error| Error::Service(error.to_string()))?
        .ceil();
    let payload = render_typed_command(
        runtime,
        call,
        entity,
        templates,
        "percentage_command_template",
        &formatted_number(ranged),
        "percentage",
        json!(percent),
    )?;
    let mut attrs = Map::new();
    attrs.insert("percentage".into(), json!(percent.round() as u8));
    make_command(
        runtime,
        entity,
        call,
        entity
            .extra_command_topics
            .get("percentage_command_topic")
            .cloned()
            .ok_or_else(|| Error::Service("MQTT fan has no percentage_command_topic".into()))?,
        payload,
        None,
        attrs,
        true,
    )
}

fn fan_preset_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    value: &Value,
) -> Result<Command> {
    let preset = value
        .as_str()
        .ok_or_else(|| Error::Service("fan preset_mode must be a string".into()))?;
    if !config_strings(&entity.platform_config, "preset_modes")
        .iter()
        .any(|known| known == preset)
    {
        return Err(Error::Service(
            "fan preset_mode is not in the configured preset_modes list".into(),
        ));
    }
    let payload = render_command(
        runtime,
        call,
        entity,
        templates,
        "preset_mode_command_template",
        preset,
    )?;
    let mut attrs = Map::new();
    attrs.insert("preset_mode".into(), Value::String(preset.into()));
    make_command(
        runtime,
        entity,
        call,
        entity
            .extra_command_topics
            .get("preset_mode_command_topic")
            .cloned()
            .ok_or_else(|| Error::Service("MQTT fan has no preset_mode_command_topic".into()))?,
        payload,
        None,
        attrs,
        true,
    )
}

fn validate_text_value(entity: &DiscoveredEntity, value: &str) -> Result<()> {
    let min = config_number(&entity.platform_config, "min", 0.0) as usize;
    let max = config_number(&entity.platform_config, "max", 255.0) as usize;
    let len = value.chars().count();
    if len < min
        || len > max
        || entity.text_pattern.as_ref().is_some_and(|pattern| {
            !pattern
                .find(value)
                .is_some_and(|matched| matched.start() == 0)
        })
    {
        return Err(Error::Service(
            "text value violates its configured length or pattern".into(),
        ));
    }
    Ok(())
}

fn light_supports_brightness(entity: &DiscoveredEntity) -> bool {
    config_bool(&entity.platform_config, "brightness", false)
        || entity
            .platform_config
            .get("supported_color_modes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes.iter().filter_map(Value::as_str).any(|mode| {
                    matches!(
                        mode,
                        "brightness"
                            | "color_temp"
                            | "hs"
                            | "rgb"
                            | "rgbw"
                            | "rgbww"
                            | "white"
                            | "xy"
                    )
                })
            })
}

fn light_option_array(call: &ServiceCall, key: &str, size: usize) -> Result<Vec<f64>> {
    let values = call
        .data
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Service(format!("light {key} must be a numeric array")))?;
    if values.len() != size {
        return Err(Error::Service(format!(
            "light {key} must have {size} values"
        )));
    }
    values
        .iter()
        .map(|value| number_value(value, key).map_err(|error| Error::Service(error.to_string())))
        .collect()
}

fn json_light_command(
    entity: &DiscoveredEntity,
    call: &ServiceCall,
    on: bool,
) -> Result<(String, Map<String, Value>)> {
    const ALLOWED: &[&str] = &[
        "brightness",
        "color_temp_kelvin",
        "hs_color",
        "rgb_color",
        "rgbw_color",
        "rgbww_color",
        "xy_color",
        "white",
        "effect",
        "flash",
        "transition",
    ];
    check_service_options(call, ALLOWED)?;
    let brightness_supported = light_supports_brightness(entity);
    if call.data.contains_key("brightness") && !brightness_supported {
        return Err(Error::Service(
            "MQTT JSON light does not support brightness".into(),
        ));
    }
    if call.data.contains_key("color_temp_kelvin") && !supported_light_mode(entity, "color_temp") {
        return Err(Error::Service(
            "MQTT JSON light does not support color temperature".into(),
        ));
    }
    for (field, mode) in [
        ("hs_color", "hs"),
        ("rgb_color", "rgb"),
        ("rgbw_color", "rgbw"),
        ("rgbww_color", "rgbww"),
        ("xy_color", "xy"),
        ("white", "white"),
    ] {
        if call.data.contains_key(field) && !supported_light_mode(entity, mode) {
            return Err(Error::Service(format!(
                "MQTT JSON light does not support {mode} color"
            )));
        }
    }
    if call.data.contains_key("effect") && !config_bool(&entity.platform_config, "effect", false) {
        return Err(Error::Service(
            "MQTT JSON light does not support effects".into(),
        ));
    }
    if call.data.contains_key("flash") && !config_bool(&entity.platform_config, "flash", true) {
        return Err(Error::Service(
            "MQTT JSON light does not support flash".into(),
        ));
    }
    if call.data.contains_key("transition")
        && !config_bool(&entity.platform_config, "transition", true)
    {
        return Err(Error::Service(
            "MQTT JSON light does not support transition".into(),
        ));
    }
    if !on
        && call
            .data
            .keys()
            .any(|key| !matches!(key.as_str(), "entity_id" | "flash" | "transition"))
    {
        return Err(Error::Service(
            "MQTT JSON light turn_off supports only flash and transition options".into(),
        ));
    }

    let mut message = Map::new();
    message.insert(
        "state".into(),
        Value::String(if on { "ON" } else { "OFF" }.into()),
    );
    let mut attrs = Map::new();
    if let Some(value) = call.data.get("brightness") {
        let brightness =
            number_value(value, "brightness").map_err(|error| Error::Service(error.to_string()))?;
        if !(1.0..=255.0).contains(&brightness) {
            return Err(Error::Service(
                "light brightness must be between 1 and 255".into(),
            ));
        }
        let scale = config_number(&entity.platform_config, "brightness_scale", 255.0);
        let scaled = (brightness / 255.0 * scale).round().max(1.0) as u32;
        message.insert("brightness".into(), json!(scaled));
        attrs.insert("brightness".into(), json!(brightness.round() as u8));
    }
    if let Some(value) = call.data.get("color_temp_kelvin") {
        let kelvin = number_value(value, "color_temp_kelvin")
            .map_err(|error| Error::Service(error.to_string()))?;
        let min = config_number(&entity.platform_config, "min_kelvin", 1500.0);
        let max = config_number(&entity.platform_config, "max_kelvin", 9000.0);
        if !(min..=max).contains(&kelvin) {
            return Err(Error::Service(
                "light color temperature is outside configured Kelvin bounds".into(),
            ));
        }
        let raw = if config_bool(&entity.platform_config, "color_temp_kelvin", false) {
            kelvin
        } else {
            1_000_000.0 / kelvin
        };
        message.insert("color_temp".into(), json!(raw.round() as u32));
        attrs.insert("color_temp_kelvin".into(), json!(kelvin.round() as u32));
        attrs.insert("color_mode".into(), Value::String("color_temp".into()));
    }
    if let Some(value) = call.data.get("hs_color") {
        let values = light_option_array(call, "hs_color", 2)?;
        if !(0.0..=360.0).contains(&values[0]) || !(0.0..=100.0).contains(&values[1]) {
            return Err(Error::Service("light hs_color is out of range".into()));
        }
        message.insert("color".into(), json!({"h": values[0], "s": values[1]}));
        attrs.insert("hs_color".into(), json!(value));
        attrs.insert("color_mode".into(), Value::String("hs".into()));
    }
    if let Some(value) = call.data.get("rgb_color") {
        let values = light_option_array(call, "rgb_color", 3)?;
        if values
            .iter()
            .any(|value| !(0.0..=255.0).contains(value) || value.fract() != 0.0)
        {
            return Err(Error::Service(
                "light rgb_color values must be integers from 0 to 255".into(),
            ));
        }
        let scale = if config_bool(&entity.platform_config, "brightness", false) {
            255.0
        } else {
            call.data
                .get("brightness")
                .map(|v| number_value(v, "brightness"))
                .transpose()
                .map_err(|e| Error::Service(e.to_string()))?
                .unwrap_or(255.0)
        };
        let scaled: Vec<u8> = values
            .iter()
            .map(|value| (value / 255.0 * scale).round().clamp(0.0, 255.0) as u8)
            .collect();
        message.insert(
            "color".into(),
            json!({"r": scaled[0], "g": scaled[1], "b": scaled[2]}),
        );
        attrs.insert("rgb_color".into(), json!(value));
        attrs.insert("color_mode".into(), Value::String("rgb".into()));
    }
    for (field, mode, keys, attribute) in [
        (
            "rgbw_color",
            "rgbw",
            &["r", "g", "b", "w"][..],
            "rgbw_color",
        ),
        (
            "rgbww_color",
            "rgbww",
            &["r", "g", "b", "c", "w"][..],
            "rgbww_color",
        ),
    ] {
        if let Some(value) = call.data.get(field) {
            let values = light_option_array(call, field, keys.len())?;
            if values
                .iter()
                .any(|value| !(0.0..=255.0).contains(value) || value.fract() != 0.0)
            {
                return Err(Error::Service(format!(
                    "light {field} values must be integers from 0 to 255"
                )));
            }
            let scale = if config_bool(&entity.platform_config, "brightness", false) {
                255.0
            } else {
                call.data
                    .get("brightness")
                    .map(|value| number_value(value, "brightness"))
                    .transpose()
                    .map_err(|error| Error::Service(error.to_string()))?
                    .unwrap_or(255.0)
            };
            let mut color = Map::new();
            for (key, value) in keys.iter().zip(values) {
                color.insert(
                    (*key).into(),
                    json!((value / 255.0 * scale).round().clamp(0.0, 255.0) as u8),
                );
            }
            message.insert("color".into(), Value::Object(color));
            attrs.insert(attribute.into(), value.clone());
            attrs.insert("color_mode".into(), Value::String(mode.into()));
        }
    }
    if let Some(value) = call.data.get("xy_color") {
        let values = light_option_array(call, "xy_color", 2)?;
        if values.iter().any(|value| !(0.0..=1.0).contains(value)) {
            return Err(Error::Service(
                "light xy_color values must be from 0 to 1".into(),
            ));
        }
        message.insert("color".into(), json!({"x": values[0], "y": values[1]}));
        attrs.insert("xy_color".into(), json!(value));
        attrs.insert("color_mode".into(), Value::String("xy".into()));
    }
    if let Some(value) = call.data.get("white") {
        let white =
            number_value(value, "white").map_err(|error| Error::Service(error.to_string()))?;
        if !(0.0..=255.0).contains(&white) {
            return Err(Error::Service(
                "light white value must be between 0 and 255".into(),
            ));
        }
        let scale = config_number(&entity.platform_config, "white_scale", 255.0);
        message.insert(
            "white".into(),
            json!((white / 255.0 * scale).round().clamp(0.0, scale) as u32),
        );
        attrs.insert("brightness".into(), json!(white.round() as u8));
        attrs.insert("color_mode".into(), Value::String("white".into()));
    }
    if let Some(value) = call.data.get("effect") {
        let effect = value
            .as_str()
            .ok_or_else(|| Error::Service("light effect must be a string".into()))?;
        let effects = config_strings(&entity.platform_config, "effect_list");
        if !effects.is_empty() && !effects.iter().any(|known| known == effect) {
            return Err(Error::Service(
                "light effect is not in the configured effect_list".into(),
            ));
        }
        message.insert("effect".into(), Value::String(effect.into()));
        attrs.insert("effect".into(), Value::String(effect.into()));
    }
    if let Some(value) = call.data.get("flash") {
        let flash = value
            .as_str()
            .ok_or_else(|| Error::Service("light flash must be short or long".into()))?;
        let key = match flash {
            "short" => "flash_time_short",
            "long" => "flash_time_long",
            _ => return Err(Error::Service("light flash must be short or long".into())),
        };
        message.insert(
            "flash".into(),
            json!(config_number(
                &entity.platform_config,
                key,
                if flash == "short" { 2.0 } else { 10.0 }
            ) as u32),
        );
    }
    if let Some(value) = call.data.get("transition") {
        let transition =
            number_value(value, "transition").map_err(|error| Error::Service(error.to_string()))?;
        if transition < 0.0 {
            return Err(Error::Service("light transition cannot be negative".into()));
        }
        message.insert("transition".into(), json!(transition));
    }
    Ok((Value::Object(message).to_string(), attrs))
}

fn normalized_json_light_command(
    entity: &DiscoveredEntity,
    command: &NormalizedLightCommand,
) -> Result<(String, Map<String, Value>)> {
    let on = command.action == LightAction::On;
    let params = &command.params;
    let mut message = Map::new();
    message.insert(
        "state".into(),
        Value::String(if on { "ON" } else { "OFF" }.into()),
    );
    let mut attributes = Map::new();

    if let Some(brightness) = optional_u8_attribute(params, "brightness")? {
        let scale = config_number(&entity.platform_config, "brightness_scale", 255.0);
        message.insert(
            "brightness".into(),
            json!((f64::from(brightness) / 255.0 * scale).round().max(1.0) as u32),
        );
        attributes.insert("brightness".into(), json!(brightness));
    }
    if let Some(kelvin) = optional_u32_attribute(params, "color_temp_kelvin")? {
        let min = config_u32(&entity.platform_config, "min_kelvin", 1500);
        let max = config_u32(&entity.platform_config, "max_kelvin", 9000);
        if kelvin < min || kelvin > max {
            return Err(Error::Service(
                "light color temperature is outside configured Kelvin bounds".into(),
            ));
        }
        let raw = if config_bool(&entity.platform_config, "color_temp_kelvin", false) {
            f64::from(kelvin)
        } else {
            1_000_000.0 / f64::from(kelvin)
        };
        message.insert("color_temp".into(), json!(raw.round() as u32));
        attributes.insert("color_temp_kelvin".into(), json!(kelvin));
        attributes.insert("color_mode".into(), json!("color_temp"));
    }
    if let Some(hs) = optional_float_array::<2>(params, "hs_color")? {
        message.insert("color".into(), json!({"h": hs[0], "s": hs[1]}));
        attributes.insert("hs_color".into(), json!(hs));
        attributes.insert("color_mode".into(), json!("hs"));
    }
    if let Some(rgb) = optional_byte_array::<3>(params, "rgb_color")? {
        message.insert(
            "color".into(),
            json!({"r": rgb[0], "g": rgb[1], "b": rgb[2]}),
        );
        attributes.insert("rgb_color".into(), json!(rgb));
        attributes.insert("color_mode".into(), json!("rgb"));
    }
    if let Some(rgbw) = optional_byte_array::<4>(params, "rgbw_color")? {
        message.insert(
            "color".into(),
            json!({"r": rgbw[0], "g": rgbw[1], "b": rgbw[2], "w": rgbw[3]}),
        );
        attributes.insert("rgbw_color".into(), json!(rgbw));
        attributes.insert("color_mode".into(), json!("rgbw"));
    }
    if let Some(rgbww) = optional_byte_array::<5>(params, "rgbww_color")? {
        message.insert(
            "color".into(),
            json!({"r": rgbww[0], "g": rgbww[1], "b": rgbww[2], "c": rgbww[3], "w": rgbww[4]}),
        );
        attributes.insert("rgbww_color".into(), json!(rgbww));
        attributes.insert("color_mode".into(), json!("rgbww"));
    }
    if let Some(xy) = optional_float_array::<2>(params, "xy_color")? {
        message.insert("color".into(), json!({"x": xy[0], "y": xy[1]}));
        attributes.insert("xy_color".into(), json!(xy));
        attributes.insert("color_mode".into(), json!("xy"));
    }
    if let Some(white) = optional_u8_attribute(params, "white")? {
        let scale = config_number(&entity.platform_config, "white_scale", 255.0);
        message.insert(
            "white".into(),
            json!((f64::from(white) / 255.0 * scale).round().clamp(0.0, scale) as u32),
        );
        attributes.insert("brightness".into(), json!(white));
        attributes.insert("color_mode".into(), json!("white"));
    }
    if let Some(effect) = params.get("effect").and_then(Value::as_str) {
        let effects = config_strings(&entity.platform_config, "effect_list");
        if !effects.is_empty() && !effects.iter().any(|known| known == effect) {
            return Err(Error::Service(
                "light effect is not in the configured effect_list".into(),
            ));
        }
        message.insert("effect".into(), Value::String(effect.to_owned()));
        attributes.insert("effect".into(), Value::String(effect.to_owned()));
    }
    if let Some(flash) = params.get("flash").and_then(Value::as_str) {
        let (key, fallback) = match flash {
            "short" => ("flash_time_short", 2.0),
            "long" => ("flash_time_long", 10.0),
            _ => {
                return Err(Error::Service("light flash must be short or long".into()));
            }
        };
        message.insert(
            "flash".into(),
            json!(config_number(&entity.platform_config, key, fallback) as u32),
        );
    }
    if let Some(transition) = params.get("transition") {
        let seconds = number_value(transition, "transition")
            .map_err(|error| Error::Service(error.to_string()))?;
        message.insert("transition".into(), json!(seconds));
    }
    Ok((Value::Object(message).to_string(), attributes))
}

fn command_variables(
    runtime: &Runtime,
    call: &ServiceCall,
    value: &str,
    entity: &DiscoveredEntity,
) -> Map<String, Value> {
    let entity_id = mqtt_entity_id(entity);
    let mut variables = Map::new();
    variables.insert("value".into(), Value::String(value.to_owned()));
    variables.insert("entity_id".into(), Value::String(entity_id));
    variables.insert("name".into(), Value::String(entity.metadata.name.clone()));
    variables.insert("this".into(), mqtt_template_state(runtime, entity));
    for (key, value) in &call.data {
        if key != "entity_id" {
            variables.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in &call.target {
        if key != "entity_id" {
            variables.insert(key.clone(), value.clone());
        }
    }
    variables
}

fn requested_entity_ids(call: &ServiceCall) -> Result<Vec<String>> {
    let value = call
        .target
        .get("entity_id")
        .or_else(|| call.data.get("entity_id"))
        .ok_or_else(|| Error::Service("MQTT services require target.entity_id".into()))?;
    let ids = match value {
        Value::String(id) => vec![id.clone()],
        Value::Array(ids) => ids
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::Service("entity_id list must contain strings".into()))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(Error::Service(
                "entity_id must be a string or list of strings".into(),
            ));
        }
    };
    if ids.is_empty() || ids.iter().any(String::is_empty) {
        return Err(Error::Service("entity_id target cannot be empty".into()));
    }
    Ok(ids)
}

fn render_value(
    runtime: &Runtime,
    templates: &TemplateEngine,
    template: Option<&str>,
    payload: &str,
    entity: &DiscoveredEntity,
) -> Result<Value> {
    let Some(template) = template else {
        return Ok(Value::String(payload.to_owned()));
    };
    let variables = mqtt_value_variables(runtime, entity, payload);
    templates
        .render_value(template, &variables)
        .map_err(|error| Error::InvalidInput(format!("MQTT value template failed: {error}")))
}

fn render_string(
    runtime: &Runtime,
    templates: &TemplateEngine,
    template: Option<&str>,
    payload: &str,
    entity: &DiscoveredEntity,
) -> Result<String> {
    let Some(template) = template else {
        return Ok(payload.to_owned());
    };
    templates
        .render(template, &mqtt_value_variables(runtime, entity, payload))
        .map_err(|error| Error::InvalidInput(format!("MQTT template failed: {error}")))
}

fn value_variables(payload: &str, entity_id: &str) -> Map<String, Value> {
    let mut variables = Map::new();
    variables.insert("value".into(), Value::String(payload.to_owned()));
    variables.insert("entity_id".into(), Value::String(entity_id.to_owned()));
    if let Ok(value_json) = serde_json::from_str(payload) {
        variables.insert("value_json".into(), value_json);
    }
    variables
}

fn mqtt_value_variables(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    payload: &str,
) -> Map<String, Value> {
    let entity_id = mqtt_entity_id(entity);
    let mut variables = value_variables(payload, &entity_id);
    variables.insert("name".into(), Value::String(entity.metadata.name.clone()));
    variables.insert("this".into(), mqtt_template_state(runtime, entity));
    variables
}

fn mqtt_template_state(runtime: &Runtime, entity: &DiscoveredEntity) -> Value {
    let entity_id = mqtt_entity_id(entity);
    let state = runtime.get_state(&entity_id);
    let state_value = state
        .as_ref()
        .map(|state| state.state.clone())
        .unwrap_or_else(|| "unknown".into());
    let attributes = state
        .map(|state| state.attributes)
        .unwrap_or_else(|| entity.attributes.clone());
    json!({
        "entity_id": entity_id,
        "name": entity.metadata.name,
        "state": state_value,
        "attributes": attributes,
    })
}

fn state_string(entity: &DiscoveredEntity, value: Value) -> String {
    match entity.metadata.platform.as_str() {
        "switch" | "light" | "binary_sensor" | "fan" => {
            if value == Value::String(entity.state_on.clone()) {
                "on".into()
            } else if value == Value::String(entity.state_off.clone()) {
                "off".into()
            } else {
                "unknown".into()
            }
        }
        _ => match value {
            Value::Null => "unknown".into(),
            Value::String(value) => value,
            value @ Value::Number(_) | value @ Value::Bool(_) => value.to_string(),
            value => serde_json::to_string(&value).unwrap_or_else(|_| "unknown".into()),
        },
    }
}

fn parse_json_attributes(
    templates: &TemplateEngine,
    entity: &DiscoveredEntity,
    payload: &str,
) -> Result<Map<String, Value>> {
    let json_payload = if let Some(template) = &entity.attributes_template {
        let rendered = templates
            .render(
                template,
                &value_variables(payload, &entity.metadata.entity_id),
            )
            .map_err(|error| {
                Error::InvalidInput(format!("JSON attributes template failed: {error}"))
            })?;
        serde_json::from_str::<Value>(&rendered).map_err(|error| {
            Error::InvalidInput(format!(
                "JSON attributes template output is invalid: {error}"
            ))
        })?
    } else {
        serde_json::from_str::<Value>(payload).map_err(|error| {
            Error::InvalidInput(format!("JSON attributes payload is invalid: {error}"))
        })?
    };
    let object = json_payload
        .as_object()
        .ok_or_else(|| Error::InvalidInput("JSON attributes payload must be an object".into()))?;
    Ok(object.clone())
}

fn config_string<'a>(config: &'a Map<String, Value>, key: &str, default: &'a str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or(default)
}

fn config_number(config: &Map<String, Value>, key: &str, default: f64) -> f64 {
    config
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(default)
}

fn config_u32(config: &Map<String, Value>, key: &str, default: u32) -> u32 {
    config
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
        .and_then(|value| u32::try_from(value as u64).ok())
        .unwrap_or(default)
}

fn config_bool(config: &Map<String, Value>, key: &str, default: bool) -> bool {
    config.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn config_strings(config: &Map<String, Value>, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn number_value(value: &Value, field: &str) -> Result<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
    .ok_or_else(|| Error::InvalidInput(format!("MQTT {field} value is not a finite number")))?;
    Ok(number)
}

fn formatted_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn render_topic_value(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    topic_key: &str,
    payload: &str,
    templates: &TemplateEngine,
) -> Result<Value> {
    let template_key = match topic_key {
        "position_topic" => "position_template",
        "tilt_status_topic" => "tilt_status_template",
        "percentage_state_topic" => "percentage_value_template",
        "preset_mode_state_topic" => "preset_mode_value_template",
        "oscillation_state_topic" => "oscillation_value_template",
        "direction_state_topic" => "direction_value_template",
        "action_topic" => "action_template",
        "current_humidity_topic" => "current_humidity_template",
        "current_temperature_topic" => "current_temperature_template",
        "target_humidity_state_topic" => "target_humidity_state_template",
        "mode_state_topic" => "mode_state_template",
        "fan_mode_state_topic" => "fan_mode_state_template",
        "swing_horizontal_mode_state_topic" => "swing_horizontal_mode_state_template",
        "swing_mode_state_topic" => "swing_mode_state_template",
        "temperature_state_topic" => "temperature_state_template",
        "temperature_low_state_topic" => "temperature_low_state_template",
        "temperature_high_state_topic" => "temperature_high_state_template",
        _ => "value_template",
    };
    let template = entity
        .platform_config
        .get(template_key)
        .and_then(Value::as_str)
        .or(entity.value_template.as_deref());
    let Some(template) = template else {
        return Ok(Value::String(payload.to_owned()));
    };
    let mut variables = mqtt_value_variables(runtime, entity, payload);
    for key in ["position_open", "position_closed", "tilt_min", "tilt_max"] {
        if let Some(value) = entity.platform_config.get(key) {
            variables.insert(key.into(), value.clone());
        }
    }
    templates
        .render_value(template, &variables)
        .map_err(|error| Error::InvalidInput(format!("MQTT state template failed: {error}")))
}

fn supported_light_mode(entity: &DiscoveredEntity, mode: &str) -> bool {
    if let Some(modes) = entity
        .platform_config
        .get("supported_color_modes")
        .and_then(Value::as_array)
    {
        modes.iter().any(|item| item.as_str() == Some(mode))
    } else if config_bool(&entity.platform_config, "brightness", false) {
        mode == "brightness"
    } else {
        mode == "onoff"
    }
}

fn update_light_json(
    entity: &DiscoveredEntity,
    payload: &str,
    mut current: LastValue,
) -> Result<LastValue> {
    let value: Value = serde_json::from_str(payload).map_err(|error| {
        Error::InvalidInput(format!("MQTT JSON light state is invalid: {error}"))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidInput("MQTT JSON light state must be an object".into()))?;
    if let Some(state) = object.get("state") {
        current.state = match state.as_str() {
            Some("ON") => "on".into(),
            Some("OFF") => "off".into(),
            None if state.is_null() => "unknown".into(),
            _ => {
                return Err(Error::InvalidInput(
                    "MQTT JSON light state must be ON, OFF, or null".into(),
                ));
            }
        };
    } else {
        current.state = "unknown".into();
    }
    if let Some(brightness) = object.get("brightness") {
        if !light_supports_brightness(entity) {
            return Err(Error::InvalidInput(
                "MQTT JSON light reported unsupported brightness".into(),
            ));
        }
        let raw = number_value(brightness, "brightness")?;
        let scale = config_number(&entity.platform_config, "brightness_scale", 255.0);
        if raw < 0.0 || raw > scale {
            return Err(Error::InvalidInput(
                "MQTT JSON light brightness is outside its configured scale".into(),
            ));
        }
        if raw > 0.0 {
            current.attributes.insert(
                "brightness".into(),
                json!((raw / scale * 255.0).round().clamp(1.0, 255.0) as u8),
            );
        }
    }
    if let Some(mode) = object.get("color_mode").and_then(Value::as_str) {
        if !matches!(
            mode,
            "onoff"
                | "brightness"
                | "color_temp"
                | "hs"
                | "rgb"
                | "rgbw"
                | "rgbww"
                | "white"
                | "xy"
        ) || !supported_light_mode(entity, mode)
        {
            return Err(Error::InvalidInput(format!(
                "MQTT JSON light reported unsupported color mode {mode:?}"
            )));
        }
        current
            .attributes
            .insert("color_mode".into(), Value::String(mode.into()));
        match mode {
            "color_temp" => {
                let raw = number_value(
                    object.get("color_temp").ok_or_else(|| {
                        Error::InvalidInput(
                            "MQTT JSON light color_temp mode requires color_temp".into(),
                        )
                    })?,
                    "color_temp",
                )?;
                let kelvin = if config_bool(&entity.platform_config, "color_temp_kelvin", false) {
                    raw
                } else if raw > 0.0 {
                    1_000_000.0 / raw
                } else {
                    return Err(Error::InvalidInput(
                        "MQTT JSON light mireds must be positive".into(),
                    ));
                };
                let min = config_number(&entity.platform_config, "min_kelvin", 1500.0);
                let max = config_number(&entity.platform_config, "max_kelvin", 9000.0);
                if !(min..=max).contains(&kelvin) {
                    return Err(Error::InvalidInput(
                        "MQTT JSON light color temperature is outside configured Kelvin bounds"
                            .into(),
                    ));
                }
                current
                    .attributes
                    .insert("color_temp_kelvin".into(), json!(kelvin.round() as u32));
            }
            "hs" | "rgb" | "rgbw" | "rgbww" | "xy" => {
                let color = object
                    .get("color")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        Error::InvalidInput("MQTT JSON light color must be an object".into())
                    })?;
                let (keys, attribute): (&[&str], &str) = match mode {
                    "hs" => (&["h", "s"], "hs_color"),
                    "rgb" => (&["r", "g", "b"], "rgb_color"),
                    "rgbw" => (&["r", "g", "b", "w"], "rgbw_color"),
                    "rgbww" => (&["r", "g", "b", "c", "w"], "rgbww_color"),
                    _ => (&["x", "y"], "xy_color"),
                };
                let mut output = Vec::with_capacity(keys.len());
                for (index, key) in keys.iter().enumerate() {
                    let number = number_value(
                        color.get(*key).ok_or_else(|| {
                            Error::InvalidInput(format!("MQTT JSON light color requires {key}"))
                        })?,
                        "color",
                    )?;
                    let valid = match mode {
                        "hs" if index == 0 => (0.0..=360.0).contains(&number),
                        "hs" => (0.0..=100.0).contains(&number),
                        "rgb" | "rgbw" | "rgbww" => (0.0..=255.0).contains(&number),
                        _ => (0.0..=1.0).contains(&number),
                    };
                    if !valid {
                        return Err(Error::InvalidInput(
                            "MQTT JSON light color value is outside its supported range".into(),
                        ));
                    }
                    output.push(json!(number));
                }
                current
                    .attributes
                    .insert(attribute.into(), Value::Array(output));
            }
            _ => {}
        }
    }
    if let Some(effect) = object.get("effect") {
        if !config_bool(&entity.platform_config, "effect", false) {
            return Err(Error::InvalidInput(
                "MQTT JSON light reported an effect without effect support".into(),
            ));
        }
        let effect = effect
            .as_str()
            .ok_or_else(|| Error::InvalidInput("MQTT JSON light effect must be a string".into()))?;
        let effects = config_strings(&entity.platform_config, "effect_list");
        if !effects.is_empty() && !effects.iter().any(|known| known == effect) {
            return Err(Error::InvalidInput(
                "MQTT JSON light reported an unknown effect".into(),
            ));
        }
        current
            .attributes
            .insert("effect".into(), Value::String(effect.into()));
    }
    Ok(current)
}

fn climate_state_field_key(field: ClimateStateField) -> &'static str {
    match field {
        ClimateStateField::Action => "action_topic",
        ClimateStateField::CurrentHumidity => "current_humidity_topic",
        ClimateStateField::CurrentTemperature => "current_temperature_topic",
        ClimateStateField::TargetHumidity => "target_humidity_state_topic",
        ClimateStateField::HvacMode => "mode_state_topic",
        ClimateStateField::FanMode => "fan_mode_state_topic",
        ClimateStateField::SwingHorizontalMode => "swing_horizontal_mode_state_topic",
        ClimateStateField::SwingMode => "swing_mode_state_topic",
        ClimateStateField::PresetMode => "preset_mode_state_topic",
        ClimateStateField::TargetTemperature => "temperature_state_topic",
        ClimateStateField::TargetTemperatureLow => "temperature_low_state_topic",
        ClimateStateField::TargetTemperatureHigh => "temperature_high_state_topic",
    }
}
fn climate_state_text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}
fn climate_number_update(value: &str, field: &str) -> Result<Option<f64>> {
    if value == "None" {
        return Ok(None);
    }
    number_value(&Value::String(value.into()), field)
        .map(Some)
        .map_err(|error| Error::InvalidInput(error.to_string()))
}
fn apply_climate_state_field(
    entity: &DiscoveredEntity,
    field: ClimateStateField,
    value: Value,
    state: &mut MqttClimateState,
) -> Result<bool> {
    let text = climate_state_text(&value);
    if text.is_empty() {
        return Ok(false);
    }
    match field {
        ClimateStateField::Action => {
            if text == "None" {
                state.snapshot.hvac_action = None;
            } else {
                state.snapshot.hvac_action = Some(text.parse::<HvacAction>().map_err(|error| {
                    Error::InvalidInput(format!("invalid MQTT climate action: {error}"))
                })?);
            }
        }
        ClimateStateField::CurrentHumidity => {
            state.snapshot.current_humidity = climate_number_update(&text, "current humidity")?;
        }
        ClimateStateField::CurrentTemperature => {
            state.snapshot.current_temperature =
                climate_number_update(&text, "current temperature")?;
        }
        ClimateStateField::TargetHumidity => {
            state.snapshot.target_humidity = climate_number_update(&text, "target humidity")?;
        }
        ClimateStateField::HvacMode => {
            if text == "None" {
                state.snapshot.hvac_mode = None;
            } else {
                let mode = text.parse::<HvacMode>().map_err(|error| {
                    Error::InvalidInput(format!("invalid MQTT climate HVAC mode: {error}"))
                })?;
                if !config_strings(&entity.platform_config, "modes")
                    .iter()
                    .any(|known| known == mode.as_str())
                {
                    return Err(Error::InvalidInput(format!(
                        "unknown MQTT climate HVAC mode {text:?}"
                    )));
                }
                state.snapshot.hvac_mode = Some(mode);
            }
        }
        ClimateStateField::FanMode => {
            state.snapshot.fan_mode = climate_mode_state_value(entity, "fan_modes", "fan", &text)?;
        }
        ClimateStateField::SwingHorizontalMode => {
            state.snapshot.swing_horizontal_mode = climate_mode_state_value(
                entity,
                "swing_horizontal_modes",
                "horizontal_swing",
                &text,
            )?;
        }
        ClimateStateField::SwingMode => {
            state.snapshot.swing_mode =
                climate_mode_state_value(entity, "swing_modes", "swing", &text)?;
        }
        ClimateStateField::PresetMode => {
            if text == "None" || text == hass_climate::PRESET_NONE {
                state.snapshot.preset_mode = Some(hass_climate::PRESET_NONE.into());
            } else if config_strings(&entity.platform_config, "preset_modes")
                .iter()
                .any(|known| known == &text)
            {
                state.snapshot.preset_mode = Some(text);
            } else {
                return Err(Error::InvalidInput(format!(
                    "unknown MQTT climate preset mode {text:?}"
                )));
            }
        }
        ClimateStateField::TargetTemperature => {
            state.snapshot.target_temperature = climate_number_update(&text, "target temperature")?;
            if state.snapshot.target_temperature.is_some() {
                climate_park_range(state);
            }
        }
        ClimateStateField::TargetTemperatureLow => {
            state.snapshot.target_temperature_low =
                climate_number_update(&text, "target temperature low")?;
            if state.snapshot.target_temperature_low.is_some() {
                climate_park_single(state);
            }
        }
        ClimateStateField::TargetTemperatureHigh => {
            state.snapshot.target_temperature_high =
                climate_number_update(&text, "target temperature high")?;
            if state.snapshot.target_temperature_high.is_some() {
                climate_park_single(state);
            }
        }
    }
    Ok(true)
}
fn climate_mode_state_value(
    entity: &DiscoveredEntity,
    list_key: &str,
    family: &str,
    value: &str,
) -> Result<Option<String>> {
    if value == "None" {
        return Ok(None);
    }
    let known = config_strings(&entity.platform_config, list_key);
    if known.iter().any(|mode| mode == value) {
        Ok(Some(value.to_owned()))
    } else {
        Err(Error::InvalidInput(format!(
            "unknown MQTT climate {family} mode {value:?}"
        )))
    }
}
fn update_climate_reported_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    topic: &str,
    payload: &str,
    templates: &TemplateEngine,
    mut current: MqttClimateState,
) -> Result<Option<MqttClimateState>> {
    let Some(fields) = entity
        .climate
        .as_ref()
        .and_then(|climate| climate.state_topics.get(topic))
    else {
        return Ok(None);
    };
    let mut changed = false;
    for field in fields {
        let key = climate_state_field_key(*field);
        let value = match render_topic_value(runtime, entity, key, payload, templates) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    entity_id = %entity.metadata.entity_id,
                    topic,
                    field = key,
                    error = %error,
                    "ignoring invalid MQTT climate state field"
                );
                continue;
            }
        };
        match apply_climate_state_field(entity, *field, value, &mut current) {
            Ok(field_changed) => changed |= field_changed,
            Err(error) => warn!(
                entity_id = %entity.metadata.entity_id,
                topic,
                field = key,
                error = %error,
                "ignoring invalid MQTT climate state field"
            ),
        }
    }
    Ok(changed.then_some(current))
}
fn update_reported_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    topic: &str,
    payload: &str,
    templates: &TemplateEngine,
    mut current: LastValue,
) -> Result<LastValue> {
    if entity.state_topic.as_deref() == Some(topic) {
        if entity.metadata.platform == "light"
            && config_string(&entity.platform_config, "schema", "basic") == "json"
        {
            return update_light_json(entity, payload, current);
        }
        let template = if entity.metadata.platform == "fan" {
            entity
                .platform_config
                .get("state_value_template")
                .and_then(Value::as_str)
        } else {
            entity.value_template.as_deref()
        };
        let value = render_value(runtime, templates, template, payload, entity)?;
        match entity.metadata.platform.as_str() {
            "cover" => {
                let state = value.as_str().unwrap_or_default();
                let mapped = [
                    ("state_closed", "closed"),
                    ("state_open", "open"),
                    ("state_opening", "opening"),
                    ("state_closing", "closing"),
                    ("state_stopped", "stopped"),
                ]
                .into_iter()
                .find_map(|(key, fallback)| {
                    (state == config_string(&entity.platform_config, key, fallback))
                        .then_some(fallback)
                })
                .ok_or_else(|| {
                    Error::InvalidInput(format!("unknown MQTT cover state {state:?}"))
                })?;
                current.state = mapped.into();
            }
            "number" => {
                if value.as_str()
                    == Some(config_string(
                        &entity.platform_config,
                        "payload_reset",
                        "None",
                    ))
                {
                    current.state = "unknown".into();
                    current.attributes.insert("value".into(), Value::Null);
                } else {
                    let number = number_value(&value, "number")?;
                    let min = config_number(&entity.platform_config, "min", 0.0);
                    let max = config_number(&entity.platform_config, "max", 100.0);
                    if !(min..=max).contains(&number) {
                        return Err(Error::InvalidInput(
                            "MQTT number state is outside its configured range".into(),
                        ));
                    }
                    current.state = formatted_number(number);
                    current.attributes.insert("value".into(), json!(number));
                }
            }
            "select" => {
                let option = value.as_str().unwrap_or_default();
                if option.eq_ignore_ascii_case("none") {
                    current.state = "unknown".into();
                } else if config_strings(&entity.platform_config, "options")
                    .iter()
                    .any(|known| known == option)
                {
                    current.state = option.to_owned();
                } else {
                    return Err(Error::InvalidInput(format!(
                        "unknown MQTT select option {option:?}"
                    )));
                }
            }
            "text" => {
                let text = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                let min = config_number(&entity.platform_config, "min", 0.0) as usize;
                let max = config_number(&entity.platform_config, "max", 255.0) as usize;
                if text.chars().count() < min
                    || text.chars().count() > max
                    || entity.text_pattern.as_ref().is_some_and(|pattern| {
                        !pattern
                            .find(&text)
                            .is_some_and(|matched| matched.start() == 0)
                    })
                {
                    return Err(Error::InvalidInput(
                        "MQTT text state violates its configured length or pattern".into(),
                    ));
                }
                current.state = text.clone();
                current
                    .attributes
                    .insert("value".into(), Value::String(text));
            }
            "fan" => current.state = state_string(entity, value),
            _ => current.state = state_string(entity, value),
        }
    } else if let Some(topic_key) = entity.extra_state_topics.get(topic) {
        let value = render_topic_value(runtime, entity, topic_key, payload, templates)?;
        match topic_key.as_str() {
            "position_topic" => {
                let value = value
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .unwrap_or(value);
                let (position, tilt) = if let Some(object) = value.as_object() {
                    let position = object.get("position").ok_or_else(|| {
                        Error::InvalidInput("cover position JSON requires position".into())
                    })?;
                    (
                        number_value(position, "cover position")?,
                        object
                            .get("tilt_position")
                            .map(|value| number_value(value, "cover tilt"))
                            .transpose()?,
                    )
                } else {
                    (number_value(&value, "cover position")?, None)
                };
                let min = config_number(&entity.platform_config, "position_closed", 0.0);
                let max = config_number(&entity.platform_config, "position_open", 100.0);
                if position < min || position > max {
                    return Err(Error::InvalidInput(
                        "MQTT cover position is outside its configured range".into(),
                    ));
                }
                current.attributes.insert(
                    "current_position".into(),
                    json!(
                        ((position - min) / (max - min) * 100.0)
                            .round()
                            .clamp(0.0, 100.0) as u8
                    ),
                );
                if let Some(tilt) = tilt {
                    let tilt_min = config_number(&entity.platform_config, "tilt_min", 0.0);
                    let tilt_max = config_number(&entity.platform_config, "tilt_max", 100.0);
                    if tilt < tilt_min || tilt > tilt_max {
                        return Err(Error::InvalidInput(
                            "MQTT cover tilt is outside its configured range".into(),
                        ));
                    }
                    current.attributes.insert(
                        "current_tilt_position".into(),
                        json!(
                            ((tilt - tilt_min) / (tilt_max - tilt_min) * 100.0)
                                .round()
                                .clamp(0.0, 100.0) as u8
                        ),
                    );
                }
            }
            "tilt_status_topic" => {
                let tilt = number_value(&value, "cover tilt")?;
                let min = config_number(&entity.platform_config, "tilt_min", 0.0);
                let max = config_number(&entity.platform_config, "tilt_max", 100.0);
                if tilt < min || tilt > max {
                    return Err(Error::InvalidInput(
                        "MQTT cover tilt is outside its configured range".into(),
                    ));
                }
                current.attributes.insert(
                    "current_tilt_position".into(),
                    json!(
                        ((tilt - min) / (max - min) * 100.0)
                            .round()
                            .clamp(0.0, 100.0) as u8
                    ),
                );
            }
            "percentage_state_topic" => {
                if value.as_str()
                    == Some(config_string(
                        &entity.platform_config,
                        "payload_reset_percentage",
                        "None",
                    ))
                {
                    current.attributes.insert("percentage".into(), Value::Null);
                } else {
                    let speed = number_value(&value, "fan percentage")?;
                    let min = config_number(&entity.platform_config, "speed_range_min", 1.0);
                    let max = config_number(&entity.platform_config, "speed_range_max", 100.0);
                    let percentage = hass_fan::ranged_value_to_percentage((min, max), speed)
                        .map_err(|error| Error::InvalidInput(error.to_string()))?;
                    if !(0..=100).contains(&percentage) {
                        return Err(Error::InvalidInput(
                            "MQTT fan percentage is outside its configured speed range".into(),
                        ));
                    }
                    current
                        .attributes
                        .insert("percentage".into(), json!(percentage));
                }
            }
            "preset_mode_state_topic" => {
                let preset = value.as_str().unwrap_or_default();
                if preset
                    == config_string(&entity.platform_config, "payload_reset_preset_mode", "None")
                {
                    current.attributes.insert("preset_mode".into(), Value::Null);
                } else if config_strings(&entity.platform_config, "preset_modes")
                    .iter()
                    .any(|known| known == preset)
                {
                    current
                        .attributes
                        .insert("preset_mode".into(), Value::String(preset.into()));
                } else {
                    return Err(Error::InvalidInput(format!(
                        "unknown MQTT fan preset mode {preset:?}"
                    )));
                }
            }
            "oscillation_state_topic" => {
                let state = value.as_str().unwrap_or_default();
                if state
                    == config_string(
                        &entity.platform_config,
                        "payload_oscillation_on",
                        "oscillate_on",
                    )
                {
                    current
                        .attributes
                        .insert("oscillating".into(), Value::Bool(true));
                } else if state
                    == config_string(
                        &entity.platform_config,
                        "payload_oscillation_off",
                        "oscillate_off",
                    )
                {
                    current
                        .attributes
                        .insert("oscillating".into(), Value::Bool(false));
                } else {
                    return Err(Error::InvalidInput(format!(
                        "unknown MQTT fan oscillation state {state:?}"
                    )));
                }
            }
            "direction_state_topic" => {
                let direction = value.as_str().unwrap_or_default();
                if !matches!(direction, "forward" | "reverse") {
                    return Err(Error::InvalidInput(format!(
                        "unknown MQTT fan direction {direction:?}"
                    )));
                }
                current
                    .attributes
                    .insert("direction".into(), Value::String(direction.into()));
            }
            _ => {}
        }
    }
    Ok(current)
}

fn entity_available(
    entity: &DiscoveredEntity,
    connected: bool,
    status: &HashMap<(String, String), bool>,
) -> bool {
    if !connected {
        return false;
    }
    if entity.availability.is_empty() {
        return true;
    }
    let values = entity.availability.iter().map(|item| {
        status
            .get(&(entity.metadata.entity_id.clone(), item.topic.clone()))
            .copied()
            .unwrap_or(false)
    });
    match entity.availability_mode {
        AvailabilityMode::All => values.into_iter().all(|value| value),
        AvailabilityMode::Any => values.into_iter().any(|value| value),
    }
}

fn apply_availability(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    available: bool,
    last_values: &HashMap<String, LastValue>,
) {
    if available {
        if let Some(last) = last_values.get(&entity.metadata.entity_id) {
            publish_discovered_state(
                runtime,
                entity,
                Some(&last.state),
                true,
                last.attributes.clone(),
                last.context.clone(),
            );
        } else {
            publish_discovered_state(
                runtime,
                entity,
                None,
                true,
                entity.attributes.clone(),
                Context::default(),
            );
        }
    } else {
        let last = last_values.get(&entity.metadata.entity_id);
        let attributes = last
            .map(|value| value.attributes.clone())
            .unwrap_or_else(|| entity.attributes.clone());
        publish_discovered_state(
            runtime,
            entity,
            last.map(|value| value.state.as_str()),
            false,
            attributes,
            last.map(|value| value.context.clone()).unwrap_or_default(),
        );
    }
}

fn publish_climate_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    state: &MqttClimateState,
    available: bool,
    context: Context,
) -> Result<Option<(State, bool)>> {
    let climate = entity
        .climate
        .as_ref()
        .ok_or_else(|| Error::Service("MQTT climate state is missing".into()))?;
    let descriptor = match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Climate { descriptor, .. }) => descriptor,
        _ => &climate.descriptor,
    };
    let mut next = state.clone();
    next.snapshot.available = available;
    next.snapshot.context = context;
    *climate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
    let publication = descriptor
        .publication(&next.snapshot, climate_unit_context(runtime.unit_system()))
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    let projected = publication.snapshot.clone();
    let change = if let Some(handle) = entity_lifecycle_handle(entity) {
        if !handle.status().is_active() {
            return Ok(None);
        }
        handle.publish_publication(publication)?
    } else {
        let state = if projected.available {
            projected.state.as_deref().unwrap_or("unknown")
        } else {
            "unavailable"
        };
        Some(runtime.set_state(
            &entity.metadata.entity_id,
            state,
            projected.attributes,
            projected.context,
            false,
        )?)
    };
    Ok(change.map(|change| (change.state, change.changed)))
}
fn reproject_climate_entities(
    runtime: &Runtime,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    connected: bool,
    availability_status: &HashMap<(String, String), bool>,
    _last_values: &HashMap<String, LastValue>,
) {
    let discovered = entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|entity| entity.climate.is_some())
        .cloned()
        .collect::<Vec<_>>();
    for entity in discovered {
        if !mqtt_entity_is_active(&entity) {
            continue;
        }
        let Some(climate) = entity.climate.as_ref() else {
            continue;
        };
        let state = climate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let available = entity_available(&entity, connected, availability_status);
        if let Err(error) = publish_climate_state(
            runtime,
            &entity,
            &state,
            available,
            state.snapshot.context.clone(),
        ) {
            warn!(entity_id = %mqtt_entity_id(&entity), error = %error, "failed to reproject MQTT climate after Core unit update");
        }
    }
}
fn update_all_availability(
    runtime: &Runtime,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    connected: bool,
    availability_status: &HashMap<(String, String), bool>,
    last_values: &HashMap<String, LastValue>,
) {
    for entity in entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
    {
        apply_availability(
            runtime,
            entity,
            entity_available(entity, connected, availability_status),
            last_values,
        );
    }
}

fn set_runtime_state(
    runtime: &Runtime,
    entity_id: &str,
    state: &str,
    attributes: Map<String, Value>,
) {
    if let Err(error) = runtime.set_state(entity_id, state, attributes, Context::default(), false) {
        warn!(entity_id, error = %error, "failed to update runtime from MQTT");
    }
}

fn entity_lifecycle_handle(entity: &DiscoveredEntity) -> Option<&EntityHandle> {
    entity.shared_entity.as_ref().map(SharedMqttEntity::handle)
}

fn shared_entity_projection(
    entity: &DiscoveredEntity,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) -> Result<Option<(EntityPublication, EntitySnapshot)>> {
    let Some(shared) = entity.shared_entity.as_ref() else {
        return Ok(None);
    };
    match shared {
        SharedMqttEntity::Switch(_) => {
            let is_on = match state {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            let snapshot = SwitchSnapshot::new(is_on)
                .with_available(available)
                .with_assumed_state(entity.optimistic)
                .with_extra_attributes(attributes)
                .with_context(context);
            let entity_snapshot = snapshot.entity_snapshot();
            Ok(Some((snapshot.publication(), entity_snapshot)))
        }
        SharedMqttEntity::Light { descriptor, .. } => {
            let snapshot =
                light_snapshot(entity, descriptor, state, available, attributes, context)?;
            let projection = snapshot
                .project(descriptor)
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            Ok(Some((projection.publication, projection.snapshot)))
        }
        SharedMqttEntity::Cover {
            descriptor,
            toggle_state,
            ..
        } => {
            let snapshot = cover_snapshot(entity, state, available, attributes, context)?;
            if available {
                toggle_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .observe(&snapshot);
            }
            let entity_snapshot = snapshot.entity_snapshot(descriptor);
            let publication = snapshot.publication(descriptor);
            Ok(Some((publication, entity_snapshot)))
        }
        SharedMqttEntity::Fan { descriptor, .. } => {
            let snapshot = fan_snapshot(entity, state, available, attributes, context)?;
            let publication = descriptor
                .publication(&snapshot)
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            let entity_snapshot = publication.snapshot.clone();
            Ok(Some((publication, entity_snapshot)))
        }
        SharedMqttEntity::Climate { .. } => Ok(None),
        SharedMqttEntity::Button { .. } => Ok(None),
    }
}

fn light_snapshot(
    _entity: &DiscoveredEntity,
    descriptor: &LightDescriptor,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) -> Result<LightSnapshot> {
    let is_on = match state {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    };
    let reported_mode = match attributes.get(hass_light::attr::COLOR_MODE) {
        Some(Value::String(value)) => Some(
            value
                .parse::<LightColorMode>()
                .map_err(|error| Error::InvalidInput(error.to_string()))?,
        ),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(Error::InvalidInput(
                "MQTT light color_mode must be a string or null".into(),
            ));
        }
    };
    let color_mode = reported_mode.or_else(|| {
        let modes = descriptor.supported_color_modes()?;
        (is_on == Some(true) && modes.len() == 1).then_some(modes[0])
    });
    let mut extra = attributes.clone();
    for key in [
        "friendly_name",
        "unique_id",
        "platform",
        "optimistic",
        hass_light::attr::SUPPORTED_COLOR_MODES,
        hass_light::attr::SUPPORTED_FEATURES,
        hass_light::attr::MIN_COLOR_TEMP_KELVIN,
        hass_light::attr::MAX_COLOR_TEMP_KELVIN,
        hass_light::attr::EFFECT_LIST,
        hass_light::attr::COLOR_MODE,
        hass_light::attr::BRIGHTNESS,
        hass_light::attr::COLOR_TEMP_KELVIN,
        hass_light::attr::HS_COLOR,
        hass_light::attr::RGB_COLOR,
        hass_light::attr::RGBW_COLOR,
        hass_light::attr::RGBWW_COLOR,
        hass_light::attr::XY_COLOR,
        hass_light::attr::EFFECT,
    ] {
        extra.remove(key);
    }
    let snapshot = LightSnapshot::new(is_on)
        .with_available(available)
        .with_brightness(optional_u8_attribute(
            &attributes,
            hass_light::attr::BRIGHTNESS,
        )?)
        .with_color_mode(color_mode)
        .with_color_temp_kelvin(optional_u32_attribute(
            &attributes,
            hass_light::attr::COLOR_TEMP_KELVIN,
        )?)
        .with_hs_color(optional_float_array::<2>(
            &attributes,
            hass_light::attr::HS_COLOR,
        )?)
        .with_xy_color(optional_float_array::<2>(
            &attributes,
            hass_light::attr::XY_COLOR,
        )?)
        .with_rgb_color(optional_byte_array::<3>(
            &attributes,
            hass_light::attr::RGB_COLOR,
        )?)
        .with_rgbw_color(optional_byte_array::<4>(
            &attributes,
            hass_light::attr::RGBW_COLOR,
        )?)
        .with_rgbww_color(optional_byte_array::<5>(
            &attributes,
            hass_light::attr::RGBWW_COLOR,
        )?)
        .with_effect(
            attributes
                .get(hass_light::attr::EFFECT)
                .and_then(Value::as_str),
        )
        .with_extra_attributes(std::mem::take(&mut extra))
        .with_context(context);
    Ok(snapshot)
}

fn cover_snapshot(
    _entity: &DiscoveredEntity,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) -> Result<CoverSnapshot> {
    let (is_closed, is_opening, is_closing) = match state {
        Some("closed") => (Some(true), None, None),
        Some("open") => (Some(false), None, None),
        Some("opening") => (None, Some(true), None),
        Some("closing") => (None, None, Some(true)),
        _ => (None, None, None),
    };
    let current_position = optional_i64_attribute(&attributes, hass_cover::ATTR_CURRENT_POSITION)?;
    let current_tilt_position =
        optional_i64_attribute(&attributes, hass_cover::ATTR_CURRENT_TILT_POSITION)?;
    let supported_speeds = optional_string_array(&attributes, hass_cover::ATTR_SUPPORTED_SPEEDS)?;
    let mut extra = attributes;
    for key in [
        hass_cover::ATTR_IS_CLOSED,
        hass_cover::ATTR_CURRENT_POSITION,
        hass_cover::ATTR_CURRENT_TILT_POSITION,
        hass_cover::ATTR_SUPPORTED_SPEEDS,
        hass_cover::ATTR_SUPPORTED_FEATURES,
    ] {
        extra.remove(key);
    }
    Ok(CoverSnapshot::new(is_closed)
        .with_is_opening(is_opening)
        .with_is_closing(is_closing)
        .with_current_position(current_position)
        .with_current_tilt_position(current_tilt_position)
        .with_supported_speeds(supported_speeds)
        .with_available(available)
        .with_extra_attributes(extra)
        .with_context(context))
}

fn fan_snapshot(
    _entity: &DiscoveredEntity,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) -> Result<FanSnapshot> {
    let percentage = optional_i64_attribute(&attributes, hass_fan::FAN_ATTR_PERCENTAGE)?;
    let preset_mode = optional_string_attribute(&attributes, hass_fan::FAN_ATTR_PRESET_MODE)?;
    let current_direction = optional_string_attribute(&attributes, hass_fan::FAN_ATTR_DIRECTION)?;
    let oscillating = optional_bool_attribute(&attributes, hass_fan::FAN_ATTR_OSCILLATING)?;
    let mut extra = attributes;
    for key in [
        hass_fan::FAN_ATTR_PERCENTAGE,
        hass_fan::FAN_ATTR_PERCENTAGE_STEP,
        hass_fan::FAN_ATTR_PRESET_MODE,
        hass_fan::FAN_ATTR_PRESET_MODES,
        hass_fan::FAN_ATTR_DIRECTION,
        hass_fan::FAN_ATTR_OSCILLATING,
        "supported_features",
    ] {
        extra.remove(key);
    }
    let mut snapshot = FanSnapshot::new()
        .with_percentage(percentage)
        .with_preset_mode(preset_mode)
        .with_current_direction(current_direction)
        .with_oscillating(oscillating)
        .with_available(available)
        .with_context(context);
    snapshot = snapshot.with_reported_state(match state {
        Some("on") => FanState::On,
        Some("off") => FanState::Off,
        _ => FanState::Unknown,
    });
    for (key, value) in extra {
        snapshot = snapshot.with_extra_attribute(key, Some(value));
    }
    Ok(snapshot)
}

fn optional_float_array<const N: usize>(
    attributes: &Map<String, Value>,
    key: &str,
) -> Result<Option<[f64; N]>> {
    let Some(value) = attributes.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let values = value.as_array().ok_or_else(|| {
        Error::InvalidInput(format!("MQTT light {key} must be a {N}-value array"))
    })?;
    if values.len() != N {
        return Err(Error::InvalidInput(format!(
            "MQTT light {key} must have {N} values"
        )));
    }
    let mut output = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        output[index] = value
            .as_f64()
            .filter(|component| component.is_finite())
            .ok_or_else(|| Error::InvalidInput(format!("MQTT light {key} must be numeric")))?;
    }
    Ok(Some(output))
}

fn optional_byte_array<const N: usize>(
    attributes: &Map<String, Value>,
    key: &str,
) -> Result<Option<[u8; N]>> {
    let Some(value) = attributes.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let values = value.as_array().ok_or_else(|| {
        Error::InvalidInput(format!("MQTT light {key} must be a {N}-value array"))
    })?;
    if values.len() != N {
        return Err(Error::InvalidInput(format!(
            "MQTT light {key} must have {N} values"
        )));
    }
    let mut output = [0; N];
    for (index, value) in values.iter().enumerate() {
        let number = value
            .as_u64()
            .and_then(|number| u8::try_from(number).ok())
            .ok_or_else(|| Error::InvalidInput(format!("MQTT light {key} values must be bytes")))?;
        output[index] = number;
    }
    Ok(Some(output))
}

fn optional_u8_attribute(attributes: &Map<String, Value>, key: &str) -> Result<Option<u8>> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .ok_or_else(|| Error::InvalidInput(format!("MQTT attribute {key} must be a byte")))
        })
        .transpose()
}

fn optional_u32_attribute(attributes: &Map<String, Value>, key: &str) -> Result<Option<u32>> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| Error::InvalidInput(format!("MQTT attribute {key} must be a u32")))
        })
        .transpose()
}

fn optional_i64_attribute(attributes: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
                .ok_or_else(|| {
                    Error::InvalidInput(format!("MQTT attribute {key} must be an integer"))
                })
        })
        .transpose()
}

fn optional_string_attribute(attributes: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                Error::InvalidInput(format!("MQTT attribute {key} must be a string"))
            })
        })
        .transpose()
}

fn optional_string_array(
    attributes: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = attributes.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let values = value
        .as_array()
        .ok_or_else(|| Error::InvalidInput(format!("MQTT attribute {key} must be a list")))?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                Error::InvalidInput(format!("MQTT attribute {key} must contain strings"))
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn optional_bool_attribute(attributes: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                Error::InvalidInput(format!("MQTT attribute {key} must be a boolean"))
            })
        })
        .transpose()
}

fn mqtt_entity_id(entity: &DiscoveredEntity) -> String {
    entity_lifecycle_handle(entity)
        .map(|handle| handle.status().entity_id().to_owned())
        .unwrap_or_else(|| entity.metadata.entity_id.clone())
}

fn mqtt_entity_is_active(entity: &DiscoveredEntity) -> bool {
    entity_lifecycle_handle(entity).is_none_or(|handle| handle.status().is_active())
}

fn refresh_shared_entity_ids(
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    metadata: &RwLock<BTreeMap<String, MqttEntityMetadata>>,
    availability_status: &mut HashMap<(String, String), bool>,
    last_values: &mut HashMap<String, LastValue>,
) {
    let mut renamed = Vec::new();
    {
        let mut entities = entities
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entity in entities.values_mut() {
            let Some(handle) = entity_lifecycle_handle(entity) else {
                continue;
            };
            let current_id = handle.status().entity_id().to_owned();
            if entity.metadata.entity_id != current_id {
                renamed.push((
                    entity.metadata.entity_id.clone(),
                    current_id.clone(),
                    entity
                        .availability
                        .iter()
                        .map(|item| item.topic.clone())
                        .collect::<Vec<_>>(),
                ));
                entity.metadata.entity_id = current_id;
            }
        }
    }
    if renamed.is_empty() {
        return;
    }
    let mut metadata = metadata
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (old_id, new_id, topics) in renamed {
        if let Some(value) = last_values.remove(&old_id) {
            last_values.insert(new_id.clone(), value);
        }
        for topic in topics {
            if let Some(value) = availability_status.remove(&(old_id.clone(), topic.clone())) {
                availability_status.insert((new_id.clone(), topic), value);
            }
        }
        if let Some(mut item) = metadata.remove(&old_id) {
            item.entity_id = new_id.clone();
            metadata.insert(new_id, item);
        }
    }
}

fn publish_discovered_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) {
    if let Some(climate) = entity.climate.as_ref() {
        let state = climate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Err(error) = publish_climate_state(runtime, entity, &state, available, context) {
            warn!(entity_id = %mqtt_entity_id(entity), error = %error, "failed to publish MQTT climate state");
        }
        return;
    }
    match entity.shared_entity.as_ref() {
        Some(SharedMqttEntity::Switch(handle)) => {
            let is_on = match state {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            let snapshot = SwitchSnapshot::new(is_on)
                .with_available(available)
                .with_assumed_state(entity.optimistic)
                .with_extra_attributes(attributes)
                .with_context(context);
            if let Err(error) = handle.publish_publication(snapshot.publication()) {
                warn!(entity_id = %mqtt_entity_id(entity), error = %error, "failed to publish MQTT switch snapshot through the shared lifecycle");
            }
        }
        Some(SharedMqttEntity::Button { handle, descriptor }) => {
            publish_button_state(
                runtime, entity, handle, descriptor, state, available, attributes, context,
            );
        }
        Some(SharedMqttEntity::Light { handle, .. })
        | Some(SharedMqttEntity::Cover { handle, .. })
        | Some(SharedMqttEntity::Fan { handle, .. }) => {
            match shared_entity_projection(entity, state, available, attributes, context) {
                Ok(Some((publication, _))) => {
                    if let Err(error) = handle.publish_publication(publication) {
                        warn!(entity_id = %mqtt_entity_id(entity), error = %error, "failed to publish MQTT snapshot through the shared lifecycle");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(entity_id = %mqtt_entity_id(entity), error = %error, "failed to project MQTT entity snapshot")
                }
            }
        }
        None if entity.metadata.platform == "switch" => {
            let is_on = match state {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            let snapshot = SwitchSnapshot::new(is_on)
                .with_available(available)
                .with_assumed_state(entity.optimistic)
                .with_extra_attributes(attributes)
                .with_context(context);
            let projected = snapshot.entity_snapshot();
            let state = if projected.available {
                projected.state.as_deref().unwrap_or("unknown")
            } else {
                "unavailable"
            };
            if let Err(error) = runtime.set_state(
                &entity.metadata.entity_id,
                state,
                projected.attributes,
                projected.context,
                false,
            ) {
                warn!(entity_id = entity.metadata.entity_id, error = %error, "failed to update runtime from MQTT switch");
            }
        }
        _ if available => {
            set_runtime_state(
                runtime,
                &entity.metadata.entity_id,
                state.unwrap_or("unknown"),
                attributes,
            );
        }
        _ => {
            set_runtime_state(
                runtime,
                &entity.metadata.entity_id,
                "unavailable",
                attributes,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_button_state(
    runtime: &Runtime,
    entity: &DiscoveredEntity,
    handle: &EntityHandle,
    descriptor: &ButtonDescriptor,
    state: Option<&str>,
    available: bool,
    attributes: Map<String, Value>,
    context: Context,
) {
    let current = if state.is_none() {
        runtime.get_state(&mqtt_entity_id(entity))
    } else {
        None
    };
    let remembered = entity
        .button_last_value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let restored_state = state
        .map(str::to_owned)
        .or_else(|| remembered.map(|value| value.state))
        .or_else(|| current.as_ref().map(|state| state.state.clone()));
    let publication_context = current
        .as_ref()
        .map(|state| state.context.clone())
        .unwrap_or(context);
    if let Some(state) = restored_state
        .as_ref()
        .filter(|state| state.as_str() != "unavailable")
    {
        *entity
            .button_last_value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LastValue {
            state: state.clone(),
            attributes: attributes.clone(),
            context: publication_context.clone(),
        });
    }
    let snapshot = if available {
        ButtonSnapshot::from_restored_state(restored_state.as_deref())
            .unwrap_or_else(ButtonSnapshot::unknown)
    } else {
        ButtonSnapshot::unavailable()
    }
    .with_context(publication_context);
    let publication = descriptor.publication(snapshot);
    if let Err(error) = handle.publish_publication(publication) {
        warn!(entity_id = %mqtt_entity_id(entity), error = %error, "failed to publish MQTT button snapshot through the shared lifecycle");
    }
}

fn discovery_parts(topic: &str, prefix: &str) -> Result<(String, String, String)> {
    let rest = topic
        .strip_prefix(&format!("{prefix}/"))
        .ok_or_else(|| Error::InvalidInput("discovery topic is outside discovery_prefix".into()))?;
    let mut parts: Vec<&str> = rest.split('/').collect();
    if parts.pop() != Some("config") {
        return Err(Error::InvalidInput(
            "MQTT discovery topic must end in /config".into(),
        ));
    }
    match parts.as_slice() {
        [platform, object_id] => Ok(((*platform).into(), String::new(), (*object_id).into())),
        [platform, node_id, object_id] => Ok(((*platform).into(), (*node_id).into(), (*object_id).into())),
        _ => Err(Error::InvalidInput("MQTT discovery topic must be prefix/platform/object/config or prefix/platform/node/object/config".into())),
    }
}

fn is_discovery_topic(topic: &str, prefix: &str) -> bool {
    topic.starts_with(&format!("{prefix}/")) && topic.ends_with("/config")
}

fn expand_topic(topic: &str, base: Option<&str>, field: &str) -> Result<String> {
    if topic == "~" {
        return base.map(str::to_owned).ok_or_else(|| {
            Error::InvalidInput(format!(
                "{field} uses ~ but discovery payload has no topic base"
            ))
        });
    }
    if let Some(suffix) = topic.strip_prefix("~/") {
        let base = base.ok_or_else(|| {
            Error::InvalidInput(format!(
                "{field} uses ~ but discovery payload has no topic base"
            ))
        })?;
        return Ok(format!("{base}/{suffix}"));
    }
    Ok(topic.to_owned())
}

fn validate_topic(topic: &str, field: &str) -> Result<()> {
    if topic.is_empty()
        || topic.len() > 1024
        || topic.contains(['+', '#', '\0'])
        || topic.starts_with('/')
        || topic.ends_with('/')
        || topic.split('/').any(str::is_empty)
    {
        return Err(Error::InvalidInput(format!(
            "invalid MQTT topic in {field}"
        )));
    }
    Ok(())
}

fn qos(value: u8) -> Result<QoS> {
    match value {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        _ => Err(Error::InvalidInput("MQTT QoS must be 0, 1, or 2".into())),
    }
}

fn qos_to_u8(value: QoS) -> u8 {
    match value {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

fn validate_filter(filter: &str) -> Result<()> {
    if filter.is_empty() || filter.len() > 1024 || filter.contains('\0') {
        return Err(Error::InvalidInput("invalid MQTT topic filter".into()));
    }
    let segments: Vec<_> = filter.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        if segment.contains('#') && (*segment != "#" || index + 1 != segments.len()) {
            return Err(Error::InvalidInput(
                "MQTT # wildcard must be the complete final topic-filter segment".into(),
            ));
        }
        if segment.contains('+') && *segment != "+" {
            return Err(Error::InvalidInput(
                "MQTT + wildcard must be a complete topic-filter segment".into(),
            ));
        }
    }
    Ok(())
}

fn dispatch_mqtt_message(runtime: &Runtime, topic: &str, payload: &[u8], qos: u8, retain: bool) {
    let mut data = Map::new();
    data.insert("topic".into(), Value::String(topic.to_owned()));
    data.insert(
        "payload".into(),
        std::str::from_utf8(payload)
            .map(|value| Value::String(value.to_owned()))
            .unwrap_or(Value::Null),
    );
    data.insert(
        "payload_bytes".into(),
        Value::Array(
            payload
                .iter()
                .map(|byte| Value::from(u64::from(*byte)))
                .collect(),
        ),
    );
    data.insert("qos".into(), Value::from(u64::from(qos)));
    data.insert("retain".into(), Value::Bool(retain));
    if let Err(error) = runtime.dispatch_signal("mqtt_message", data, Context::default()) {
        warn!(topic, error = %error, "failed to dispatch mqtt_message signal");
    }
}

fn slug(value: &str) -> String {
    let mut result = String::with_capacity(value.len().min(128));
    let mut underscore = false;
    for ch in value.chars().take(128) {
        if ch.is_ascii_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
            underscore = false;
        } else if !underscore && !result.is_empty() {
            result.push('_');
            underscore = true;
        }
    }
    while result.ends_with('_') {
        result.pop();
    }
    if result.is_empty() {
        "mqtt_entity".into()
    } else {
        result
    }
}

#[cfg(test)]
mod async_publish_tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hass_core::{MemoryStore, Runtime};
    use rumqttc::{AsyncClient, MqttOptions, QoS};
    use serde_json::Map;
    use tokio::sync::oneshot;

    use super::{
        AvailabilityMode, Command, CommandRequest, DiscoveredEntity, MqttConfig,
        MqttEntityMetadata, OptimisticState, PendingPublish, PublishCompletionContext,
        complete_expired_publishes, complete_publish, process_discovery, queue_publish_command,
    };
    use crate::Context;

    fn discovered_switch(entity_id: &str, discovery_topic: &str) -> DiscoveredEntity {
        DiscoveredEntity {
            metadata: MqttEntityMetadata {
                entity_id: entity_id.into(),
                platform: "switch".into(),
                unique_id: "test-switch".into(),
                discovery_topic: discovery_topic.into(),
                name: "Test switch".into(),
                device: None,
                entity_lifecycle_managed: false,
            },
            shared_entity: None,
            climate: None,
            switch_device_class: None,
            button_device_class: None,
            button_last_value: Arc::new(Mutex::new(None)),
            state_topic: None,
            extra_state_topics: BTreeMap::new(),
            extra_command_topics: BTreeMap::new(),
            json_attributes_topic: None,
            availability: Vec::new(),
            availability_mode: AvailabilityMode::All,
            command_topic: Some("test/switch/set".into()),
            payload_on: "ON".into(),
            payload_off: "OFF".into(),
            state_on: "ON".into(),
            state_off: "OFF".into(),
            value_template: None,
            attributes_template: None,
            command_template: None,
            optimistic: true,
            retain: false,
            qos: 1,
            max_payload_bytes: 65_536,
            attributes: Map::new(),
            platform_config: Map::new(),
            text_pattern: None,
        }
    }

    #[tokio::test]
    async fn late_ack_cannot_publish_into_a_replacement_discovery_entity() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = hass_registry::Registries::new(runtime.clone()).unwrap();
        let lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let mut handles = lifecycle
            .register_provider_batch(
                "retired",
                vec![
                    hass_switch::SwitchDescriptor::new("mqtt", "retired", "original")
                        .with_unique_id("retired-publisher")
                        .entity_spec(),
                ],
            )
            .unwrap();
        let retired_handle = handles.remove(0);
        retired_handle.release().unwrap();
        assert!(!retired_handle.status().is_active());

        const TOPIC: &str = "homeassistant/switch/reused/config";
        runtime
            .set_state(
                "switch.replacement",
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let entities = std::sync::RwLock::new(BTreeMap::from([(
            TOPIC.into(),
            discovered_switch("switch.replacement", TOPIC),
        )]));
        let availability_status = HashMap::new();
        let mut last_values = HashMap::new();
        let mut ready_continuations = VecDeque::new();
        let mut completion = PublishCompletionContext {
            runtime: &runtime,
            entities: &entities,
            connected: true,
            availability_status: &availability_status,
            last_values: &mut last_values,
            ready_continuations: &mut ready_continuations,
        };
        let (reply, result) = oneshot::channel();
        let mut pending = PendingPublish {
            qos: QoS::AtLeastOnce,
            reply: Some(reply),
            remaining_commands: VecDeque::new(),
            changed_states: Vec::new(),
            deadline: None,
            optimistic: Some(OptimisticState {
                discovery_topic: TOPIC.into(),
                state: Some("on".into()),
                attributes: Map::new(),
                optimistic_updates: Map::new(),
                context: Context::default(),
                switch_state: true,
                climate_state: None,
                entity_handle: Some(retired_handle),
            }),
        };
        complete_publish(&mut completion, &mut pending);
        assert_eq!(
            runtime.get_state("switch.replacement").unwrap().state,
            "off"
        );
        assert!(result.await.unwrap().unwrap().is_empty());
    }

    #[tokio::test]
    async fn callback_timeout_returns_normally_and_applies_optimistic_state() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .set_state(
                "switch.timeout",
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let (reply, result) = oneshot::channel();
        let now = tokio::time::Instant::now();
        let mut pending = VecDeque::from([PendingPublish {
            qos: QoS::AtLeastOnce,
            reply: Some(reply),
            remaining_commands: VecDeque::new(),
            changed_states: Vec::new(),
            optimistic: Some(OptimisticState {
                state: Some("on".into()),
                discovery_topic: "homeassistant/switch/timeout/config".into(),
                attributes: Map::new(),
                optimistic_updates: Map::new(),
                context: Context::default(),
                switch_state: true,
                climate_state: None,
                entity_handle: None,
            }),
            deadline: Some(now - Duration::from_secs(1)),
        }]);
        let mut awaiting_ack = HashMap::new();
        let mut last_values = HashMap::new();
        let entities = std::sync::RwLock::new(BTreeMap::from([(
            "homeassistant/switch/timeout/config".into(),
            discovered_switch("switch.timeout", "homeassistant/switch/timeout/config"),
        )]));

        let availability_status = HashMap::new();
        let mut ready_continuations = VecDeque::new();
        let mut completion = PublishCompletionContext {
            runtime: &runtime,
            entities: &entities,
            connected: true,
            availability_status: &availability_status,
            last_values: &mut last_values,
            ready_continuations: &mut ready_continuations,
        };
        complete_expired_publishes(&mut completion, &mut pending, &mut awaiting_ack, now);

        let changed_states = result.await.unwrap().unwrap();
        assert_eq!(changed_states.len(), 1);
        assert_eq!(changed_states[0].entity_id, "switch.timeout");
        assert_eq!(changed_states[0].state, "on");
        assert_eq!(runtime.get_state("switch.timeout").unwrap().state, "on");
        assert!(pending[0].reply.is_none());
        assert!(pending[0].optimistic.is_none());
        assert!(pending[0].deadline.is_none());
    }

    #[tokio::test]
    async fn callback_timeout_advances_one_continuation_and_late_ack_does_not_repeat_it() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let (reply, _result) = oneshot::channel();
        let now = tokio::time::Instant::now();
        let command = |topic: &str| Command {
            topic: topic.into(),
            payload: b"value".to_vec(),
            publish: true,
            qos: QoS::AtLeastOnce,
            retain: false,
            optimistic: None,
            entity_handle: None,
        };
        let mut pending = VecDeque::from([PendingPublish {
            qos: QoS::AtLeastOnce,
            reply: Some(reply),
            remaining_commands: VecDeque::from([command("lab/second"), command("lab/third")]),
            changed_states: Vec::new(),
            optimistic: None,
            deadline: Some(now - Duration::from_secs(1)),
        }]);
        let mut awaiting_ack = HashMap::new();
        let mut last_values = HashMap::new();
        let entities = std::sync::RwLock::new(BTreeMap::new());
        let availability_status = HashMap::new();
        let mut ready_continuations = VecDeque::new();
        {
            let mut completion = PublishCompletionContext {
                runtime: &runtime,
                entities: &entities,
                connected: true,
                availability_status: &availability_status,
                last_values: &mut last_values,
                ready_continuations: &mut ready_continuations,
            };
            complete_expired_publishes(&mut completion, &mut pending, &mut awaiting_ack, now);
        }
        assert_eq!(ready_continuations.len(), 1);
        assert_eq!(ready_continuations[0].command.topic, "lab/second");
        assert_eq!(ready_continuations[0].remaining_commands.len(), 1);
        assert!(pending[0].reply.is_none());

        // A late PUBACK completes only the tombstone; it cannot schedule the
        // continuation a second time after timeout already advanced the chain.
        {
            let mut completion = PublishCompletionContext {
                runtime: &runtime,
                entities: &entities,
                connected: true,
                availability_status: &availability_status,
                last_values: &mut last_values,
                ready_continuations: &mut ready_continuations,
            };
            complete_publish(&mut completion, &mut pending[0]);
        }
        assert_eq!(ready_continuations.len(), 1);
    }

    #[tokio::test]
    async fn attribute_only_optimism_uses_the_state_cached_at_completion() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .set_state(
                "fan.timeout",
                "off",
                Map::from_iter([("percentage".into(), serde_json::json!(50))]),
                Context::default(),
                false,
            )
            .unwrap();
        let mut entity = discovered_switch("fan.timeout", "homeassistant/fan/timeout/config");
        entity.metadata.platform = "fan".into();
        let entities = std::sync::RwLock::new(BTreeMap::from([(
            entity.metadata.discovery_topic.clone(),
            entity,
        )]));
        let update = OptimisticState {
            state: None,
            discovery_topic: "homeassistant/fan/timeout/config".into(),
            attributes: Map::new(),
            optimistic_updates: Map::from_iter([("percentage".into(), serde_json::json!(0))]),
            context: Context::default(),
            switch_state: false,
            climate_state: None,
            entity_handle: None,
        };
        // This represents another admitted effect changing the entity after
        // the command batch was preflighted but before its attribute update
        // completes.
        runtime
            .set_state(
                "fan.timeout",
                "on",
                Map::from_iter([("percentage".into(), serde_json::json!(50))]),
                Context::default(),
                false,
            )
            .unwrap();
        let (reply, result) = oneshot::channel();
        let mut pending = PendingPublish {
            qos: QoS::AtLeastOnce,
            reply: Some(reply),
            remaining_commands: VecDeque::new(),
            changed_states: Vec::new(),
            optimistic: Some(update),
            deadline: None,
        };
        let availability_status = HashMap::new();
        let mut last_values = HashMap::new();
        let mut ready_continuations = VecDeque::new();
        let mut completion = PublishCompletionContext {
            runtime: &runtime,
            entities: &entities,
            connected: true,
            availability_status: &availability_status,
            last_values: &mut last_values,
            ready_continuations: &mut ready_continuations,
        };

        complete_publish(&mut completion, &mut pending);

        let state = runtime.get_state("fan.timeout").unwrap();
        assert_eq!(state.state, "on");
        assert_eq!(state.attributes["percentage"], 0);
        assert_eq!(result.await.unwrap().unwrap().len(), 1);
        assert!(ready_continuations.is_empty());
    }

    #[test]
    fn failed_or_unsupported_discovery_does_not_record_component_receipts() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config: MqttConfig = serde_yaml::from_str("host: 127.0.0.1\n").unwrap();
        let entities = Arc::new(std::sync::RwLock::new(BTreeMap::new()));
        let metadata = Arc::new(std::sync::RwLock::new(BTreeMap::new()));
        let mut availability_status = HashMap::new();

        let unsupported = process_discovery(
            &runtime,
            &config,
            "homeassistant/scene/lab/unsupported/config",
            b"{}",
            &entities,
            &metadata,
            &None,
            &mut availability_status,
        )
        .unwrap();
        assert!(!unsupported);
        assert!(runtime.loaded_components().components().is_empty());

        assert!(
            process_discovery(
                &runtime,
                &config,
                "homeassistant/fan/lab/malformed/config",
                b"{",
                &entities,
                &metadata,
                &None,
                &mut availability_status,
            )
            .is_err()
        );
        assert!(runtime.loaded_components().components().is_empty());
    }

    #[tokio::test]
    async fn rumqttc_queue_error_returns_error_without_optimistic_state() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .set_state(
                "switch.queue_error",
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let options = MqttOptions::new("mqtt-queue-test", "127.0.0.1", 1883);
        let (client, _event_loop) = AsyncClient::new(options, 1);
        client
            .try_publish(
                "lab/occupied",
                QoS::AtLeastOnce,
                false,
                b"occupied".to_vec(),
            )
            .unwrap();

        let (reply, result) = oneshot::channel();
        let mut pending = VecDeque::new();
        let entities = std::sync::RwLock::new(BTreeMap::new());
        queue_publish_command(
            &client,
            CommandRequest {
                command: Command {
                    topic: "lab/command".into(),
                    payload: b"ON".to_vec(),
                    publish: true,
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    optimistic: Some(OptimisticState {
                        discovery_topic: "homeassistant/switch/queue_error/config".into(),
                        state: Some("on".into()),
                        attributes: Map::new(),
                        optimistic_updates: Map::new(),
                        context: Context::default(),
                        switch_state: true,
                        climate_state: None,
                        entity_handle: None,
                    }),
                    entity_handle: None,
                },
                remaining_commands: VecDeque::from([Command {
                    topic: "lab/command/continuation".into(),
                    payload: b"OFF".to_vec(),
                    publish: true,
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    optimistic: None,
                    entity_handle: None,
                }]),
                changed_states: Vec::new(),
                reply,
            },
            &entities,
            &mut pending,
        );

        assert!(result.await.unwrap().is_err());
        assert!(pending.is_empty());
        assert_eq!(
            runtime.get_state("switch.queue_error").unwrap().state,
            "off"
        );
    }
}
