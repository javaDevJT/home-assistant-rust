//! Compiled-process acceptance coverage for MQTT switch discovery and lifecycle.

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};

const TOKEN: &str = "rust-rewrite-mqtt-process-test-token-never-use-in-production";
const DISCOVERY_TOPIC: &str = "homeassistant/switch/process/heater/config";
const STATE_TOPIC: &str = "process/heater/state";
const AVAILABILITY_TOPIC: &str = "process/heater/availability";
const COMMAND_TOPIC: &str = "process/heater/set";
const UNIQUE_ID: &str = "compiled-process-switch-unique-id";
const RENAMED_ENTITY_ID: &str = "switch.compiled_process_heater";

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
enum BrokerCommand {
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
}

#[derive(Debug)]
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

#[derive(Clone)]
struct RetainedMessage {
    payload: Vec<u8>,
    qos: u8,
}

struct ParsedPublish {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    retain: bool,
    packet_id: Option<u16>,
}

struct LocalBroker {
    address: String,
    commands: mpsc::UnboundedSender<BrokerCommand>,
    events: mpsc::UnboundedReceiver<BrokerEvent>,
    task: JoinHandle<()>,
}

impl LocalBroker {
    async fn start() -> Self {
        // Keep the test broker numeric and strictly loopback-only.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut retained = BTreeMap::<String, RetainedMessage>::new();
            let mut packet_id = 1u16;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if serve_connection(
                    stream,
                    &mut command_rx,
                    &event_tx,
                    &mut retained,
                    &mut packet_id,
                )
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

    fn publish(&self, topic: &str, payload: impl Into<Vec<u8>>, retain: bool) {
        self.commands
            .send(BrokerCommand::Publish {
                topic: topic.to_owned(),
                payload: payload.into(),
                retain,
            })
            .unwrap();
    }

    async fn wait_for(&mut self, mut predicate: impl FnMut(&BrokerEvent) -> bool) -> BrokerEvent {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut seen = Vec::new();
        loop {
            let event = match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => panic!("synthetic MQTT broker ended; saw {seen:?}"),
                Err(_) => panic!("timed out waiting for broker event; saw {seen:?}"),
            };
            if predicate(&event) {
                return event;
            }
            seen.push(event_name(&event));
        }
    }

    async fn wait_for_subscription(&mut self, topic: &str) {
        self.wait_for_subscriptions(&[topic]).await;
    }

    async fn wait_for_subscriptions(&mut self, topics: &[&str]) {
        let expected = topics
            .iter()
            .map(|topic| (*topic).to_owned())
            .collect::<BTreeSet<_>>();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut subscribed = BTreeSet::new();
        while !expected.is_subset(&subscribed) {
            let event = match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    panic!("synthetic MQTT broker ended; subscriptions seen: {subscribed:?}")
                }
                Err(_) => panic!("timed out waiting for subscriptions; seen: {subscribed:?}"),
            };
            if let BrokerEvent::Subscribed { topic, qos: 1 } = event {
                subscribed.insert(topic);
            }
        }
    }

    async fn wait_for_disconnect(&mut self) {
        self.wait_for(|event| matches!(event, BrokerEvent::Disconnected))
            .await;
    }

    async fn assert_no_command_for(&mut self, duration: Duration) {
        let command_topic = COMMAND_TOPIC;
        let unexpected = tokio::time::timeout(
            duration,
            self.wait_for(|event| {
                matches!(event, BrokerEvent::Published { topic, .. } if topic == command_topic)
            }),
        )
        .await;
        assert!(
            unexpected.is_err(),
            "received an MQTT command after its entity or client was quiesced"
        );
    }
}

impl Drop for LocalBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn event_name(event: &BrokerEvent) -> &'static str {
    match event {
        BrokerEvent::Connected => "connected",
        BrokerEvent::Subscribed { .. } => "subscribed",
        BrokerEvent::Published { .. } => "published",
        BrokerEvent::Disconnected => "disconnected",
    }
}

async fn serve_connection(
    stream: TcpStream,
    commands: &mut mpsc::UnboundedReceiver<BrokerCommand>,
    events: &mpsc::UnboundedSender<BrokerEvent>,
    retained: &mut BTreeMap<String, RetainedMessage>,
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
                let Some((header, body)) = packet else {
                    events.send(BrokerEvent::Disconnected).ok();
                    break;
                };
                match header >> 4 {
                    1 => {
                        events.send(BrokerEvent::Connected).ok();
                        write_packet(&mut writer, 0x20, &[0, 0]).await?;
                    }
                    3 => {
                        let ParsedPublish {
                            topic,
                            payload,
                            qos,
                            retain,
                            packet_id,
                        } = parse_publish(header, &body)?;
                        events.send(BrokerEvent::Published {
                            topic,
                            payload,
                            qos,
                            retain,
                        }).ok();
                        if let Some(id) = packet_id {
                            write_packet(&mut writer, 0x40, &id.to_be_bytes()).await?;
                        }
                    }
                    8 => {
                        let (id, subscriptions) = parse_subscribe(&body)?;
                        let mut response = id.to_be_bytes().to_vec();
                        for (topic, qos) in &subscriptions {
                            events.send(BrokerEvent::Subscribed {
                                topic: topic.clone(),
                                qos: *qos,
                            }).ok();
                            response.push(*qos);
                        }
                        write_packet(&mut writer, 0x90, &response).await?;

                        for (filter, requested_qos) in subscriptions {
                            let matching = retained
                                .iter()
                                .filter(|(topic, _)| topic_matches(&filter, topic))
                                .map(|(topic, message)| (topic.clone(), message.clone()))
                                .collect::<Vec<_>>();
                            for (topic, message) in matching {
                                let qos = requested_qos.min(message.qos);
                                let id = if qos == 0 {
                                    None
                                } else {
                                    let current = *packet_id;
                                    *packet_id = (*packet_id).wrapping_add(1).max(1);
                                    Some(current)
                                };
                                write_publish(&mut writer, &topic, &message.payload, qos, true, id).await?;
                            }
                        }
                    }
                    4 | 10 => {}
                    12 => write_packet(&mut writer, 0xd0, &[]).await?,
                    14 => {
                        events.send(BrokerEvent::Disconnected).ok();
                        break;
                    }
                    _ => {}
                }
            }
            command = commands.recv() => {
                let Some(BrokerCommand::Publish { topic, payload, retain }) = command else {
                    break;
                };
                if retain {
                    if payload.is_empty() {
                        retained.remove(&topic);
                    } else {
                        retained.insert(topic.clone(), RetainedMessage {
                            payload: payload.clone(),
                            qos: 1,
                        });
                    }
                }
                let id = *packet_id;
                *packet_id = (*packet_id).wrapping_add(1).max(1);
                write_publish(&mut writer, &topic, &payload, 1, retain, Some(id)).await?;
            }
        }
    }
    reader_task.abort();
    Ok(())
}

async fn read_packet(reader: &mut (impl AsyncReadExt + Unpin)) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 1];
    reader.read_exact(&mut header).await?;
    let mut remaining = 0usize;
    let mut multiplier = 1usize;
    for _ in 0..4 {
        let mut digit = [0; 1];
        reader.read_exact(&mut digit).await?;
        remaining = remaining.saturating_add(((digit[0] & 0x7f) as usize) * multiplier);
        if digit[0] & 0x80 == 0 {
            let mut body = vec![0; remaining];
            reader.read_exact(&mut body).await?;
            return Ok((header[0], body));
        }
        multiplier *= 128;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid MQTT remaining length",
    ))
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

async fn write_publish(
    writer: &mut (impl AsyncWriteExt + Unpin),
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    packet_id: Option<u16>,
) -> io::Result<()> {
    let mut body = Vec::new();
    write_mqtt_string(&mut body, topic)?;
    if let Some(id) = packet_id {
        body.extend_from_slice(&id.to_be_bytes());
    }
    body.extend_from_slice(payload);
    let header = 0x30 | (qos << 1) | u8::from(retain);
    write_packet(writer, header, &body).await
}

fn parse_publish(header: u8, body: &[u8]) -> io::Result<ParsedPublish> {
    let mut offset = 0;
    let topic = read_mqtt_string(body, &mut offset)?;
    let qos = (header >> 1) & 0x03;
    let packet_id = if qos == 0 {
        None
    } else {
        Some(read_mqtt_u16(body, &mut offset)?)
    };
    Ok(ParsedPublish {
        topic,
        payload: body[offset..].to_vec(),
        qos,
        packet_id,
        retain: header & 0x01 != 0,
    })
}

fn parse_subscribe(body: &[u8]) -> io::Result<(u16, Vec<(String, u8)>)> {
    let mut offset = 0;
    let id = read_mqtt_u16(body, &mut offset)?;
    let mut subscriptions = Vec::new();
    while offset < body.len() {
        let topic = read_mqtt_string(body, &mut offset)?;
        let qos = *body.get(offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "short MQTT subscribe payload")
        })?;
        offset += 1;
        subscriptions.push((topic, qos.min(2)));
    }
    if subscriptions.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty MQTT subscription",
        ));
    }
    Ok((id, subscriptions))
}

fn read_mqtt_u16(body: &[u8], offset: &mut usize) -> io::Result<u16> {
    let bytes = body
        .get(*offset..*offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short MQTT two-byte value"))?;
    *offset += 2;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_mqtt_string(body: &[u8], offset: &mut usize) -> io::Result<String> {
    let length = read_mqtt_u16(body, offset)? as usize;
    let bytes = body
        .get(*offset..*offset + length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short MQTT string"))?;
    *offset += length;
    String::from_utf8(bytes.to_vec())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_mqtt_string(body: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let length = u16::try_from(value.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "MQTT string too long"))?;
    body.extend_from_slice(&length.to_be_bytes());
    body.extend_from_slice(value.as_bytes());
    Ok(())
}

fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut filter_parts = filter.split('/');
    let mut topic_parts = topic.split('/');
    loop {
        match filter_parts.next() {
            Some("#") => return filter_parts.next().is_none(),
            Some("+") => {
                if topic_parts.next().is_none() {
                    return false;
                }
            }
            Some(value) => {
                if topic_parts.next() != Some(value) {
                    return false;
                }
            }
            None => return topic_parts.next().is_none(),
        }
    }
}

struct Server {
    child: Child,
    url: String,
    client: Client,
    _stdout: JoinHandle<()>,
}

impl Server {
    async fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hass"))
            .arg("--config")
            .arg(config)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .env("HASS_TOKEN", TOKEN)
            .env_remove("HASS_TOKEN_FILE")
            .env("RUST_LOG", "hass=info")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("compiled hass process should start");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("Rust runtime listening")
                    && let Some((_, address)) = line.split_once("127.0.0.1:")
                {
                    let port: String = address.chars().take_while(char::is_ascii_digit).collect();
                    return port.parse::<u16>().unwrap();
                }
            }
            panic!(
                "compiled server exited before announcing its listener: {:?}",
                child.wait().await.unwrap()
            );
        })
        .await
        .expect("compiled server did not announce its listener");
        let stdout =
            tokio::spawn(async move { while matches!(lines.next_line().await, Ok(Some(_))) {} });
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if let Ok(response) = client.get(format!("{url}/health")).send().await {
                assert_eq!(response.status(), StatusCode::OK);
                return Self {
                    child,
                    url,
                    client,
                    _stdout: stdout,
                };
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("compiled server exited before readiness: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("compiled server did not become ready");
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
    }

    async fn wait_state(&self, entity_id: &str, expected: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let response = self.get(&format!("/api/states/{entity_id}")).await;
            let last = if response.status() == StatusCode::OK {
                let value: Value = response.json().await.unwrap();
                if value["state"] == expected {
                    return value;
                }
                value
            } else {
                json!({"status": response.status().as_u16()})
            };
            assert!(
                Instant::now() < deadline,
                "{entity_id} did not reach {expected}; last response: {last}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_absent(&self, entity_id: &str, phase: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let response = self.get(&format!("/api/states/{entity_id}")).await;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return;
            }
            let last = if status == StatusCode::OK {
                response.json().await.unwrap_or(Value::Null)
            } else {
                json!({
                    "status": status.as_u16(),
                    "body": response.text().await.unwrap_or_default()
                })
            };
            assert!(
                Instant::now() < deadline,
                "{phase}: {entity_id} remained published after removal; last HTTP response: {last}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn ws_command(socket: &mut Socket, value: Value) -> Value {
        let id = value["id"]
            .as_u64()
            .expect("WebSocket command has numeric id");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                value.to_string().into(),
            ))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let frame = tokio::time::timeout_at(deadline, socket.next())
                .await
                .expect("timed out waiting for authenticated WebSocket result")
                .expect("WebSocket closed before command response")
                .expect("WebSocket command failed")
                .into_text()
                .unwrap();
            let response: Value = serde_json::from_str(&frame).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    async fn shutdown_gracefully(&mut self) {
        let pid = self.child.id().expect("server child has a process id");
        let status = StdCommand::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .expect("send SIGINT to the compiled server");
        assert!(status.success(), "failed to send SIGINT to server child");
        let status = tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("server did not finish its graceful shutdown")
            .unwrap();
        assert!(status.success(), "server shutdown returned {status}");
    }
}

async fn authenticated_socket(server: &Server) -> Socket {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "{}/api/websocket",
        server.url.replacen("http://", "ws://", 1)
    ))
    .await
    .unwrap();
    let greeting = next_json(&mut socket).await;
    assert_eq!(greeting["type"], "auth_required");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type":"auth","access_token":TOKEN})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "auth_ok");
    socket
}

async fn next_json(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(8), socket.next())
        .await
        .expect("timed out waiting for WebSocket frame")
        .expect("WebSocket closed unexpectedly")
        .expect("WebSocket frame failed")
        .into_text()
        .unwrap();
    serde_json::from_str(&frame).unwrap()
}

fn write_config(directory: &Path, broker_address: &str) -> PathBuf {
    let port = broker_address.rsplit(':').next().unwrap();
    let config = directory.join("configuration.yaml");
    std::fs::write(
        &config,
        format!(
            "recorder:\n  db_path: state.sqlite\nmqtt:\n  host: 127.0.0.1\n  port: {port}\n  client_id: compiled-process-mqtt-switch\n  qos: 1\n  discovery_prefix: homeassistant\n  max_payload_bytes: 4096\n"
        ),
    )
    .unwrap();
    config
}

fn entity_registry_entry(entries: &Value, unique_id: &str) -> Value {
    entries["result"]
        .as_array()
        .expect("entity registry list is an array")
        .iter()
        .find(|entry| entry["unique_id"] == unique_id)
        .expect("MQTT switch is present in the entity registry")
        .clone()
}

fn mqtt_config_entry(entries: &Value) -> Value {
    entries["result"]
        .as_array()
        .expect("config entry list is an array")
        .iter()
        .find(|entry| entry["domain"] == "mqtt")
        .expect("YAML import created the MQTT config entry")
        .clone()
}

#[tokio::test]
async fn compiled_process_mqtt_switch_uses_shared_lifecycle_and_quiesces() {
    let mut broker = LocalBroker::start().await;
    let directory = tempfile::tempdir().expect("temporary process directory");
    let config = write_config(directory.path(), &broker.address);
    let database = directory.path().join("state.sqlite");

    let mut server = Server::start(&config).await;
    assert!(
        database.exists(),
        "compiled process opened its owned SQLite file"
    );
    let mut socket = authenticated_socket(&server).await;
    let imported = Server::ws_command(
        &mut socket,
        json!({"id":1,"type":"config_entries/get","domain":"mqtt"}),
    )
    .await;
    assert_eq!(imported["success"], true);
    let imported_entry = mqtt_config_entry(&imported);
    assert_eq!(
        imported_entry["state"], "loaded",
        "the main process imports and sets up its configured MQTT entry: {imported_entry}"
    );
    broker.wait_for_subscription("homeassistant/#").await;

    broker.publish(
        DISCOVERY_TOPIC,
        br#"{"name":"Kitchen Heater","unique_id":"compiled-process-switch-unique-id","state_topic":"process/heater/state","command_topic":"process/heater/set","command_template":"{\"entity_id\":\"{{ entity_id }}\",\"value\":\"{{ value }}\"}","availability_topic":"process/heater/availability","payload_on":"ENABLED","payload_off":"DISABLED","state_on":"ENABLED","state_off":"DISABLED","optimistic":true,"device_class":"OUTLET","device":{"identifiers":["compiled-process-switch-device"],"name":"Compiled Process Fixture"}}"#.to_vec(),
        true,
    );
    broker
        .wait_for_subscriptions(&[STATE_TOPIC, AVAILABILITY_TOPIC])
        .await;

    let initial_entries = Server::ws_command(
        &mut socket,
        json!({"id":2,"type":"config/entity_registry/list"}),
    )
    .await;
    assert_eq!(initial_entries["success"], true);
    let initial_entry = entity_registry_entry(&initial_entries, UNIQUE_ID);
    let original_entity_id = initial_entry["entity_id"]
        .as_str()
        .expect("registered switch has an entity id")
        .to_owned();
    assert!(original_entity_id.starts_with("switch."));

    let unavailable = server.wait_state(&original_entity_id, "unavailable").await;
    assert_eq!(unavailable["attributes"]["friendly_name"], "Kitchen Heater");
    assert_eq!(unavailable["attributes"]["device_class"], "outlet");
    assert_eq!(unavailable["attributes"]["assumed_state"], true);

    broker.publish(AVAILABILITY_TOPIC, b"online".to_vec(), true);
    broker.publish(STATE_TOPIC, b"DISABLED".to_vec(), true);
    let off = server.wait_state(&original_entity_id, "off").await;
    assert_eq!(off["attributes"]["assumed_state"], true);

    let rename = Server::ws_command(
        &mut socket,
        json!({
            "id":3,
            "type":"config/entity_registry/update",
            "entity_id":original_entity_id,
            "new_entity_id":RENAMED_ENTITY_ID,
            "name":"Workshop Heater"
        }),
    )
    .await;
    assert_eq!(rename["success"], true);
    server
        .wait_absent(&original_entity_id, "old ID after registry rename")
        .await;
    let renamed = server.wait_state(RENAMED_ENTITY_ID, "off").await;
    assert_eq!(renamed["attributes"]["friendly_name"], "Workshop Heater");
    assert_eq!(renamed["attributes"]["device_class"], "outlet");
    assert_eq!(renamed["attributes"]["assumed_state"], true);

    let call = Server::ws_command(
        &mut socket,
        json!({
            "id":4,
            "type":"call_service",
            "domain":"switch",
            "service":"turn_on",
            "target":{"entity_id":RENAMED_ENTITY_ID}
        }),
    )
    .await;
    assert_eq!(
        call["success"], true,
        "renamed switch accepts a targeted service call: {call}"
    );
    assert!(
        call["result"]["context"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    let published = broker
        .wait_for(
            |event| matches!(event, BrokerEvent::Published { topic, .. } if topic == COMMAND_TOPIC),
        )
        .await;
    match published {
        BrokerEvent::Published {
            payload,
            qos,
            retain,
            ..
        } => {
            assert_eq!(qos, 1);
            assert!(!retain);
            assert_eq!(
                serde_json::from_slice::<Value>(&payload).unwrap(),
                json!({
                    "entity_id":RENAMED_ENTITY_ID,
                    "value":"ENABLED"
                }),
                "actual broker publication carries the renamed target and requested state"
            );
        }
        _ => unreachable!(),
    }
    let on = server.wait_state(RENAMED_ENTITY_ID, "on").await;
    assert_eq!(on["attributes"]["assumed_state"], true);

    let generic_off = Server::ws_command(
        &mut socket,
        json!({
            "id":5,
            "type":"call_service",
            "domain":"homeassistant",
            "service":"turn_off",
            "target":{"entity_id":RENAMED_ENTITY_ID}
        }),
    )
    .await;
    assert_eq!(
        generic_off["success"], true,
        "generic control awaits the MQTT provider: {generic_off}"
    );
    let generic_command = broker
        .wait_for(
            |event| matches!(event, BrokerEvent::Published { topic, .. } if topic == COMMAND_TOPIC),
        )
        .await;
    match generic_command {
        BrokerEvent::Published {
            payload,
            qos,
            retain,
            ..
        } => {
            assert_eq!(qos, 1);
            assert!(!retain);
            assert_eq!(
                serde_json::from_slice::<Value>(&payload).unwrap(),
                json!({"entity_id":RENAMED_ENTITY_ID,"value":"DISABLED"})
            );
        }
        _ => unreachable!(),
    }
    let off = server.wait_state(RENAMED_ENTITY_ID, "off").await;
    assert_eq!(off["context"]["id"], generic_off["result"]["context"]["id"]);

    let http_on = server
        .client
        .post(format!("{}/api/services/switch/turn_on", server.url))
        .bearer_auth(TOKEN)
        .json(&json!({"target":{"entity_id":RENAMED_ENTITY_ID}}))
        .send()
        .await
        .unwrap();
    assert_eq!(http_on.status(), StatusCode::OK);
    let changed: Vec<Value> = http_on.json().await.unwrap();
    assert_eq!(
        changed.len(),
        1,
        "HTTP result reports the actual optimistic state change: {changed:?}"
    );
    assert_eq!(changed[0]["entity_id"], RENAMED_ENTITY_ID);
    assert_eq!(changed[0]["state"], "on");
    let http_command = broker
        .wait_for(
            |event| matches!(event, BrokerEvent::Published { topic, .. } if topic == COMMAND_TOPIC),
        )
        .await;
    match http_command {
        BrokerEvent::Published {
            payload,
            qos,
            retain,
            ..
        } => {
            assert_eq!(qos, 1);
            assert!(!retain);
            assert_eq!(
                serde_json::from_slice::<Value>(&payload).unwrap(),
                json!({"entity_id":RENAMED_ENTITY_ID,"value":"ENABLED"})
            );
        }
        _ => unreachable!(),
    }
    let http_state = server.wait_state(RENAMED_ENTITY_ID, "on").await;
    assert_eq!(http_state["context"], changed[0]["context"]);

    let disabled = Server::ws_command(
        &mut socket,
        json!({
            "id":6,
            "type":"config/entity_registry/update",
            "entity_id":RENAMED_ENTITY_ID,
            "disabled_by":"user"
        }),
    )
    .await;
    assert_eq!(
        disabled["success"], true,
        "entity disable response: {disabled}"
    );
    let disabled_entries = Server::ws_command(
        &mut socket,
        json!({"id":7,"type":"config/entity_registry/list"}),
    )
    .await;
    assert_eq!(
        entity_registry_entry(&disabled_entries, UNIQUE_ID)["disabled_by"],
        "user",
        "the registry readback confirms the disable reached persistent registry state"
    );
    server
        .wait_absent(RENAMED_ENTITY_ID, "registry-disabled switch")
        .await;
    let blocked = Server::ws_command(
        &mut socket,
        json!({
            "id":8,
            "type":"call_service",
            "domain":"switch",
            "service":"turn_off",
            "target":{"entity_id":RENAMED_ENTITY_ID}
        }),
    )
    .await;
    assert_eq!(
        blocked["success"], false,
        "disabled entity cannot be targeted: {blocked}"
    );
    broker
        .assert_no_command_for(Duration::from_millis(150))
        .await;

    let enabled = Server::ws_command(
        &mut socket,
        json!({
            "id":9,
            "type":"config/entity_registry/update",
            "entity_id":RENAMED_ENTITY_ID,
            "disabled_by":null
        }),
    )
    .await;
    assert_eq!(enabled["success"], true);
    broker.publish(STATE_TOPIC, b"DISABLED".to_vec(), true);
    server.wait_state(RENAMED_ENTITY_ID, "off").await;

    let config_entries = Server::ws_command(
        &mut socket,
        json!({"id":10,"type":"config_entries/get","domain":"mqtt"}),
    )
    .await;
    assert_eq!(config_entries["success"], true);
    let mqtt_entry = mqtt_config_entry(&config_entries);
    assert_eq!(mqtt_entry["state"], "loaded");
    let entry_id = mqtt_entry["entry_id"].as_str().unwrap();
    let unloaded = Server::ws_command(
        &mut socket,
        json!({
            "id":11,
            "type":"config_entries/disable",
            "entry_id":entry_id,
            "disabled_by":"user"
        }),
    )
    .await;
    assert_eq!(
        unloaded["success"], true,
        "MQTT config entry unload succeeds: {unloaded}"
    );
    broker.wait_for_disconnect().await;
    let unloaded_entities = Server::ws_command(
        &mut socket,
        json!({"id":12,"type":"config/entity_registry/list"}),
    )
    .await;
    assert_eq!(
        entity_registry_entry(&unloaded_entities, UNIQUE_ID)["disabled_by"],
        "config_entry",
        "disabling the MQTT config entry disables its registered switch"
    );
    server
        .wait_absent(RENAMED_ENTITY_ID, "config-entry disabled switch")
        .await;
    let after_unload = Server::ws_command(
        &mut socket,
        json!({
            "id":13,
            "type":"call_service",
            "domain":"switch",
            "service":"turn_on",
            "target":{"entity_id":RENAMED_ENTITY_ID}
        }),
    )
    .await;
    assert_eq!(
        after_unload["success"], false,
        "unloaded entry cannot target its switch"
    );
    broker
        .assert_no_command_for(Duration::from_millis(150))
        .await;

    let reloaded = Server::ws_command(
        &mut socket,
        json!({
            "id":14,
            "type":"config_entries/disable",
            "entry_id":entry_id,
            "disabled_by":null
        }),
    )
    .await;
    assert_eq!(
        reloaded["success"], true,
        "re-enabled MQTT entry loads again: {reloaded}"
    );
    broker.wait_for_subscription("homeassistant/#").await;
    server.wait_state(RENAMED_ENTITY_ID, "off").await;
    socket.close(None).await.unwrap();

    server.shutdown_gracefully().await;
    broker.wait_for_disconnect().await;
    broker
        .assert_no_command_for(Duration::from_millis(200))
        .await;

    let mut restarted = Server::start(&config).await;
    broker.wait_for_subscription("homeassistant/#").await;
    let restored = restarted.wait_state(RENAMED_ENTITY_ID, "off").await;
    assert_eq!(restored["attributes"]["friendly_name"], "Workshop Heater");
    assert_eq!(restored["attributes"]["device_class"], "outlet");
    assert_eq!(restored["attributes"]["assumed_state"], true);
    assert_eq!(
        restarted
            .get(&format!("/api/states/{original_entity_id}"))
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "the old generated ID stays absent after SQLite-backed restart"
    );
    let mut restarted_socket = authenticated_socket(&restarted).await;
    let restored_entries = Server::ws_command(
        &mut restarted_socket,
        json!({"id":1,"type":"config/entity_registry/list"}),
    )
    .await;
    let restored_entry = entity_registry_entry(&restored_entries, UNIQUE_ID);
    assert_eq!(restored_entry["id"], initial_entry["id"]);
    assert_eq!(restored_entry["entity_id"], RENAMED_ENTITY_ID);
    assert_eq!(restored_entry["name"], "Workshop Heater");
    assert!(restored_entry["disabled_by"].is_null());
    restarted_socket.close(None).await.unwrap();
    restarted.shutdown_gracefully().await;
    broker.wait_for_disconnect().await;
}
