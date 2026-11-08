use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use hass_core::{Context, Event, MemoryStore, Runtime, ServiceCall};
use hass_entity::EntityLifecycle;
use hass_mqtt::{MqttConfig, MqttIntegration};
use hass_registry::{EntityUpdate, Registries};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug)]
enum ServerCommand {
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
    },
    DropConnection,
    HoldAcknowledgments(bool),
    Acknowledge(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrokerEvent {
    Connected,
    Subscribed {
        topic: String,
        qos: u8,
    },
    Published {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        packet_id: Option<u16>,
    },
    AwaitingAcknowledgment {
        topic: String,
        packet_id: u16,
        qos: u8,
    },
    Disconnected,
}

struct LocalBroker {
    address: String,
    commands: mpsc::UnboundedSender<ServerCommand>,
    events: mpsc::UnboundedReceiver<BrokerEvent>,
    task: JoinHandle<()>,
}

impl LocalBroker {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut packet_id = 1u16;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if serve_connection(stream, &mut command_rx, &event_tx, &mut packet_id)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            address,
            commands: command_tx,
            events: event_rx,
            task,
        }
    }

    async fn publish(&self, topic: &str, payload: impl Into<Vec<u8>>, retain: bool) {
        self.commands
            .send(ServerCommand::Publish {
                topic: topic.into(),
                payload: payload.into(),
                qos: 1,
                retain,
            })
            .unwrap();
    }

    async fn drop_connection(&self) {
        self.commands.send(ServerCommand::DropConnection).unwrap();
    }

    fn hold_acknowledgments(&self, hold: bool) {
        self.commands
            .send(ServerCommand::HoldAcknowledgments(hold))
            .unwrap();
    }

    fn acknowledge(&self, packet_id: u16) {
        self.commands
            .send(ServerCommand::Acknowledge(packet_id))
            .unwrap();
    }

    async fn wait_for_subscriptions(&mut self, expected: &[(&str, u8)]) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = HashSet::new();
        loop {
            let event = match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => panic!("broker task ended; subscriptions seen: {seen:?}"),
                Err(_) => panic!("timed out waiting for subscriptions; seen: {seen:?}"),
            };
            if let BrokerEvent::Subscribed { topic, qos } = event {
                seen.insert((topic, qos));
                if expected
                    .iter()
                    .all(|(topic, qos)| seen.contains(&(topic.to_string(), *qos)))
                {
                    return;
                }
            }
        }
    }

    async fn wait_for(&mut self, mut predicate: impl FnMut(&BrokerEvent) -> bool) -> BrokerEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            let event = match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => panic!("broker task ended; saw {seen:?}"),
                Err(_) => panic!("timed out waiting for broker packet; saw {seen:?}"),
            };
            if predicate(&event) {
                return event;
            }
            seen.push(event);
        }
    }
}

impl Drop for LocalBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(
    stream: TcpStream,
    commands: &mut mpsc::UnboundedReceiver<ServerCommand>,
    events: &mpsc::UnboundedSender<BrokerEvent>,
    packet_id: &mut u16,
) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_packet(&mut reader).await {
                Ok(packet) => {
                    if packet_tx.send(packet).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    let mut hold_acknowledgments = false;
    let mut pending_acknowledgments = HashMap::new();

    loop {
        tokio::select! {
            packet = packet_rx.recv() => {
                let Some((header, body)) = packet else { break };
                match header >> 4 {
                    1 => {
                        events.send(BrokerEvent::Connected).ok();
                        write_packet(&mut writer, 0x20, &[0, 0]).await?;
                    }
                    8 => {
                        if body.len() < 5 { continue; }
                        let id = u16::from_be_bytes([body[0], body[1]]);
                        let mut index = 2;
                        let mut response = vec![body[0], body[1]];
                        while index + 2 <= body.len() {
                            let length = u16::from_be_bytes([body[index], body[index + 1]]) as usize;
                            index += 2;
                            if index + length + 1 > body.len() { break; }
                            let topic = String::from_utf8_lossy(&body[index..index + length]).to_string();
                            index += length;
                            let qos = body[index];
                            index += 1;
                            events.send(BrokerEvent::Subscribed { topic, qos }).ok();
                            response.push(qos.min(2));
                        }
                        if response.len() > 2 {
                            write_packet(&mut writer, 0x90, &response).await?;
                        }
                        let _ = id;
                    }
                    3 => {
                        if body.len() < 2 { continue; }
                        let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                        if body.len() < 2 + topic_len { continue; }
                        let topic = String::from_utf8_lossy(&body[2..2 + topic_len]).to_string();
                        let qos = (header >> 1) & 0x03;
                        let retain = (header & 0x01) != 0;
                        let mut payload_offset = 2 + topic_len;
                        let id = if qos > 0 && body.len() >= payload_offset + 2 {
                            let value = u16::from_be_bytes([body[payload_offset], body[payload_offset + 1]]);
                            payload_offset += 2;
                            Some(value)
                        } else {
                            None
                        };
                        events.send(BrokerEvent::Published {
                            topic: topic.clone(),
                            payload: body[payload_offset..].to_vec(),
                            qos,
                            retain,
                            packet_id: id,
                        }).ok();
                        if let Some(id) = id {
                            match qos {
                                1 if hold_acknowledgments => {
                                    pending_acknowledgments.insert(id, (1, topic.clone()));
                                    events.send(BrokerEvent::AwaitingAcknowledgment {
                                        topic,
                                        packet_id: id,
                                        qos: 1,
                                    }).ok();
                                }
                                1 => write_packet(&mut writer, 0x40, &id.to_be_bytes()).await?,
                                2 => {
                                    pending_acknowledgments.insert(id, (2, topic));
                                    write_packet(&mut writer, 0x50, &id.to_be_bytes()).await?;
                                }
                                _ => {}
                            }
                        }
                    }
                    6 => {
                        if body.len() < 2 { continue; }
                        let id = u16::from_be_bytes([body[0], body[1]]);
                        if let Some((2, topic)) = pending_acknowledgments.get(&id) {
                            if hold_acknowledgments {
                                events.send(BrokerEvent::AwaitingAcknowledgment {
                                    topic: topic.clone(),
                                    packet_id: id,
                                    qos: 2,
                                }).ok();
                            } else {
                                pending_acknowledgments.remove(&id);
                                write_packet(&mut writer, 0x70, &id.to_be_bytes()).await?;
                            }
                        }
                    }
                    12 => write_packet(&mut writer, 0xD0, &[]).await?,
                    14 => {
                        events.send(BrokerEvent::Disconnected).ok();
                        break;
                    }
                    _ => {}
                }
            }
            command = commands.recv() => {
                match command {
                    Some(ServerCommand::Publish { topic, payload, qos, retain }) => {
                        let id = *packet_id;
                        *packet_id = packet_id.wrapping_add(1).max(1);
                        let mut body = Vec::new();
                        body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
                        body.extend_from_slice(topic.as_bytes());
                        if qos > 0 { body.extend_from_slice(&id.to_be_bytes()); }
                        body.extend_from_slice(&payload);
                        let header = 0x30 | ((qos.min(2)) << 1) | u8::from(retain);
                        write_packet(&mut writer, header, &body).await?;
                    }
                    Some(ServerCommand::DropConnection) => {
                        events.send(BrokerEvent::Disconnected).ok();
                        reader_task.abort();
                        return Ok(());
                    }
                    Some(ServerCommand::HoldAcknowledgments(hold)) => {
                        hold_acknowledgments = hold;
                    }
                    Some(ServerCommand::Acknowledge(packet_id)) => {
                        if let Some((qos, _)) = pending_acknowledgments.remove(&packet_id) {
                            let header = if qos == 1 { 0x40 } else { 0x70 };
                            write_packet(&mut writer, header, &packet_id.to_be_bytes()).await?;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    reader_task.abort();
    Ok(())
}

async fn read_packet(reader: &mut (impl AsyncReadExt + Unpin)) -> io::Result<(u8, Vec<u8>)> {
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
                "invalid remaining length",
            ));
        }
    }
    let mut body = vec![0; remaining];
    reader.read_exact(&mut body).await?;
    Ok((first[0], body))
}

async fn write_packet(
    writer: &mut (impl AsyncWriteExt + Unpin),
    header: u8,
    body: &[u8],
) -> io::Result<()> {
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

fn runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).unwrap()
}

async fn wait_state(runtime: &Runtime, entity_id: &str, expected: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .get_state(entity_id)
                .is_some_and(|state| state.state == expected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{entity_id} did not reach {expected:?}: {:?}",
            runtime.get_state(entity_id)
        )
    });
}

async fn wait_removed(runtime: &Runtime, entity_id: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state(entity_id).is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{entity_id} was not removed"));
}

async fn wait_metadata_entity_id(
    integration: &MqttIntegration,
    unique_id: &str,
    expected_entity_id: &str,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if integration.entity_metadata().iter().any(|entity| {
                entity.unique_id == unique_id && entity.entity_id == expected_entity_id
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "MQTT metadata for {unique_id} did not reach {expected_entity_id}: {:?}",
            integration.entity_metadata()
        )
    });
}

async fn wait_state_attribute(
    runtime: &Runtime,
    entity_id: &str,
    expected_state: &str,
    attribute: &str,
    expected_value: &Value,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state(entity_id).is_some_and(|state| {
                state.state == expected_state
                    && state.attributes.get(attribute) == Some(expected_value)
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{entity_id} did not reach {expected_state:?} with {attribute}={expected_value}: {:?}",
            runtime.get_state(entity_id)
        )
    });
}

async fn wait_for_signal(
    signals: &mut tokio::sync::broadcast::Receiver<Event>,
    topic: &str,
) -> Event {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = signals.recv().await.expect("signal channel closed");
            if event.event_type == "mqtt_message"
                && event.data.get("topic").and_then(Value::as_str) == Some(topic)
            {
                return event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no mqtt_message signal for {topic}"))
}

fn service_call(domain: &str, service: &str, entity_id: &str) -> ServiceCall {
    ServiceCall {
        domain: domain.into(),
        service: service.into(),
        data: Map::new(),
        target: Map::from_iter([("entity_id".into(), Value::String(entity_id.into()))]),
        context: Context::default(),
    }
}

fn service_call_with_data(
    domain: &str,
    service: &str,
    entity_id: &str,
    data: Value,
) -> ServiceCall {
    let mut call = service_call(domain, service, entity_id);
    call.data = data
        .as_object()
        .expect("service data must be an object")
        .clone();
    call
}

async fn wait_published(broker: &mut LocalBroker, topic: &str) -> (Value, u8, bool) {
    match broker
        .wait_for(|event| matches!(event, BrokerEvent::Published { topic: published, .. } if published == topic))
        .await
    {
        BrokerEvent::Published {
            payload,
            qos,
            retain,
            ..
        } => (serde_json::from_slice(&payload).unwrap_or(Value::String(String::from_utf8(payload).unwrap())), qos, retain),
        _ => unreachable!("wait_for filters to published packets"),
    }
}

#[tokio::test]
async fn real_wire_discovery_state_availability_service_reconnect_and_shutdown() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let mut signals = runtime.subscribe_signals();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-wire-test\nqos: 1\ndiscovery_prefix: homeassistant\nmax_payload_bytes: 1024\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    config.validate().unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker.wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, qos: 1 } if topic == "homeassistant/#")).await;
    integration
        .subscribe_topic("custom/+/event", 2)
        .await
        .unwrap();
    assert!(
        integration
            .subscribe_topic("custom/#/invalid", 1)
            .await
            .is_err()
    );
    broker.wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, qos: 2 } if topic == "custom/+/event")).await;
    broker
        .publish("custom/device/event", b"pressed".to_vec(), false)
        .await;
    let signal = wait_for_signal(&mut signals, "custom/device/event").await;
    assert_eq!(signal.data["payload"], "pressed");

    broker.publish(
        "homeassistant/sensor/room/temperature/config",
        r#"{"name":"Room Temperature","unique_id":"room-temperature-1","~":"lab","state_topic":"~/temperature","availability_topic":"~/available","value_template":"{{ value_json.temperature }}","unit_of_measurement":"°C","device":{"identifiers":["room-1"],"name":"Room"}}"#.as_bytes().to_vec(),
        true,
    ).await;
    broker.wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, .. } if topic == "lab/temperature")).await;
    broker
        .publish("lab/available", b"online".to_vec(), true)
        .await;
    broker
        .publish("lab/temperature", br#"{"temperature":21.5}"#.to_vec(), true)
        .await;
    wait_state(&runtime, "sensor.room_temperature", "21.5").await;
    let signal = wait_for_signal(&mut signals, "lab/temperature").await;
    assert_eq!(signal.data["payload"], r#"{"temperature":21.5}"#);
    assert_eq!(signal.data["payload_bytes"][0], 123);
    assert_eq!(signal.data["qos"], 1);
    assert_eq!(signal.data["retain"], true);
    let metadata = integration
        .entity_metadata()
        .into_iter()
        .find(|entity| entity.entity_id == "sensor.room_temperature")
        .unwrap();
    assert_eq!(metadata.unique_id, "room-temperature-1");
    assert_eq!(
        metadata.device,
        Some(json!({"identifiers":["room-1"],"name":"Room"}))
    );
    assert_eq!(
        runtime
            .get_state("sensor.room_temperature")
            .unwrap()
            .attributes["unit_of_measurement"],
        "°C"
    );

    broker.publish(
        "homeassistant/switch/room/heater/config",
        br#"{"name":"Heater","unique_id":"heater-1","state_topic":"lab/heater/state","command_topic":"lab/heater/set","command_template":"{\"state\":\"{{ value }}\"}","payload_on":"1","payload_off":"0"}"#.to_vec(),
        true,
    ).await;
    broker.wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, .. } if topic == "lab/heater/state")).await;
    broker
        .publish("lab/heater/state", b"0".to_vec(), true)
        .await;
    wait_state(&runtime, "switch.heater", "off").await;
    let result = runtime
        .call_service_async(service_call("switch", "turn_on", "switch.heater"))
        .await
        .unwrap();
    assert_eq!(result.response, None);
    let outgoing = broker.wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "lab/heater/set")).await;
    assert!(
        matches!(outgoing, BrokerEvent::Published { payload, qos: 1, retain: false, .. } if payload == br#"{"state":"1"}"#)
    );
    assert_eq!(
        runtime.get_state("switch.heater").unwrap().state,
        "off",
        "reported state must wait for MQTT state input"
    );
    broker
        .publish("lab/heater/state", b"1".to_vec(), true)
        .await;
    wait_state(&runtime, "switch.heater", "on").await;

    broker
        .publish(
            "homeassistant/binary_sensor/room/window/config",
            br#"{"name":"Window","unique_id":"window-1","state_topic":"lab/window","payload_on":"OPEN","payload_off":"CLOSED","qos":2}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, qos: 2 } if topic == "lab/window"))
        .await;
    broker.publish("lab/window", b"OPEN".to_vec(), true).await;
    wait_state(&runtime, "binary_sensor.window", "on").await;

    broker
        .publish(
            "homeassistant/light/room/desk/config",
            br#"{"name":"Desk","unique_id":"desk-light-1","state_topic":"lab/desk/state","command_topic":"lab/desk/set"}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, .. } if topic == "lab/desk/state"))
        .await;
    broker
        .publish("lab/desk/state", b"OFF".to_vec(), true)
        .await;
    wait_state(&runtime, "light.desk", "off").await;
    runtime
        .call_service_async(service_call("light", "turn_on", "light.desk"))
        .await
        .unwrap();
    let outgoing = broker
        .wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "lab/desk/set"))
        .await;
    assert!(matches!(outgoing, BrokerEvent::Published { payload, qos: 1, .. } if payload == b"ON"));
    broker.publish("lab/desk/state", b"ON".to_vec(), true).await;
    wait_state(&runtime, "light.desk", "on").await;

    broker
        .publish("lab/available", b"offline".to_vec(), true)
        .await;
    wait_state(&runtime, "sensor.room_temperature", "unavailable").await;
    broker
        .publish("lab/available", b"online".to_vec(), true)
        .await;
    wait_state(&runtime, "sensor.room_temperature", "21.5").await;

    broker
        .publish(
            "homeassistant/sensor/room/bad/config",
            b"{invalid".to_vec(),
            true,
        )
        .await;
    broker
        .publish(
            "homeassistant/sensor/room/oversized/config",
            vec![b'x'; 1100],
            true,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        integration
            .entity_metadata()
            .iter()
            .all(|entity| entity.unique_id != "bad" && entity.unique_id != "mqtt::oversized")
    );

    broker.drop_connection().await;
    wait_state(&runtime, "switch.heater", "unavailable").await;
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Connected))
        .await;
    broker
        .wait_for_subscriptions(&[
            ("homeassistant/#", 1),
            ("lab/heater/state", 1),
            ("custom/+/event", 2),
        ])
        .await;
    wait_state(&runtime, "switch.heater", "on").await;

    broker
        .publish("homeassistant/switch/room/heater/config", Vec::new(), true)
        .await;
    wait_removed(&runtime, "switch.heater").await;
    assert!(
        integration
            .entity_metadata()
            .iter()
            .all(|entity| entity.entity_id != "switch.heater")
    );
    assert!(
        runtime
            .call_service_async(service_call("switch", "turn_on", "switch.heater"))
            .await
            .is_err()
    );

    integration.shutdown().await.unwrap();
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Disconnected))
        .await;
}

#[test]
fn strict_config_validation_and_password_redaction() {
    let config: MqttConfig =
        serde_yaml::from_str("host: broker.local\nusername: user\npassword: very-secret\n")
            .unwrap();
    config.validate().unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("very-secret"));
    assert!(debug.contains("<redacted>"));
    assert!(serde_yaml::from_str::<MqttConfig>("host: broker.local\nunsupported: true\n").is_err());
    let invalid: MqttConfig = serde_yaml::from_str("host: broker.local\nqos: 3\n").unwrap();
    assert!(invalid.validate().is_err());
}

#[tokio::test]
async fn startup_returns_an_error_after_a_bounded_connection_wait() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-no-broker\n",
        address.port()
    ))
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(7),
        MqttIntegration::start(runtime(), config),
    )
    .await;
    assert!(matches!(result, Ok(Err(_))));
}

#[tokio::test]
async fn json_light_reports_features_and_publishes_supported_commands() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-light-wire\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker
        .publish(
            "homeassistant/light/lab/full/config",
        br#"{"name":"Full Light","unique_id":"full-light","schema":"json","state_topic":"lab/light/state","command_topic":"lab/light/set","brightness":true,"brightness_scale":100,"color_temp_kelvin":true,"supported_color_modes":["brightness","color_temp","hs","rgb","rgbw","rgbww","white","xy"],"effect":true,"effect_list":["rainbow"],"flash":true,"flash_time_short":3,"flash_time_long":12,"transition":true}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for_subscriptions(&[("lab/light/state", 1)])
        .await;

    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","brightness":50,"color_mode":"hs","color":{"h":180,"s":60},"effect":"rainbow"}"#.to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "light.full_light", "on").await;
    let state = runtime.get_state("light.full_light").unwrap();
    assert_eq!(state.attributes["brightness"], 128);
    assert_eq!(state.attributes["hs_color"], json!([180.0, 60.0]));
    assert_eq!(state.attributes["effect"], "rainbow");

    let command = runtime.call_service_async(service_call_with_data(
        "light",
        "turn_on",
        "light.full_light",
        json!({"brightness":127,"hs_color":[180,60],"effect":"rainbow","flash":"short","transition":2.5}),
    )).await.unwrap();
    assert_eq!(command.response, None);
    let (payload, qos, retain) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(qos, 1);
    assert!(!retain);
    assert_eq!(
        payload,
        json!({"state":"ON","brightness":50,"color":{"h":180.0,"s":60.0},"effect":"rainbow","flash":3,"transition":2.5})
    );
    assert_eq!(runtime.get_state("light.full_light").unwrap().state, "on");

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"color_temp_kelvin":3500}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(payload, json!({"state":"ON","color_temp":3500}));

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgb_color":[12,34,56]}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56}})
    );

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgbw_color":[12,34,56,78]}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56,"w":78}})
    );

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgbww_color":[12,34,56,78,90]}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56,"c":78,"w":90}})
    );

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"white":128}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(payload, json!({"state":"ON","white":128}));

    runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"xy_color":[0.25,0.4]}),
        ))
        .await
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(payload, json!({"state":"ON","color":{"x":0.25,"y":0.4}}));

    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"color_temp","color_temp":4000}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("light.full_light").is_some_and(|state| {
                state.attributes.get("color_temp_kelvin") == Some(&json!(4000))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"rgbw","color":{"r":1,"g":2,"b":3,"w":4}}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("light.full_light").is_some_and(|state| {
                state.attributes.get("rgbw_color") == Some(&json!([1.0, 2.0, 3.0, 4.0]))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"rgbww","color":{"r":1,"g":2,"b":3,"c":4,"w":5}}"#
                .to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("light.full_light").is_some_and(|state| {
                state.attributes.get("rgbww_color") == Some(&json!([1.0, 2.0, 3.0, 4.0, 5.0]))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"white"}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .get_state("light.full_light")
                .is_some_and(|state| state.attributes.get("color_mode") == Some(&json!("white")))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"rgb","color":{"r":1,"g":2,"b":3}}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("light.full_light").is_some_and(|state| {
                state.attributes.get("rgb_color") == Some(&json!([1.0, 2.0, 3.0]))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","color_mode":"xy","color":{"x":0.3,"y":0.6}}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .get_state("light.full_light")
                .is_some_and(|state| state.attributes.get("xy_color") == Some(&json!([0.3, 0.6])))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let error = runtime
        .call_service_async(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"bogus_feature":true}),
        ))
        .await;
    assert!(error.is_err(), "unsupported light options must be rejected");
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn cover_and_fan_services_publish_and_topics_report_state() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-platform-wire\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;

    broker.publish(
        "homeassistant/cover/lab/awning/config",
        br#"{"name":"Awning","unique_id":"awning","state_topic":"lab/awning/state","command_topic":"lab/awning/cmd","position_topic":"lab/awning/position","set_position_topic":"lab/awning/set-position","tilt_status_topic":"lab/awning/tilt","tilt_command_topic":"lab/awning/set-tilt","availability_topic":"lab/awning/availability","tilt_min":10,"tilt_max":90,"tilt_open_position":90,"tilt_closed_position":10,"payload_stop_tilt":"HALT"}"#.to_vec(),
        true,
    ).await;
    broker
        .wait_for_subscriptions(&[
            ("lab/awning/state", 1),
            ("lab/awning/position", 1),
            ("lab/awning/tilt", 1),
            ("lab/awning/availability", 1),
        ])
        .await;
    broker
        .publish("lab/awning/availability", b"online".to_vec(), true)
        .await;
    broker
        .publish("lab/awning/state", b"open".to_vec(), true)
        .await;
    wait_state(&runtime, "cover.awning", "open").await;
    broker
        .publish(
            "lab/awning/position",
            br#"{"position":50,"tilt_position":50}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("cover.awning").is_some_and(|state| {
                state.attributes.get("current_position") == Some(&json!(50))
                    && state.attributes.get("current_tilt_position") == Some(&json!(50))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    broker
        .publish("lab/awning/tilt", b"50".to_vec(), true)
        .await;
    broker
        .publish("lab/awning/availability", b"offline".to_vec(), true)
        .await;
    wait_state(&runtime, "cover.awning", "unavailable").await;
    broker
        .publish("lab/awning/availability", b"online".to_vec(), true)
        .await;
    wait_state(&runtime, "cover.awning", "open").await;
    runtime
        .call_service_async(service_call("cover", "open_cover", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("OPEN".into())
    );
    runtime
        .call_service_async(service_call("cover", "close_cover", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("CLOSE".into())
    );
    runtime
        .call_service_async(service_call("cover", "stop_cover", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("STOP".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "cover",
            "set_cover_position",
            "cover.awning",
            json!({"position":25}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-position")
            .await
            .0,
        json!(25)
    );
    runtime
        .call_service_async(service_call("cover", "open_cover_tilt", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(90)
    );
    runtime
        .call_service_async(service_call("cover", "close_cover_tilt", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(10)
    );
    runtime
        .call_service_async(service_call_with_data(
            "cover",
            "set_cover_tilt_position",
            "cover.awning",
            json!({"tilt_position":50}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(50)
    );
    runtime
        .call_service_async(service_call("cover", "stop_cover_tilt", "cover.awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        Value::String("HALT".into())
    );
    assert_eq!(
        runtime.get_state("cover.awning").unwrap().attributes["current_position"],
        50,
        "commands do not replace reported position when optimism is disabled"
    );

    broker.publish(
        "homeassistant/fan/lab/ceiling/config",
        br#"{"name":"Ceiling","unique_id":"ceiling","state_topic":"lab/fan/state","command_topic":"lab/fan/set","percentage_state_topic":"lab/fan/percentage","percentage_command_topic":"lab/fan/set-percentage","preset_mode_state_topic":"lab/fan/preset","preset_mode_command_topic":"lab/fan/set-preset","preset_modes":["auto","quiet"],"oscillation_state_topic":"lab/fan/oscillation","oscillation_command_topic":"lab/fan/set-oscillation","direction_state_topic":"lab/fan/direction","direction_command_topic":"lab/fan/set-direction","speed_range_min":1,"speed_range_max":5}"#.to_vec(),
        true,
    ).await;
    broker
        .wait_for_subscriptions(&[
            ("lab/fan/state", 1),
            ("lab/fan/percentage", 1),
            ("lab/fan/preset", 1),
            ("lab/fan/oscillation", 1),
            ("lab/fan/direction", 1),
        ])
        .await;
    broker.publish("lab/fan/state", b"ON".to_vec(), true).await;
    broker
        .publish("lab/fan/percentage", b"3".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/preset", b"auto".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/oscillation", b"oscillate_on".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/direction", b"forward".to_vec(), true)
        .await;
    wait_state(&runtime, "fan.ceiling", "on").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_state("fan.ceiling").is_some_and(|state| {
                state.attributes.get("percentage") == Some(&json!(60))
                    && state.attributes.get("preset_mode") == Some(&json!("auto"))
                    && state.attributes.get("oscillating") == Some(&json!(true))
                    && state.attributes.get("direction") == Some(&json!("forward"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "turn_on",
            "fan.ceiling",
            json!({"percentage":75,"preset_mode":"quiet"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set").await.0,
        Value::String("ON".into())
    );
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-percentage")
            .await
            .0,
        json!(4)
    );
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-preset").await.0,
        Value::String("quiet".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "set_percentage",
            "fan.ceiling",
            json!({"percentage":25}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-percentage")
            .await
            .0,
        json!(2)
    );
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "set_preset_mode",
            "fan.ceiling",
            json!({"preset_mode":"auto"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-preset").await.0,
        Value::String("auto".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "oscillate",
            "fan.ceiling",
            json!({"oscillating":false}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-oscillation")
            .await
            .0,
        Value::String("oscillate_off".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "set_direction",
            "fan.ceiling",
            json!({"direction":"reverse"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-direction").await.0,
        Value::String("reverse".into())
    );
    assert_eq!(
        runtime.get_state("fan.ceiling").unwrap().attributes["percentage"],
        60,
        "commands do not replace reported fan state when optimism is disabled"
    );
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_light_cover_fan_lifecycle_projects_registry_state_and_releases_leases() {
    const LIGHT_DISCOVERY: &[u8] = br#"{"name":"Desk Light","unique_id":"shared-light","schema":"json","state_topic":"lab/light/state","command_topic":"lab/light/set","json_attributes_topic":"lab/light/attributes","availability_topic":"lab/light/available","supported_color_modes":["color_temp","hs"],"brightness":true,"color_temp_kelvin":true,"min_kelvin":2500,"max_kelvin":6500,"effect":true,"effect_list":["pulse"],"flash":true,"transition":true}"#;
    const COVER_DISCOVERY: &[u8] = br#"{"name":"Awning","unique_id":"shared-cover","state_topic":"lab/cover/state","command_topic":"lab/cover/set","position_topic":"lab/cover/position","set_position_topic":"lab/cover/set-position","tilt_status_topic":"lab/cover/tilt","tilt_command_topic":"lab/cover/set-tilt","json_attributes_topic":"lab/cover/attributes","availability_topic":"lab/cover/available"}"#;
    const FAN_DISCOVERY: &[u8] = br#"{"name":"Ceiling Fan","unique_id":"shared-fan","state_topic":"lab/fan/state","command_topic":"lab/fan/set","percentage_state_topic":"lab/fan/percentage","percentage_command_topic":"lab/fan/set-percentage","preset_mode_state_topic":"lab/fan/preset","preset_mode_command_topic":"lab/fan/set-preset","preset_modes":["quiet","boost"],"oscillation_state_topic":"lab/fan/oscillation","oscillation_command_topic":"lab/fan/set-oscillation","direction_state_topic":"lab/fan/direction","direction_command_topic":"lab/fan/set-direction","speed_range_min":1,"speed_range_max":5,"json_attributes_topic":"lab/fan/attributes","availability_topic":"lab/fan/available"}"#;
    const DISCOVERIES: [(&str, &[u8]); 3] = [
        ("homeassistant/light/lab/desk/config", LIGHT_DISCOVERY),
        ("homeassistant/cover/lab/awning/config", COVER_DISCOVERY),
        ("homeassistant/fan/lab/ceiling/config", FAN_DISCOVERY),
    ];

    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let entry_id = "mqtt-entry-platform-lifecycle";
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-platform-lifecycle\nqos: 1\ndiscovery_prefix: homeassistant\nmax_payload_bytes: 4096\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config.clone(),
        lifecycle.clone(),
        registries.clone(),
        entry_id,
    )
    .await
    .unwrap();
    assert_eq!(runtime.loaded_components().components(), ["mqtt"]);
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;

    for (topic, payload) in DISCOVERIES {
        broker.publish(topic, payload.to_vec(), true).await;
    }
    broker
        .wait_for_subscriptions(&[
            ("lab/light/state", 1),
            ("lab/light/attributes", 1),
            ("lab/light/available", 1),
            ("lab/cover/state", 1),
            ("lab/cover/position", 1),
            ("lab/cover/tilt", 1),
            ("lab/cover/attributes", 1),
            ("lab/cover/available", 1),
            ("lab/fan/state", 1),
            ("lab/fan/percentage", 1),
            ("lab/fan/preset", 1),
            ("lab/fan/oscillation", 1),
            ("lab/fan/direction", 1),
            ("lab/fan/attributes", 1),
            ("lab/fan/available", 1),
        ])
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let metadata = integration.entity_metadata();
            if metadata.len() == 3
                && metadata
                    .iter()
                    .all(|entity| entity.entity_lifecycle_managed)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all three discoveries must be registered by the shared lifecycle");
    let component_receipts = runtime.loaded_components().components();
    for name in [
        "cover",
        "fan",
        "light",
        "mqtt.cover",
        "mqtt.fan",
        "mqtt.light",
    ] {
        assert!(component_receipts.iter().any(|receipt| receipt == name));
    }
    for entity_id in ["light.desk_light", "cover.awning", "fan.ceiling_fan"] {
        wait_state(&runtime, entity_id, "unavailable").await;
    }

    for topic in [
        "lab/light/available",
        "lab/cover/available",
        "lab/fan/available",
    ] {
        broker.publish(topic, b"online".to_vec(), true).await;
    }
    for entity_id in ["light.desk_light", "cover.awning", "fan.ceiling_fan"] {
        wait_state(&runtime, entity_id, "unknown").await;
    }
    let light_unknown = runtime.get_state("light.desk_light").unwrap();
    assert_eq!(
        light_unknown.attributes["supported_color_modes"],
        json!(["color_temp", "hs"])
    );
    assert_eq!(light_unknown.attributes["supported_features"], 44);
    assert_eq!(light_unknown.attributes["min_color_temp_kelvin"], 2500);
    assert_eq!(light_unknown.attributes["max_color_temp_kelvin"], 6500);
    assert_eq!(light_unknown.attributes["effect_list"], json!(["pulse"]));
    assert_eq!(light_unknown.attributes["color_mode"], Value::Null);
    let cover_unknown = runtime.get_state("cover.awning").unwrap();
    assert_eq!(cover_unknown.attributes["is_closed"], Value::Null);
    assert_eq!(cover_unknown.attributes["supported_features"], 255);
    let fan_unknown = runtime.get_state("fan.ceiling_fan").unwrap();
    assert_eq!(fan_unknown.attributes["percentage"], Value::Null);
    assert_eq!(fan_unknown.attributes["preset_mode"], Value::Null);
    assert_eq!(fan_unknown.attributes["oscillating"], Value::Null);
    assert_eq!(fan_unknown.attributes["direction"], Value::Null);
    assert_eq!(fan_unknown.attributes["supported_features"], 63);
    assert_eq!(
        fan_unknown.attributes["preset_modes"],
        json!(["quiet", "boost"])
    );

    for (topic, label) in [
        ("lab/light/attributes", "light ordinary value"),
        ("lab/cover/attributes", "cover ordinary value"),
        ("lab/fan/attributes", "fan ordinary value"),
    ] {
        broker
            .publish(
                topic,
                json!({"mqtt_payload_label":label}).to_string().into_bytes(),
                true,
            )
            .await;
    }
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","brightness":127,"color_mode":"color_temp","color_temp":4000}"#
                .to_vec(),
            true,
        )
        .await;
    broker
        .publish("lab/cover/state", b"open".to_vec(), true)
        .await;
    broker
        .publish("lab/cover/position", b"50".to_vec(), true)
        .await;
    broker.publish("lab/cover/tilt", b"25".to_vec(), true).await;
    broker.publish("lab/fan/state", b"ON".to_vec(), true).await;
    broker
        .publish("lab/fan/percentage", b"3".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/preset", b"quiet".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/oscillation", b"oscillate_on".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/direction", b"forward".to_vec(), true)
        .await;
    wait_state_attribute(
        &runtime,
        "light.desk_light",
        "on",
        "mqtt_payload_label",
        &json!("light ordinary value"),
    )
    .await;
    wait_state_attribute(
        &runtime,
        "cover.awning",
        "open",
        "mqtt_payload_label",
        &json!("cover ordinary value"),
    )
    .await;
    wait_state_attribute(
        &runtime,
        "fan.ceiling_fan",
        "on",
        "mqtt_payload_label",
        &json!("fan ordinary value"),
    )
    .await;
    assert_eq!(
        runtime.get_state("cover.awning").unwrap().attributes["current_position"],
        50
    );
    assert_eq!(
        runtime.get_state("cover.awning").unwrap().attributes["current_tilt_position"],
        25
    );
    assert_eq!(
        runtime.get_state("fan.ceiling_fan").unwrap().attributes["percentage"],
        60
    );

    // JSON nulls clear light values and project to unknown. Cover/fan expose
    // nullable SDK fields while their independently reported state is unknown.
    broker
        .publish(
            "lab/light/state",
            br#"{"state":null,"color_mode":null}"#.to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "light.desk_light", "unknown").await;
    assert_eq!(
        runtime.get_state("light.desk_light").unwrap().attributes["color_mode"],
        Value::Null
    );
    broker
        .publish("lab/cover/state", b"stopped".to_vec(), true)
        .await;
    wait_state(&runtime, "cover.awning", "unknown").await;
    assert_eq!(
        runtime.get_state("cover.awning").unwrap().attributes["is_closed"],
        Value::Null
    );
    broker
        .publish("lab/fan/percentage", b"None".to_vec(), true)
        .await;
    broker
        .publish("lab/fan/preset", b"None".to_vec(), true)
        .await;
    broker.publish("lab/fan/state", Vec::new(), true).await;
    wait_state(&runtime, "fan.ceiling_fan", "unknown").await;
    assert_eq!(
        runtime.get_state("fan.ceiling_fan").unwrap().attributes["percentage"],
        Value::Null
    );
    assert_eq!(
        runtime.get_state("fan.ceiling_fan").unwrap().attributes["preset_mode"],
        Value::Null
    );

    for topic in [
        "lab/light/available",
        "lab/cover/available",
        "lab/fan/available",
    ] {
        broker.publish(topic, b"offline".to_vec(), true).await;
    }
    for (entity_id, feature_bits, modes) in [
        ("light.desk_light", 44, Some(json!(["color_temp", "hs"]))),
        ("cover.awning", 255, None),
        ("fan.ceiling_fan", 63, None),
    ] {
        wait_state(&runtime, entity_id, "unavailable").await;
        let state = runtime.get_state(entity_id).unwrap();
        assert_eq!(state.attributes["supported_features"], feature_bits);
        assert!(!state.attributes.contains_key("mqtt_payload_label"));
        if let Some(modes) = modes {
            assert_eq!(state.attributes["supported_color_modes"], modes);
        }
    }
    assert_eq!(
        runtime.get_state("fan.ceiling_fan").unwrap().attributes["preset_modes"],
        json!(["quiet", "boost"])
    );

    let renames = [
        ("shared-light", "light.desk_light", "light.registry_desk"),
        ("shared-cover", "cover.awning", "cover.registry_awning"),
        ("shared-fan", "fan.ceiling_fan", "fan.registry_fan"),
    ];
    let mut registry_ids = HashMap::new();
    for (unique_id, old_id, new_id) in renames {
        let entry = registries
            .entities()
            .into_iter()
            .find(|entity| entity.unique_id == unique_id)
            .unwrap();
        registry_ids.insert(unique_id, entry.id.clone());
        registries
            .update_entity(
                old_id,
                EntityUpdate {
                    new_entity_id: Some(new_id.into()),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_state(&runtime, new_id, "unavailable").await;
        wait_metadata_entity_id(&integration, unique_id, new_id).await;
    }

    for topic in [
        "lab/light/available",
        "lab/cover/available",
        "lab/fan/available",
    ] {
        broker.publish(topic, b"online".to_vec(), true).await;
    }
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","brightness":127,"color_mode":"color_temp","color_temp":4000}"#
                .to_vec(),
            true,
        )
        .await;
    broker
        .publish("lab/cover/state", b"open".to_vec(), true)
        .await;
    broker.publish("lab/fan/state", b"ON".to_vec(), true).await;
    broker
        .publish("lab/fan/percentage", b"3".to_vec(), true)
        .await;
    wait_state(&runtime, "light.registry_desk", "on").await;
    wait_state(&runtime, "cover.registry_awning", "open").await;
    wait_state(&runtime, "fan.registry_fan", "on").await;

    runtime
        .call_service_async(service_call("light", "turn_off", "light.registry_desk"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/light/set").await.0,
        json!({"state":"OFF"})
    );
    runtime
        .call_service_async(service_call("cover", "open_cover", "cover.registry_awning"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/cover/set").await.0,
        Value::String("OPEN".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "set_percentage",
            "fan.registry_fan",
            json!({"percentage":75}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-percentage")
            .await
            .0,
        json!(4)
    );
    assert_eq!(
        integration
            .entity_metadata()
            .into_iter()
            .filter(|entity| entity.entity_lifecycle_managed)
            .map(|entity| entity.entity_id)
            .collect::<HashSet<_>>(),
        HashSet::from([
            "light.registry_desk".to_owned(),
            "cover.registry_awning".to_owned(),
            "fan.registry_fan".to_owned(),
        ])
    );

    for (_, _, entity_id) in renames {
        registries
            .update_entity(
                entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_removed(&runtime, entity_id).await;
    }
    for (domain, service, entity_id) in [
        ("light", "turn_on", "light.registry_desk"),
        ("cover", "close_cover", "cover.registry_awning"),
        ("fan", "turn_off", "fan.registry_fan"),
    ] {
        assert!(
            runtime
                .call_service_async(service_call(domain, service, entity_id))
                .await
                .is_err(),
            "disabled {entity_id} must not accept a service target"
        );
    }
    for (_, _, entity_id) in renames {
        registries
            .update_entity(
                entity_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
    }

    for (topic, _) in DISCOVERIES {
        broker.publish(topic, Vec::new(), true).await;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if integration.entity_metadata().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retained discovery removal must release every shared lifecycle lease");
    assert_eq!(runtime.loaded_components().components(), component_receipts);
    for (_, _, entity_id) in renames {
        wait_state(&runtime, entity_id, "unavailable").await;
    }

    for (topic, payload) in DISCOVERIES {
        broker.publish(topic, payload.to_vec(), true).await;
    }
    for (unique_id, _, entity_id) in renames {
        wait_metadata_entity_id(&integration, unique_id, entity_id).await;
        let entry = registries
            .entities()
            .into_iter()
            .find(|entity| entity.unique_id == unique_id)
            .unwrap();
        assert_eq!(entry.id, registry_ids[unique_id]);
    }
    for topic in [
        "lab/light/available",
        "lab/cover/available",
        "lab/fan/available",
    ] {
        broker.publish(topic, b"online".to_vec(), true).await;
    }
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","brightness":127,"color_mode":"color_temp","color_temp":4000}"#
                .to_vec(),
            true,
        )
        .await;
    broker
        .publish("lab/cover/state", b"open".to_vec(), true)
        .await;
    broker.publish("lab/fan/state", b"ON".to_vec(), true).await;
    broker
        .publish("lab/fan/percentage", b"3".to_vec(), true)
        .await;
    wait_state(&runtime, "light.registry_desk", "on").await;
    wait_state(&runtime, "cover.registry_awning", "open").await;
    wait_state(&runtime, "fan.registry_fan", "on").await;

    integration.shutdown().await.unwrap();
    for (_, _, entity_id) in renames {
        wait_state(&runtime, entity_id, "unavailable").await;
        assert_eq!(
            runtime.get_state(entity_id).unwrap().attributes["restored"],
            true
        );
    }

    // Starting a fresh MQTT owner for the same config entry and entity IDs is
    // also the regression check for shutdown releasing all provider leases.
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries.clone(),
        entry_id,
    )
    .await
    .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    for (topic, payload) in DISCOVERIES {
        broker.publish(topic, payload.to_vec(), true).await;
    }
    for (unique_id, _, entity_id) in renames {
        wait_metadata_entity_id(&integration, unique_id, entity_id).await;
        let entry = registries
            .entities()
            .into_iter()
            .find(|entity| entity.unique_id == unique_id)
            .unwrap();
        assert_eq!(entry.id, registry_ids[unique_id]);
    }
    for topic in [
        "lab/light/available",
        "lab/cover/available",
        "lab/fan/available",
    ] {
        broker.publish(topic, b"online".to_vec(), true).await;
    }
    broker
        .publish(
            "lab/light/state",
            br#"{"state":"ON","brightness":127,"color_mode":"color_temp","color_temp":4000}"#
                .to_vec(),
            true,
        )
        .await;
    broker
        .publish("lab/cover/state", b"open".to_vec(), true)
        .await;
    broker.publish("lab/fan/state", b"ON".to_vec(), true).await;
    broker
        .publish("lab/fan/percentage", b"3".to_vec(), true)
        .await;
    wait_state(&runtime, "light.registry_desk", "on").await;
    wait_state(&runtime, "cover.registry_awning", "open").await;
    wait_state(&runtime, "fan.registry_fan", "on").await;
    integration.shutdown().await.unwrap();
    assert_eq!(runtime.loaded_components().components(), component_receipts);
    lifecycle.shutdown().await.unwrap();
    assert_eq!(runtime.loaded_components().components(), component_receipts);
}

#[tokio::test]
async fn shared_fan_uses_reported_power_inclusive_speeds_and_zero_command_order() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-fan-command-boundaries\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries,
        "fan-command-entry",
    )
    .await
    .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    for (topic, discovery) in [
        (
            "homeassistant/fan/lab/basic/config",
            json!({
                "name":"Basic Fan","unique_id":"reported-basic-fan",
                "state_topic":"regression/basic/state","command_topic":"regression/basic/set"
            }),
        ),
        (
            "homeassistant/fan/lab/speed/config",
            json!({
                "name":"Speed Fan","unique_id":"reported-speed-fan",
                "state_topic":"regression/speed/state","command_topic":"regression/speed/set",
                "percentage_state_topic":"regression/speed/percentage",
                "percentage_command_topic":"regression/speed/set-percentage",
                "speed_range_min":1,"speed_range_max":5
            }),
        ),
    ] {
        broker
            .publish(topic, serde_json::to_vec(&discovery).unwrap(), true)
            .await;
    }
    broker
        .wait_for_subscriptions(&[
            ("regression/basic/state", 1),
            ("regression/speed/state", 1),
            ("regression/speed/percentage", 1),
        ])
        .await;
    broker
        .publish("regression/basic/state", b"ON".to_vec(), true)
        .await;
    wait_state(&runtime, "fan.basic_fan", "on").await;
    runtime
        .call_service_async(service_call("fan", "toggle", "fan.basic_fan"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "regression/basic/set").await.0,
        json!("OFF")
    );
    broker
        .publish("regression/basic/state", b"OFF".to_vec(), true)
        .await;
    wait_state(&runtime, "fan.basic_fan", "off").await;
    runtime
        .call_service_async(service_call("fan", "toggle", "fan.basic_fan"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "regression/basic/set").await.0,
        json!("ON")
    );

    // Pinned MQTT power is independent of nonzero reported speed.
    broker
        .publish("regression/speed/state", b"OFF".to_vec(), true)
        .await;
    broker
        .publish("regression/speed/percentage", b"3".to_vec(), true)
        .await;
    wait_state_attribute(&runtime, "fan.speed_fan", "off", "percentage", &json!(60)).await;
    assert_eq!(
        runtime.get_state("fan.speed_fan").unwrap().attributes["percentage_step"],
        20.0
    );
    for (service, expected) in [("increase_speed", 4), ("decrease_speed", 2)] {
        runtime
            .call_service_async(service_call("fan", service, "fan.speed_fan"))
            .await
            .unwrap();
        assert_eq!(
            wait_published(&mut broker, "regression/speed/set-percentage")
                .await
                .0,
            json!(expected)
        );
    }
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "set_percentage",
            "fan.speed_fan",
            json!({"percentage":0}),
        ))
        .await
        .unwrap();
    // Source FanEntity calls turn_off before MQTT's percentage setter.
    for (expected_topic, expected_payload) in [
        ("regression/speed/set", b"OFF".as_slice()),
        ("regression/speed/set-percentage", b"0".as_slice()),
    ] {
        match broker
            .wait_for(|event| matches!(event, BrokerEvent::Published { .. }))
            .await
        {
            BrokerEvent::Published { topic, payload, .. } => {
                assert_eq!(topic, expected_topic);
                assert_eq!(payload, expected_payload);
            }
            _ => unreachable!(),
        }
    }
    runtime
        .call_service_async(service_call_with_data(
            "fan",
            "turn_on",
            "fan.speed_fan",
            json!({"percentage":0}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "regression/speed/set").await.0,
        json!("ON")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), async {
            while let Some(event) = broker.events.recv().await {
                assert!(
                    !matches!(event, BrokerEvent::Published { .. }),
                    "turn_on(percentage=0) must not publish a speed command: {event:?}"
                );
            }
            panic!("broker ended before the absence check completed");
        })
        .await
        .is_err()
    );
    broker
        .publish("regression/speed/percentage", b"0".to_vec(), true)
        .await;
    wait_state_attribute(&runtime, "fan.speed_fan", "off", "percentage", &json!(0)).await;
    integration.shutdown().await.unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn fan_effect_chains_are_ack_sequenced_driver_owned_and_stopped_on_release_or_shutdown() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-fan-sequence\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries,
        "fan-sequence-entry",
    )
    .await
    .unwrap();
    assert_eq!(runtime.loaded_components().components(), ["mqtt"]);
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;

    for (object, name) in [("sequence", "Sequence Fan"), ("released", "Released Fan")] {
        broker
            .publish(
                &format!("homeassistant/fan/lab/{object}/config"),
                json!({
                    "name":name,
                    "unique_id":format!("fan-{object}"),
                    "state_topic":format!("regression/{object}/state"),
                    "command_topic":format!("regression/{object}/set"),
                    "percentage_state_topic":format!("regression/{object}/percentage"),
                    "percentage_command_topic":format!("regression/{object}/set-percentage"),
                    "speed_range_min":1,
                    "speed_range_max":5,
                    "optimistic":true
                })
                .to_string()
                .into_bytes(),
                true,
            )
            .await;
    }
    broker
        .wait_for_subscriptions(&[
            ("regression/sequence/state", 1),
            ("regression/sequence/percentage", 1),
            ("regression/released/state", 1),
            ("regression/released/percentage", 1),
        ])
        .await;
    assert!(
        runtime
            .loaded_components()
            .components()
            .iter()
            .any(|component| component == "mqtt.fan")
    );
    for object in ["sequence", "released"] {
        broker
            .publish(&format!("regression/{object}/state"), b"ON".to_vec(), true)
            .await;
        broker
            .publish(
                &format!("regression/{object}/percentage"),
                b"3".to_vec(),
                true,
            )
            .await;
    }
    wait_state_attribute(&runtime, "fan.sequence_fan", "on", "percentage", &json!(60)).await;
    wait_state_attribute(&runtime, "fan.released_fan", "on", "percentage", &json!(60)).await;

    let multi_target_call = ServiceCall {
        domain: "fan".into(),
        service: "set_percentage".into(),
        data: Map::from_iter([("percentage".into(), json!(0))]),
        target: Map::from_iter([(
            "entity_id".into(),
            json!(["fan.sequence_fan", "fan.released_fan"]),
        )]),
        context: Context::default(),
    };
    let mut batch_service = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.call_service_async(multi_target_call).await }
    });
    let sequence_off_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/sequence/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    let released_off_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/released/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    assert!(tokio::time::timeout(
        Duration::from_millis(150),
        broker.wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "regression/sequence/set-percentage" || topic == "regression/released/set-percentage"))
    )
    .await
    .is_err());

    // One target advances independently when its first effect is acknowledged.
    broker.acknowledge(sequence_off_id);
    let sequence_speed_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/sequence/set-percentage"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    wait_state_attribute(
        &runtime,
        "fan.sequence_fan",
        "off",
        "percentage",
        &json!(60),
    )
    .await;
    wait_state_attribute(&runtime, "fan.released_fan", "on", "percentage", &json!(60)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut batch_service)
            .await
            .is_err()
    );
    broker.acknowledge(sequence_speed_id);
    wait_state_attribute(&runtime, "fan.sequence_fan", "off", "percentage", &json!(0)).await;

    // A released handle rejects the later effect for that target while the
    // already admitted sibling chain completes normally.
    broker
        .publish("homeassistant/fan/lab/released/config", Vec::new(), true)
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if integration
                .entity_metadata()
                .iter()
                .all(|entity| entity.unique_id != "fan-released")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retained discovery removal must release the target lease");
    broker.acknowledge(released_off_id);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut batch_service)
            .await
            .expect("the inactive continuation must resolve the target chain")
            .unwrap()
            .is_err()
    );
    assert!(tokio::time::timeout(
        Duration::from_millis(150),
        broker.wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "regression/released/set-percentage"))
    )
    .await
    .is_err());

    // Aborting the service waiter does not cancel a chain already admitted to
    // the driver.
    let cancelled_service = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call_with_data(
                    "fan",
                    "set_percentage",
                    "fan.sequence_fan",
                    json!({"percentage":0}),
                ))
                .await
        }
    });
    let cancelled_off_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/sequence/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    cancelled_service.abort();
    assert!(cancelled_service.await.is_err());
    broker.acknowledge(cancelled_off_id);
    let cancelled_speed_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/sequence/set-percentage"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    broker.acknowledge(cancelled_speed_id);
    wait_state_attribute(&runtime, "fan.sequence_fan", "off", "percentage", &json!(0)).await;

    // Shutdown rejects an unstarted continuation and resolves the waiter.
    let shutdown_service = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call_with_data(
                    "fan",
                    "set_percentage",
                    "fan.sequence_fan",
                    json!({"percentage":0}),
                ))
                .await
        }
    });
    let _ = broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "regression/sequence/set"))
        .await;
    tokio::time::timeout(Duration::from_secs(3), integration.shutdown())
        .await
        .expect("shutdown must join with an effect awaiting PUBACK")
        .unwrap();
    assert!(shutdown_service.await.unwrap().is_err());
    assert!(tokio::time::timeout(
        Duration::from_millis(150),
        broker.wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "regression/sequence/set-percentage"))
    )
    .await
    .is_err());
    let receipts = runtime.loaded_components().components();
    assert!(receipts.iter().any(|component| component == "fan"));
    assert!(receipts.iter().any(|component| component == "mqtt.fan"));
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_cover_toggle_remembers_observed_direction_and_exposes_tilt_toggle() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-cover-direction\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries,
        "cover-direction-entry",
    )
    .await
    .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker
        .publish(
            "homeassistant/cover/lab/direction/config",
            serde_json::to_vec(&json!({
                "name":"Direction Cover","unique_id":"direction-cover",
                "state_topic":"regression/cover/state","command_topic":"regression/cover/set",
                "tilt_status_topic":"regression/cover/tilt",
                "tilt_command_topic":"regression/cover/set-tilt"
            }))
            .unwrap(),
            true,
        )
        .await;
    broker
        .wait_for_subscriptions(&[("regression/cover/state", 1), ("regression/cover/tilt", 1)])
        .await;
    for (motion, after_stop_command) in [("closing", "OPEN"), ("opening", "CLOSE")] {
        broker
            .publish("regression/cover/state", motion.as_bytes().to_vec(), true)
            .await;
        wait_state(&runtime, "cover.direction_cover", motion).await;
        runtime
            .call_service_async(service_call("cover", "toggle", "cover.direction_cover"))
            .await
            .unwrap();
        assert_eq!(
            wait_published(&mut broker, "regression/cover/set").await.0,
            json!("STOP")
        );
        broker
            .publish("regression/cover/state", b"open".to_vec(), true)
            .await;
        wait_state(&runtime, "cover.direction_cover", "open").await;
        runtime
            .call_service_async(service_call("cover", "toggle", "cover.direction_cover"))
            .await
            .unwrap();
        assert_eq!(
            wait_published(&mut broker, "regression/cover/set").await.0,
            json!(after_stop_command)
        );
    }
    for (position, expected) in [(0, 100), (100, 0)] {
        broker
            .publish(
                "regression/cover/tilt",
                position.to_string().into_bytes(),
                true,
            )
            .await;
        wait_state_attribute(
            &runtime,
            "cover.direction_cover",
            "open",
            "current_tilt_position",
            &json!(position),
        )
        .await;
        runtime
            .call_service_async(service_call(
                "cover",
                "toggle_cover_tilt",
                "cover.direction_cover",
            ))
            .await
            .unwrap();
        assert_eq!(
            wait_published(&mut broker, "regression/cover/set-tilt")
                .await
                .0,
            json!(expected)
        );
    }
    integration.shutdown().await.unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_switch_lifecycle_projects_metadata_state_targets_and_commands() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-switch-lifecycle\nqos: 1\ndiscovery_prefix: homeassistant\nmax_payload_bytes: 1024\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries.clone(),
        "mqtt-entry-switch-lifecycle",
    )
    .await
    .unwrap();
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Subscribed { topic, qos: 1 } if topic == "homeassistant/#"))
        .await;

    assert_eq!(
        runtime.services()["switch"]["turn_on"],
        json!({
            "name": "Turn on switch",
            "description": "Turns on a switch.",
            "target": {"entity": {"domain": "switch"}}
        })
    );
    assert_eq!(
        runtime.services()["switch"]["turn_off"]["name"],
        "Turn off switch"
    );
    assert_eq!(
        runtime.services()["switch"]["toggle"]["description"],
        "Toggles a switch on/off."
    );

    broker
        .publish(
            "homeassistant/switch/lab/heater/config",
            br#"{"name":"Heater","unique_id":"heater-switch-id","state_topic":"lab/heater/state","command_topic":"lab/heater/set","command_template":"{\"entity\":\"{{ entity_id }}\",\"state\":\"{{ value }}\"}","availability_topic":"lab/heater/available","payload_on":"1","payload_off":"0","state_on":"1","state_off":"0","optimistic":true,"device_class":"OUTLET","icon":"mdi:power","entity_category":"config","device":{"identifiers":["switch-device"],"name":"Kitchen"}}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for_subscriptions(&[("lab/heater/state", 1), ("lab/heater/available", 1)])
        .await;
    let entity_id = integration
        .entity_metadata()
        .into_iter()
        .find(|entity| entity.unique_id == "heater-switch-id")
        .unwrap()
        .entity_id;
    wait_state(&runtime, &entity_id, "unavailable").await;
    assert_eq!(
        runtime.get_state(&entity_id).unwrap().attributes["assumed_state"],
        true,
        "assumed state stays live while the entity is unavailable"
    );

    broker
        .publish("lab/heater/available", b"online".to_vec(), true)
        .await;
    broker
        .publish("lab/heater/state", b"0".to_vec(), true)
        .await;
    wait_state(&runtime, &entity_id, "off").await;
    assert_eq!(
        runtime.get_state(&entity_id).unwrap().attributes["device_class"],
        "outlet",
        "switch descriptor keeps the typed source device class"
    );
    let device = registries.devices().into_iter().find(|device| {
        device
            .identifiers
            .contains(&("mqtt".to_owned(), "switch-device".to_owned()))
    });
    assert!(
        device.is_some(),
        "the source device metadata is registered once"
    );

    let registered = registries
        .entities()
        .into_iter()
        .find(|entity| entity.domain == "switch" && entity.unique_id == "heater-switch-id")
        .unwrap();
    let renamed_id = "switch.kitchen_heater";
    registries
        .update_entity(
            &registered.entity_id,
            EntityUpdate {
                new_entity_id: Some(renamed_id.into()),
                name: Some(Some("Kitchen Heater".into())),
                device_class: Some(Some("switch".into())),
                icon: Some(Some("mdi:toggle-switch".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_state(&runtime, renamed_id, "off").await;
    assert_eq!(
        integration
            .entity_metadata()
            .into_iter()
            .find(|entity| entity.unique_id == "heater-switch-id")
            .unwrap()
            .entity_id,
        renamed_id
    );
    let renamed = runtime.get_state(renamed_id).unwrap();
    assert_eq!(renamed.attributes["friendly_name"], "Kitchen Heater");
    assert_eq!(renamed.attributes["device_class"], "switch");
    assert_eq!(renamed.attributes["icon"], "mdi:toggle-switch");

    broker
        .publish("lab/heater/state", b"unmapped".to_vec(), true)
        .await;
    wait_state(&runtime, renamed_id, "unknown").await;
    assert!(
        runtime
            .call_service_async(service_call_with_data(
                "switch",
                "turn_on",
                renamed_id,
                json!({"brightness": 50}),
            ))
            .await
            .is_err()
    );
    runtime
        .call_service_async(service_call("switch", "toggle", renamed_id))
        .await
        .unwrap();
    let outgoing = broker
        .wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "lab/heater/set"))
        .await;
    assert!(matches!(
        outgoing,
        BrokerEvent::Published { payload, qos: 1, retain: false, .. }
            if payload == br#"{"entity":"switch.kitchen_heater","state":"1"}"#
    ));
    wait_state(&runtime, renamed_id, "on").await;
    assert_eq!(
        runtime.get_state(renamed_id).unwrap().attributes["assumed_state"],
        true
    );

    registries
        .update_entity(
            renamed_id,
            EntityUpdate {
                disabled_by: Some(Some("user".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_removed(&runtime, renamed_id).await;
    assert!(
        runtime
            .call_service_async(service_call("switch", "turn_off", renamed_id))
            .await
            .is_err()
    );

    integration.shutdown().await.unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn button_number_select_and_text_use_local_mqtt_wire() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-value-wire\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker.publish("homeassistant/button/lab/reboot/config", br#"{"name":"Reboot","unique_id":"reboot","command_topic":"lab/reboot","payload_press":"PUSH"}"#.to_vec(), true).await;
    broker.publish("homeassistant/number/lab/target/config", br#"{"name":"Target","unique_id":"target","state_topic":"lab/target/state","command_topic":"lab/target/set","min":0,"max":10,"step":2}"#.to_vec(), true).await;
    broker.publish("homeassistant/select/lab/mode/config", br#"{"name":"Mode","unique_id":"mode","state_topic":"lab/mode/state","command_topic":"lab/mode/set","options":["low","high"]}"#.to_vec(), true).await;
    broker.publish("homeassistant/text/lab/label/config", br#"{"name":"Label","unique_id":"label","state_topic":"lab/label/state","command_topic":"lab/label/set","min":2,"max":10,"pattern":"^[a-z]+$"}"#.to_vec(), true).await;
    broker
        .wait_for_subscriptions(&[
            ("lab/target/state", 1),
            ("lab/mode/state", 1),
            ("lab/label/state", 1),
        ])
        .await;
    broker
        .publish("lab/target/state", b"4".to_vec(), true)
        .await;
    broker
        .publish("lab/mode/state", b"low".to_vec(), true)
        .await;
    broker
        .publish("lab/label/state", b"hello".to_vec(), true)
        .await;
    wait_state(&runtime, "number.target", "4").await;
    wait_state(&runtime, "select.mode", "low").await;
    wait_state(&runtime, "text.label", "hello").await;

    runtime
        .call_service_async(service_call("button", "press", "button.reboot"))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/reboot").await.0,
        Value::String("PUSH".into())
    );
    runtime
        .call_service_async(service_call_with_data(
            "number",
            "set_value",
            "number.target",
            json!({"value":6}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/target/set").await.0,
        json!(6)
    );
    assert_eq!(runtime.get_state("number.target").unwrap().state, "4");
    runtime
        .call_service_async(service_call_with_data(
            "select",
            "select_option",
            "select.mode",
            json!({"option":"high"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/mode/set").await.0,
        Value::String("high".into())
    );
    assert_eq!(runtime.get_state("select.mode").unwrap().state, "low");
    runtime
        .call_service_async(service_call_with_data(
            "text",
            "set_value",
            "text.label",
            json!({"value":"goodbye"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/label/set").await.0,
        Value::String("goodbye".into())
    );
    assert_eq!(runtime.get_state("text.label").unwrap().state, "hello");
    assert!(
        runtime
            .call_service_async(service_call_with_data(
                "text",
                "set_value",
                "text.label",
                json!({"value":"not valid"})
            ))
            .await
            .is_err()
    );
    assert!(
        runtime
            .call_service_async(service_call_with_data(
                "number",
                "set_value",
                "number.target",
                json!({"value":5})
            ))
            .await
            .is_err()
    );
    broker
        .publish("homeassistant/text/lab/label/config", Vec::new(), true)
        .await;
    wait_removed(&runtime, "text.label").await;
    assert!(
        integration
            .entity_metadata()
            .iter()
            .all(|entity| entity.entity_id != "text.label")
    );
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn qos0_completes_after_write_and_returns_no_queued_response() {
    let mut broker = LocalBroker::start().await;
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-qos0-wire\nqos: 0\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 0)])
        .await;
    broker
        .publish(
            "homeassistant/switch/lab/light/config",
            br#"{"name":"Light","unique_id":"qos0","state_topic":"lab/light/state","command_topic":"lab/light/set","optimistic":true}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for_subscriptions(&[("lab/light/state", 0)])
        .await;
    broker
        .publish("lab/light/state", b"OFF".to_vec(), true)
        .await;
    wait_state(&runtime, "switch.light", "off").await;

    let result = runtime
        .call_service_async(service_call("switch", "turn_on", "switch.light"))
        .await
        .unwrap();
    assert_eq!(result.response, None);
    assert_eq!(result.changed_states.len(), 1);
    assert_eq!(result.changed_states[0].entity_id, "switch.light");
    assert_eq!(result.changed_states[0].state, "on");
    assert_eq!(runtime.get_state("switch.light").unwrap().state, "on");
    let packet = broker
        .wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "lab/light/set"))
        .await;
    assert!(matches!(
        packet,
        BrokerEvent::Published {
            qos: 0,
            packet_id: None,
            ..
        }
    ));
    let unchanged = runtime
        .call_service_async(service_call("switch", "turn_on", "switch.light"))
        .await
        .unwrap();
    assert!(unchanged.changed_states.is_empty());
    let _ = broker
        .wait_for(|event| matches!(event, BrokerEvent::Published { topic, .. } if topic == "lab/light/set"))
        .await;
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn qos1_out_of_order_acks_cancellation_and_shutdown_are_deterministic() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-qos1-held\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    for (object, topic) in [("one", "lab/one/set"), ("two", "lab/two/set")] {
        broker
            .publish(
                &format!("homeassistant/switch/lab/{object}/config"),
                format!(r#"{{"name":"{object}","unique_id":"{object}","command_topic":"{topic}","optimistic":true}}"#).into_bytes(),
                true,
            )
            .await;
    }
    wait_state(&runtime, "switch.one", "unknown").await;
    wait_state(&runtime, "switch.two", "unknown").await;
    let call = ServiceCall {
        domain: "switch".into(),
        service: "turn_on".into(),
        data: Map::new(),
        target: Map::from_iter([("entity_id".into(), json!(["switch.one", "switch.two"]))]),
        context: Context::default(),
    };
    let mut service = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.call_service_async(call).await }
    });
    let (_, first_id) = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, .. } if topic == "lab/one/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, topic, .. } => (topic, packet_id),
        _ => unreachable!(),
    };
    let second_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, .. } if topic == "lab/two/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    broker.acknowledge(second_id);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .get_state("switch.two")
                .is_some_and(|state| state.state == "on")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut service)
            .await
            .is_err()
    );
    assert!(
        runtime
            .get_state("switch.one")
            .is_none_or(|state| state.state != "on")
    );
    broker.acknowledge(first_id);
    let result = service.await.unwrap().unwrap();
    assert_eq!(result.changed_states.len(), 2);
    assert_eq!(
        result
            .changed_states
            .iter()
            .map(|state| state.entity_id.as_str())
            .collect::<Vec<_>>(),
        ["switch.one", "switch.two"]
    );

    let canceled = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("switch", "turn_off", "switch.two"))
                .await
        }
    });
    let canceled_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, .. } if topic == "lab/two/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    canceled.abort();
    assert!(canceled.await.is_err());
    assert_eq!(runtime.get_state("switch.two").unwrap().state, "on");
    broker.acknowledge(canceled_id);
    wait_state(&runtime, "switch.two", "off").await;

    let pending = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("switch", "turn_on", "switch.two"))
                .await
        }
    });
    let _ = broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, .. } if topic == "lab/two/set"))
        .await;
    tokio::time::timeout(Duration::from_secs(3), integration.shutdown())
        .await
        .expect("shutdown must join with a pending acknowledgment")
        .unwrap();
    assert!(pending.await.unwrap().is_err());
}

#[tokio::test]
async fn qos2_service_waits_for_pubcomp() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-qos2-held\nqos: 2\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 2)])
        .await;
    broker
        .publish(
            "homeassistant/button/lab/action/config",
            br#"{"name":"Action","unique_id":"qos2","command_topic":"lab/action/set"}"#.to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "button.action", "unknown").await;
    let mut service = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("button", "press", "button.action"))
                .await
        }
    });
    let packet_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 2, .. } if topic == "lab/action/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut service)
            .await
            .is_err()
    );
    let pressed_state = runtime.get_state("button.action").unwrap();
    assert!(pressed_state.state.ends_with("+00:00"));
    assert!(pressed_state.state.contains('T'));
    broker.acknowledge(packet_id);
    let result = service.await.unwrap().unwrap();
    assert_eq!(result.changed_states.len(), 1);
    assert_eq!(result.changed_states[0].entity_id, "button.action");
    assert_eq!(result.changed_states[0].state, pressed_state.state);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn qos1_callback_timeout_completes_normally_and_late_ack_does_not_stick() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-qos1-timeout\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start(runtime.clone(), config)
        .await
        .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker
        .publish(
            "homeassistant/switch/lab/timeout/config",
            br#"{"name":"Timeout","unique_id":"timeout","command_topic":"lab/timeout/set","optimistic":true}"#.to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "switch.timeout", "unknown").await;

    let mut service = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("switch", "turn_on", "switch.timeout"))
                .await
        }
    });
    let packet_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "lab/timeout/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    assert!(
        runtime
            .get_state("switch.timeout")
            .is_none_or(|state| state.state != "on")
    );
    let timed_out = tokio::time::timeout(Duration::from_secs(12), &mut service)
        .await
        .expect("the source-compatible ten-second callback timeout should finish the service")
        .unwrap()
        .unwrap();
    assert_eq!(timed_out.changed_states.len(), 1);
    assert_eq!(timed_out.changed_states[0].state, "on");
    assert_eq!(runtime.get_state("switch.timeout").unwrap().state, "on");

    broker.acknowledge(packet_id);
    let mut next = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("switch", "turn_off", "switch.timeout"))
                .await
        }
    });
    let next_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "lab/timeout/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    broker.acknowledge(next_id);
    let next_result = tokio::time::timeout(Duration::from_secs(3), &mut next)
        .await
        .expect("a late acknowledgment must not prevent later commands")
        .unwrap()
        .unwrap();
    assert_eq!(next_result.changed_states.len(), 1);
    assert_eq!(next_result.changed_states[0].state, "off");
    assert_eq!(runtime.get_state("switch.timeout").unwrap().state, "off");
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn held_ack_completion_while_unavailable_preserves_new_attributes_and_reconnects() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-offline-pending-ack\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries,
        "mqtt-entry-offline-pending-ack",
    )
    .await
    .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker
        .publish(
            "homeassistant/switch/lab/heater/config",
            br#"{"name":"Heater","unique_id":"offline-ack","state_topic":"lab/heater/state","command_topic":"lab/heater/set","availability_topic":"lab/heater/available","json_attributes_topic":"lab/heater/attributes","optimistic":true}"#.to_vec(),
            true,
        )
        .await;
    broker
        .wait_for_subscriptions(&[
            ("lab/heater/state", 1),
            ("lab/heater/available", 1),
            ("lab/heater/attributes", 1),
        ])
        .await;
    broker
        .publish("lab/heater/available", b"online".to_vec(), true)
        .await;
    broker
        .publish("lab/heater/state", b"OFF".to_vec(), true)
        .await;
    broker
        .publish(
            "lab/heater/attributes",
            br#"{"temperature":20}"#.to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "switch.heater", "off").await;

    let mut service = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("switch", "turn_on", "switch.heater"))
                .await
        }
    });
    let packet_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "lab/heater/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    broker
        .publish(
            "lab/heater/attributes",
            br#"{"temperature":21,"humidity":41}"#.to_vec(),
            true,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .get_state("switch.heater")
                .is_some_and(|state| state.attributes.get("humidity") == Some(&json!(41)))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    broker
        .publish("lab/heater/available", b"offline".to_vec(), true)
        .await;
    wait_state(&runtime, "switch.heater", "unavailable").await;
    assert_eq!(
        runtime.get_state("switch.heater").unwrap().attributes["assumed_state"],
        true
    );

    broker.acknowledge(packet_id);
    let result = tokio::time::timeout(Duration::from_secs(3), &mut service)
        .await
        .expect("the held acknowledgment should complete the service")
        .unwrap()
        .unwrap();
    assert!(result.changed_states.is_empty());
    let unavailable = runtime.get_state("switch.heater").unwrap();
    assert_eq!(unavailable.state, "unavailable");
    assert_eq!(unavailable.attributes["assumed_state"], true);
    assert!(!unavailable.attributes.contains_key("humidity"));

    broker.drop_connection().await;
    wait_state(&runtime, "switch.heater", "unavailable").await;
    broker
        .wait_for(|event| matches!(event, BrokerEvent::Connected))
        .await;
    broker
        .wait_for_subscriptions(&[
            ("homeassistant/#", 1),
            ("lab/heater/state", 1),
            ("lab/heater/available", 1),
            ("lab/heater/attributes", 1),
        ])
        .await;
    broker
        .publish("lab/heater/available", b"online".to_vec(), true)
        .await;
    wait_state(&runtime, "switch.heater", "on").await;
    let restored = runtime.get_state("switch.heater").unwrap();
    assert_eq!(restored.attributes["assumed_state"], true);
    assert_eq!(restored.attributes["humidity"], 41);
    integration.shutdown().await.unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_button_presses_ack_independently_and_report_their_timestamp_states() {
    let mut broker = LocalBroker::start().await;
    broker.hold_acknowledgments(true);
    let runtime = runtime();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-button-async\nqos: 1\n",
        broker.address.rsplit(':').next().unwrap()
    ))
    .unwrap();
    let integration = MqttIntegration::start_with_entity_lifecycle(
        runtime.clone(),
        config,
        lifecycle.clone(),
        registries,
        "mqtt-entry-button-async",
    )
    .await
    .unwrap();
    broker
        .wait_for_subscriptions(&[("homeassistant/#", 1)])
        .await;
    broker
        .publish(
            "homeassistant/button/lab/action/config",
            br#"{"name":"Action","unique_id":"async-action","command_topic":"lab/action/set"}"#
                .to_vec(),
            true,
        )
        .await;
    wait_state(&runtime, "button.action", "unknown").await;

    let mut first = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("button", "press", "button.action"))
                .await
        }
    });
    let first_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "lab/action/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    let first_state = runtime.get_state("button.action").unwrap().state;
    assert!(first_state.ends_with("+00:00"));

    tokio::time::sleep(Duration::from_millis(5)).await;
    let mut second = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .call_service_async(service_call("button", "press", "button.action"))
                .await
        }
    });
    let second_id = match broker
        .wait_for(|event| matches!(event, BrokerEvent::AwaitingAcknowledgment { topic, qos: 1, .. } if topic == "lab/action/set"))
        .await
    {
        BrokerEvent::AwaitingAcknowledgment { packet_id, .. } => packet_id,
        _ => unreachable!(),
    };
    let second_state = runtime.get_state("button.action").unwrap().state;
    assert_ne!(first_state, second_state);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut first)
            .await
            .is_err()
    );

    broker.acknowledge(second_id);
    let second_result = tokio::time::timeout(Duration::from_secs(3), &mut second)
        .await
        .expect("the second button call must progress while the first awaits PUBACK")
        .unwrap()
        .unwrap();
    assert_eq!(second_result.changed_states.len(), 1);
    assert_eq!(second_result.changed_states[0].state, second_state);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut first)
            .await
            .is_err()
    );

    broker.acknowledge(first_id);
    let first_result = first.await.unwrap().unwrap();
    assert_eq!(first_result.changed_states.len(), 1);
    assert_eq!(first_result.changed_states[0].state, first_state);
    assert_eq!(
        runtime.get_state("button.action").unwrap().state,
        second_state
    );
    integration.shutdown().await.unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn canceling_start_signals_and_reaps_the_connected_event_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (connect_tx, connect_rx) = oneshot::channel();
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let broker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (header, _) = read_packet(&mut stream).await.unwrap();
        assert_eq!(header >> 4, 1, "first MQTT packet must be CONNECT");
        let _ = connect_tx.send(());
        let stopped =
            match tokio::time::timeout(Duration::from_secs(3), read_packet(&mut stream)).await {
                Ok(Ok((header, _))) => header >> 4 == 14,
                Ok(Err(_)) => true,
                Err(_) => false,
            };
        let _ = disconnect_tx.send(stopped);
    });
    let config: MqttConfig = serde_yaml::from_str(&format!(
        "host: 127.0.0.1\nport: {}\nclient_id: mqtt-cancel-start\n",
        address.port()
    ))
    .unwrap();
    let start = tokio::spawn(async move { MqttIntegration::start(runtime(), config).await });
    tokio::time::timeout(Duration::from_secs(3), connect_rx)
        .await
        .expect("the client should connect before the start future is canceled")
        .unwrap();
    start.abort();
    match start.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the startup task should have been canceled"),
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(3), disconnect_rx)
            .await
            .expect("canceling startup must signal and reap the event-loop worker")
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(3), broker)
        .await
        .expect("the loopback broker task should finish")
        .unwrap();
}
