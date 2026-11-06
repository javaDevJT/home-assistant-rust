use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use hass_core::{Context, Event, MemoryStore, Runtime, ServiceCall};
use hass_mqtt::{MqttConfig, MqttIntegration};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
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
                            topic,
                            payload: body[payload_offset..].to_vec(),
                            qos,
                            retain,
                        }).ok();
                        if let Some(id) = id {
                            write_packet(&mut writer, 0x40, &id.to_be_bytes()).await?;
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
        .call_service(service_call("switch", "turn_on", "switch.heater"))
        .unwrap();
    assert_eq!(result.response, Some(json!({"queued": 1})));
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
        .call_service(service_call("light", "turn_on", "light.desk"))
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
            .call_service(service_call("switch", "turn_on", "switch.heater"))
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

    let command = runtime.call_service(service_call_with_data(
        "light",
        "turn_on",
        "light.full_light",
        json!({"brightness":127,"hs_color":[180,60],"effect":"rainbow","flash":"short","transition":2.5}),
    )).unwrap();
    assert_eq!(command.response, Some(json!({"queued":1})));
    let (payload, qos, retain) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(qos, 1);
    assert!(!retain);
    assert_eq!(
        payload,
        json!({"state":"ON","brightness":50,"color":{"h":180.0,"s":60.0},"effect":"rainbow","flash":3,"transition":2.5})
    );
    assert_eq!(runtime.get_state("light.full_light").unwrap().state, "on");

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"color_temp_kelvin":3500}),
        ))
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(payload, json!({"state":"ON","color_temp":3500}));

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgb_color":[12,34,56]}),
        ))
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56}})
    );

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgbw_color":[12,34,56,78]}),
        ))
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56,"w":78}})
    );

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"rgbww_color":[12,34,56,78,90]}),
        ))
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(
        payload,
        json!({"state":"ON","color":{"r":12,"g":34,"b":56,"c":78,"w":90}})
    );

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"white":128}),
        ))
        .unwrap();
    let (payload, _, _) = wait_published(&mut broker, "lab/light/set").await;
    assert_eq!(payload, json!({"state":"ON","white":128}));

    runtime
        .call_service(service_call_with_data(
            "light",
            "turn_on",
            "light.full_light",
            json!({"xy_color":[0.25,0.4]}),
        ))
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

    let error = runtime.call_service(service_call_with_data(
        "light",
        "turn_on",
        "light.full_light",
        json!({"bogus_feature":true}),
    ));
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
        .call_service(service_call("cover", "open_cover", "cover.awning"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("OPEN".into())
    );
    runtime
        .call_service(service_call("cover", "close_cover", "cover.awning"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("CLOSE".into())
    );
    runtime
        .call_service(service_call("cover", "stop_cover", "cover.awning"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/cmd").await.0,
        Value::String("STOP".into())
    );
    runtime
        .call_service(service_call_with_data(
            "cover",
            "set_cover_position",
            "cover.awning",
            json!({"position":25}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-position")
            .await
            .0,
        json!(25)
    );
    runtime
        .call_service(service_call("cover", "open_cover_tilt", "cover.awning"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(90)
    );
    runtime
        .call_service(service_call("cover", "close_cover_tilt", "cover.awning"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(10)
    );
    runtime
        .call_service(service_call_with_data(
            "cover",
            "set_cover_tilt_position",
            "cover.awning",
            json!({"tilt_position":50}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/awning/set-tilt").await.0,
        json!(50)
    );
    runtime
        .call_service(service_call("cover", "stop_cover_tilt", "cover.awning"))
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
                state.attributes.get("percentage") == Some(&json!(50))
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
        .call_service(service_call_with_data(
            "fan",
            "turn_on",
            "fan.ceiling",
            json!({"percentage":75,"preset_mode":"quiet"}),
        ))
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
        .call_service(service_call_with_data(
            "fan",
            "set_percentage",
            "fan.ceiling",
            json!({"percentage":25}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-percentage")
            .await
            .0,
        json!(2)
    );
    runtime
        .call_service(service_call_with_data(
            "fan",
            "set_preset_mode",
            "fan.ceiling",
            json!({"preset_mode":"auto"}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-preset").await.0,
        Value::String("auto".into())
    );
    runtime
        .call_service(service_call_with_data(
            "fan",
            "oscillate",
            "fan.ceiling",
            json!({"oscillating":false}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-oscillation")
            .await
            .0,
        Value::String("oscillate_off".into())
    );
    runtime
        .call_service(service_call_with_data(
            "fan",
            "set_direction",
            "fan.ceiling",
            json!({"direction":"reverse"}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/fan/set-direction").await.0,
        Value::String("reverse".into())
    );
    assert_eq!(
        runtime.get_state("fan.ceiling").unwrap().attributes["percentage"],
        50,
        "commands do not replace reported fan state when optimism is disabled"
    );
    integration.shutdown().await.unwrap();
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
        .call_service(service_call("button", "press", "button.reboot"))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/reboot").await.0,
        Value::String("PUSH".into())
    );
    runtime
        .call_service(service_call_with_data(
            "number",
            "set_value",
            "number.target",
            json!({"value":6}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/target/set").await.0,
        json!(6)
    );
    assert_eq!(runtime.get_state("number.target").unwrap().state, "4");
    runtime
        .call_service(service_call_with_data(
            "select",
            "select_option",
            "select.mode",
            json!({"option":"high"}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/mode/set").await.0,
        Value::String("high".into())
    );
    assert_eq!(runtime.get_state("select.mode").unwrap().state, "low");
    runtime
        .call_service(service_call_with_data(
            "text",
            "set_value",
            "text.label",
            json!({"value":"goodbye"}),
        ))
        .unwrap();
    assert_eq!(
        wait_published(&mut broker, "lab/label/set").await.0,
        Value::String("goodbye".into())
    );
    assert_eq!(runtime.get_state("text.label").unwrap().state, "hello");
    assert!(
        runtime
            .call_service(service_call_with_data(
                "text",
                "set_value",
                "text.label",
                json!({"value":"not valid"})
            ))
            .is_err()
    );
    assert!(
        runtime
            .call_service(service_call_with_data(
                "number",
                "set_value",
                "number.target",
                json!({"value":5})
            ))
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
