//! Native MQTT integration for Home Assistant style discovery and entities.
//!
//! Discovery covers the supported MQTT entity platforms. Reported state changes
//! arrive only from MQTT state/availability messages. Commands use a bounded
//! broker queue; optimism must be explicitly enabled in configuration.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult};
use hass_template::TemplateEngine;
use regex::Regex;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const CLIENT_QUEUE_CAPACITY: usize = 128;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
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
/// The root integration can use this readback to create or reconcile native
/// entity/device registry records without reaching into this crate's state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MqttEntityMetadata {
    pub entity_id: String,
    pub platform: String,
    pub unique_id: String,
    pub discovery_topic: String,
    pub name: String,
    pub device: Option<Value>,
}

/// Running MQTT connection and entity/service bridge.
pub struct MqttIntegration {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
    subscription_tx: mpsc::Sender<SubscriptionRequest>,
    service_owner: Option<String>,
    runtime: Runtime,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
}

impl MqttIntegration {
    /// Connect to the broker and start processing incoming and outgoing MQTT.
    ///
    /// Startup waits at most five seconds for the first successful CONNACK. A
    /// connection that cannot be established is returned as an error instead
    /// of leaving a detached background retry loop behind.
    pub async fn start(runtime: Runtime, config: MqttConfig) -> Result<Self> {
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
        let (subscription_tx, subscription_rx) = mpsc::channel(32);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let entities = Arc::new(RwLock::new(BTreeMap::new()));
        let metadata = Arc::new(RwLock::new(BTreeMap::new()));
        let additional_subscriptions = Arc::new(RwLock::new(BTreeMap::new()));
        let templates = TemplateEngine::new(runtime.clone());

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
            stop_rx,
            ready_tx,
            task_entities,
            task_metadata,
            Arc::clone(&additional_subscriptions),
        ));

        match tokio::time::timeout(STARTUP_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                let _ = task.await;
                return Err(Error::Service(
                    "mqtt connection task ended before connecting".into(),
                ));
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = task.await;
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
        ) {
            if let Err(cleanup_error) = runtime.unregister_entity_services(&service_owner) {
                warn!(owner = %service_owner, error = %cleanup_error, "failed to clean up partial MQTT service registration");
            }
            let _ = stop_tx.send(());
            let _ = task.await;
            return Err(error);
        }

        Ok(Self {
            shutdown: Some(stop_tx),
            task: Some(task),
            subscription_tx,
            service_owner: Some(service_owner),
            runtime,
            entities,
            metadata,
        })
    }

    /// Return currently discovered entity identities in stable order.
    pub fn entity_metadata(&self) -> Vec<MqttEntityMetadata> {
        self.metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Rebind an active entity to its persistent registry-assigned entity ID.
    ///
    /// This only changes the entity's internal identity and current runtime
    /// state. It never publishes an MQTT command or changes any topic.
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
            .find(|(_, entity)| entity.metadata.entity_id == old_entity_id)
            .map(|(topic, _)| topic.clone())
        else {
            return Err(Error::NotFound(format!(
                "MQTT entity {old_entity_id} is no longer active"
            )));
        };
        if entities
            .values()
            .any(|other| other.metadata.entity_id == new_entity_id)
        {
            return Err(Error::InvalidInput(format!(
                "MQTT entity ID {new_entity_id} is already in use"
            )));
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
        let entity = entities
            .get_mut(&discovery_topic)
            .expect("discovery topic was found above");
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
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let mut first_error = None;
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => first_error = Some(error),
                Err(error) => {
                    first_error = Some(Error::Service(format!(
                        "mqtt event loop task failed: {error}"
                    )));
                }
            }
        }
        if let Some(owner) = self.service_owner.take()
            && let Err(error) = self.runtime.unregister_entity_services(&owner)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for MqttIntegration {
    fn drop(&mut self) {
        // Closing the oneshot also wakes the task if callers drop the owner
        // without explicitly awaiting shutdown. The explicit method remains
        // necessary when the caller needs to observe completion/errors.
        self.shutdown.take();
        if let Some(owner) = self.service_owner.take()
            && let Err(error) = self.runtime.unregister_entity_services(&owner)
        {
            warn!(owner, %error, "failed to unregister MQTT entity services during drop");
        }
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

#[derive(Debug)]
struct Command {
    topic: String,
    payload: Vec<u8>,
    qos: QoS,
    retain: bool,
    optimistic: Option<(String, String, Map<String, Value>, Context)>,
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
}

#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    runtime: Runtime,
    config: MqttConfig,
    templates: TemplateEngine,
    client: AsyncClient,
    mut event_loop: rumqttc::EventLoop,
    mut command_rx: mpsc::Receiver<Command>,
    mut subscription_rx: mpsc::Receiver<SubscriptionRequest>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: oneshot::Sender<()>,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
    additional_subscriptions: Arc<RwLock<BTreeMap<String, u8>>>,
) -> Result<()> {
    let mut connected = false;
    let mut ready_tx = Some(ready_tx);
    let mut subscribed = BTreeMap::new();
    let mut availability_status: HashMap<(String, String), bool> = HashMap::new();
    let mut last_values: HashMap<String, LastValue> = HashMap::new();

    loop {
        enum Next {
            Stop,
            Command(Option<Command>),
            Subscription(Option<SubscriptionRequest>),
            Poll(std::result::Result<Event, rumqttc::ConnectionError>),
        }
        let next = tokio::select! {
            _ = &mut stop_rx => Next::Stop,
            command = command_rx.recv() => Next::Command(command),
            subscription = subscription_rx.recv() => Next::Subscription(subscription),
            event = event_loop.poll() => Next::Poll(event),
        };

        match next {
            Next::Stop => {
                graceful_disconnect(&client, &mut event_loop).await;
                return Ok(());
            }
            Next::Command(Some(command)) => {
                let published = client
                    .publish(command.topic, command.qos, command.retain, command.payload)
                    .await;
                if let Err(error) = published {
                    warn!(error = %error, "failed to queue MQTT command for publication");
                    continue;
                }
                if let Some((entity_id, state, attributes, context)) = command.optimistic {
                    if let Err(error) =
                        runtime.set_state(&entity_id, &state, attributes.clone(), context, false)
                    {
                        warn!(entity_id, error = %error, "failed to apply explicitly optimistic MQTT state");
                    } else {
                        last_values.insert(entity_id, LastValue { state, attributes });
                    }
                }
            }
            Next::Command(None) => {
                graceful_disconnect(&client, &mut event_loop).await;
                return Ok(());
            }
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
            Next::Subscription(None) => {
                graceful_disconnect(&client, &mut event_loop).await;
                return Ok(());
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

async fn graceful_disconnect(client: &AsyncClient, event_loop: &mut rumqttc::EventLoop) {
    if client.disconnect().await.is_ok() {
        // Poll once so rumqttc can write the queued DISCONNECT packet. Some
        // brokers close immediately after receiving it, so this has a bound.
        let _ = tokio::time::timeout(Duration::from_secs(1), event_loop.poll()).await;
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
                templates,
                availability.value_template.as_deref(),
                payload_text,
                &entity.metadata.entity_id,
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
                        })
                })
                .unwrap_or_else(|| LastValue {
                    state: "unknown".into(),
                    attributes: entity.attributes.clone(),
                });
            match update_reported_state(&entity, topic, payload_text, templates, current) {
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
                set_runtime_state(
                    runtime,
                    &entity.metadata.entity_id,
                    &next.state,
                    next.attributes,
                );
            }
        }
    }
}

fn process_discovery(
    runtime: &Runtime,
    config: &MqttConfig,
    discovery_topic: &str,
    payload: &[u8],
    entities: &Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    metadata: &Arc<RwLock<BTreeMap<String, MqttEntityMetadata>>>,
    availability_status: &mut HashMap<(String, String), bool>,
) -> Result<bool> {
    if payload.len() > config.max_payload_bytes {
        return Err(Error::InvalidInput(
            "discovery payload exceeds configured limit".into(),
        ));
    }
    if payload.is_empty() {
        let removed = entities
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(discovery_topic);
        if let Some(entity) = removed {
            for item in &entity.availability {
                availability_status
                    .remove(&(entity.metadata.entity_id.clone(), item.topic.clone()));
            }
            let _ = runtime.remove_state(&entity.metadata.entity_id, Context::default());
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
            | "number"
            | "select"
            | "text"
    ) {
        return Ok(false);
    }
    let parsed = parse_discovered_entity(
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
        let occupied_by_runtime = runtime.get_state(&entity_id).is_some();
        if occupied_by_runtime
            || entity_map
                .values()
                .any(|known| known.metadata.entity_id == entity_id)
        {
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

    let is_new = prior.is_none();
    if let Some(prior) = prior {
        for item in &prior.availability {
            availability_status.remove(&(prior.metadata.entity_id.clone(), item.topic.clone()));
        }
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

    if is_new {
        let initial = if entity.availability.is_empty() {
            "unknown"
        } else {
            "unavailable"
        };
        set_runtime_state(
            runtime,
            &entity.metadata.entity_id,
            initial,
            entity.attributes.clone(),
        );
    } else if !entity.availability.is_empty() {
        set_runtime_state(
            runtime,
            &entity.metadata.entity_id,
            "unavailable",
            runtime
                .get_state(&entity.metadata.entity_id)
                .map(|state| state.attributes)
                .unwrap_or_else(|| entity.attributes.clone()),
        );
    }
    Ok(true)
}

fn parse_discovered_entity(
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
    let state_topic = topic("state_topic")?;
    let command_topic = topic("command_topic")?;
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
            attributes.insert(key.to_owned(), value.clone());
        }
    }
    let mut platform_config = object.clone();
    platform_config.insert("schema".into(), Value::String(schema.clone()));
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
    Ok(DiscoveredEntity {
        metadata: MqttEntityMetadata {
            entity_id,
            platform: platform.to_owned(),
            unique_id,
            discovery_topic: discovery_topic.to_owned(),
            name,
            device,
        },
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
    })
}

fn register_services(
    runtime: &Runtime,
    owner: &str,
    command_tx: mpsc::Sender<Command>,
    entities: Arc<RwLock<BTreeMap<String, DiscoveredEntity>>>,
    templates: TemplateEngine,
) -> Result<()> {
    let serial = Arc::new(Mutex::new(()));
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
        ("cover", "set_cover_position"),
        ("cover", "open_cover_tilt"),
        ("cover", "close_cover_tilt"),
        ("cover", "stop_cover_tilt"),
        ("cover", "set_cover_tilt_position"),
        ("fan", "turn_on"),
        ("fan", "turn_off"),
        ("fan", "set_percentage"),
        ("fan", "set_preset_mode"),
        ("fan", "oscillate"),
        ("fan", "set_direction"),
        ("button", "press"),
        ("number", "set_value"),
        ("select", "select_option"),
        ("text", "set_value"),
    ] {
        let tx = command_tx.clone();
        let entities = Arc::clone(&entities);
        let templates = templates.clone();
        let serial = Arc::clone(&serial);
        let matcher_entities = Arc::clone(&entities);
        let matcher_domain = domain.to_owned();
        runtime.register_entity_service(
            owner,
            domain,
            service,
            json!({"description": "Publish an MQTT command for discovered entities"}),
            Arc::new(move |entity_id| {
                matcher_entities
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .any(|entity| {
                        entity.metadata.platform == matcher_domain
                            && entity.metadata.entity_id == entity_id
                    })
            }),
            Arc::new(move |runtime, call| {
                enqueue_service_commands(runtime, call, &tx, &entities, &templates, &serial)
            }),
        )?;
    }
    Ok(())
}

fn enqueue_service_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    tx: &mpsc::Sender<Command>,
    entities: &RwLock<BTreeMap<String, DiscoveredEntity>>,
    templates: &TemplateEngine,
    serial: &Mutex<()>,
) -> Result<ServiceResult> {
    let _guard = serial
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids = requested_entity_ids(call)?;
    if ids.len() > MAX_TARGETS_PER_SERVICE {
        return Err(Error::Service(format!(
            "MQTT service target limit is {MAX_TARGETS_PER_SERVICE}"
        )));
    }
    let discovered = entities
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut pending = Vec::with_capacity(ids.len());
    for entity_id in ids {
        let entity = discovered
            .values()
            .find(|entity| entity.metadata.entity_id == entity_id)
            .ok_or_else(|| Error::Service(format!("unknown MQTT entity {entity_id}")))?;
        if entity.metadata.platform != call.domain {
            return Err(Error::Service(format!(
                "{} is not a {} entity",
                entity_id, call.domain
            )));
        }
        pending.extend(build_entity_commands(runtime, call, entity, templates)?);
    }
    if tx.capacity() < pending.len() {
        return Err(Error::Service("MQTT command queue is full".into()));
    }
    let queued = pending.len();
    let changed_states = Vec::new();
    for command in pending {
        tx.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                Error::Service("MQTT command queue is full".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                Error::Service("MQTT connection is shutting down".into())
            }
        })?;
    }
    // Optimistic state is applied by the MQTT worker only after AsyncClient
    // accepts the command into its publish queue.
    Ok(ServiceResult {
        changed_states,
        response: Some(json!({"queued": queued})),
        context: call.context.clone(),
    })
}

fn build_entity_commands(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
) -> Result<Vec<Command>> {
    let platform = entity.metadata.platform.as_str();
    let service = call.service.as_str();
    let current = runtime.get_state(&entity.metadata.entity_id);
    let current_state = current
        .as_ref()
        .map(|state| state.state.as_str())
        .unwrap_or("unknown");
    let mut output = Vec::new();
    match (platform, service) {
        ("switch" | "light", "turn_on" | "turn_off" | "toggle") => {
            if platform == "light"
                && config_string(&entity.platform_config, "schema", "basic") == "json"
            {
                let requested_state = requested_on_off(current_state, service);
                let (payload, attrs) = json_light_command(entity, call, requested_state)?;
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
                let requested_state = requested_on_off(current_state, service);
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
                let payload = render_command(call, entity, templates, "command_template", raw)?;
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
            check_service_options(call, &[])?;
            let (payload_key, fallback, state) = match service {
                "open_cover" => ("payload_open", "OPEN", Some("open")),
                "close_cover" => ("payload_close", "CLOSE", Some("closed")),
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
            check_service_options(call, &["position"])?;
            let percent = numeric_arg(call, "position", 0.0, 100.0)?;
            let closed = config_number(&entity.platform_config, "position_closed", 0.0);
            let open = config_number(&entity.platform_config, "position_open", 100.0);
            let ranged = (closed + (open - closed) * percent / 100.0).round();
            let payload = render_typed_command(
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
            let setting_position = service == "set_cover_tilt_position";
            if setting_position {
                check_service_options(call, &["tilt_position"])?;
            } else {
                check_service_options(call, &[])?;
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
                    let percent = numeric_arg(call, "tilt_position", 0.0, 100.0)?;
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
                        runtime, call, entity, templates, value,
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
                runtime, call, entity, templates, value,
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
            check_service_options(call, &[])?;
            let payload = render_command(
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
            let payload = render_command(call, entity, templates, "command_template", &raw)?;
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
            let payload = render_command(call, entity, templates, "command_template", option)?;
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
            let payload = render_command(call, entity, templates, "command_template", text)?;
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
    let variables = command_variables(call, value, &entity.metadata.entity_id);
    templates
        .render(template, &variables)
        .map_err(|error| Error::Service(format!("MQTT command template failed: {error}")))
}

fn render_typed_command(
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
    let mut variables = command_variables(call, value, &entity.metadata.entity_id);
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
        let (state, mut attributes) = runtime
            .get_state(&entity.metadata.entity_id)
            .map(|state| (state.state, state.attributes))
            .unwrap_or_else(|| ("unknown".into(), entity.attributes.clone()));
        attributes.extend(optimistic_updates);
        (
            entity.metadata.entity_id.clone(),
            optimistic_state.unwrap_or(state),
            attributes,
            call.context.clone(),
        )
    });
    Ok(Command {
        topic,
        payload: payload.into_bytes(),
        qos: qos(entity.qos)?,
        retain: entity.retain,
        optimistic,
    })
}

fn fan_percentage_command(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &DiscoveredEntity,
    templates: &TemplateEngine,
    value: &Value,
) -> Result<Command> {
    let percent =
        number_value(value, "fan percentage").map_err(|error| Error::Service(error.to_string()))?;
    if !(1.0..=100.0).contains(&percent) {
        return Err(Error::Service(
            "fan percentage must be between 1 and 100".into(),
        ));
    }
    let min = config_number(&entity.platform_config, "speed_range_min", 1.0);
    let max = config_number(&entity.platform_config, "speed_range_max", 100.0);
    let ranged = (min + (max - min) * percent / 100.0).ceil();
    let payload = render_typed_command(
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

fn command_variables(call: &ServiceCall, value: &str, entity_id: &str) -> Map<String, Value> {
    let mut variables = Map::new();
    variables.insert("value".into(), Value::String(value.to_owned()));
    variables.insert("entity_id".into(), Value::String(entity_id.to_owned()));
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
    templates: &TemplateEngine,
    template: Option<&str>,
    payload: &str,
    entity_id: &str,
) -> Result<Value> {
    let Some(template) = template else {
        return Ok(Value::String(payload.to_owned()));
    };
    let variables = value_variables(payload, entity_id);
    templates
        .render_value(template, &variables)
        .map_err(|error| Error::InvalidInput(format!("MQTT value template failed: {error}")))
}

fn render_string(
    templates: &TemplateEngine,
    template: Option<&str>,
    payload: &str,
    entity_id: &str,
) -> Result<String> {
    let Some(template) = template else {
        return Ok(payload.to_owned());
    };
    templates
        .render(template, &value_variables(payload, entity_id))
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
    let mut variables = value_variables(payload, &entity.metadata.entity_id);
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

fn update_reported_state(
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
        let value = render_value(templates, template, payload, &entity.metadata.entity_id)?;
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
        let value = render_topic_value(entity, topic_key, payload, templates)?;
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
                    if !(min..=max).contains(&speed) {
                        return Err(Error::InvalidInput(
                            "MQTT fan percentage is outside its configured speed range".into(),
                        ));
                    }
                    current.attributes.insert(
                        "percentage".into(),
                        json!(
                            ((speed - min) / (max - min) * 100.0)
                                .round()
                                .clamp(0.0, 100.0) as u8
                        ),
                    );
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
            set_runtime_state(
                runtime,
                &entity.metadata.entity_id,
                &last.state,
                last.attributes.clone(),
            );
        } else {
            set_runtime_state(
                runtime,
                &entity.metadata.entity_id,
                "unknown",
                entity.attributes.clone(),
            );
        }
    } else {
        let attributes = last_values
            .get(&entity.metadata.entity_id)
            .map(|value| value.attributes.clone())
            .unwrap_or_else(|| entity.attributes.clone());
        set_runtime_state(
            runtime,
            &entity.metadata.entity_id,
            "unavailable",
            attributes,
        );
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
