//! Draft loopback fixtures for the USB request-response WebSocket commands.
//!
//! This file stays under `tests/drafts` until the application enrolls the USB
//! SDK and the API dispatcher wires its USB feature.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hass_api::{
    ApiFeatures, ApiState, UsbDiscoveryFlowUsage, UsbUsageClaim, UsbUsageProvider,
    UsbUsageSnapshot, router_with_features,
};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_usb::{
    ScheduledTask, SerialNumberPolicy, SerialPortConsumer, SerialPortConsumerKind, SerialPortInfo,
    SerialPortProvider, TimerScheduler, USBMatcher, UsbError, UsbIntegration, UsbManager,
    UsbOptions,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener as TokioTcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "usb-protocol-fixture";
type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
}

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime")
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "USB Owner", "usb-owner-password")
        .expect("create owner");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "usb-user-password",
            false,
        )
        .expect("create ordinary user");
    Tokens {
        owner: auth
            .issue_tokens(&owner.id, CLIENT_ID, None)
            .expect("issue owner token")
            .access_token,
        ordinary: auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .expect("issue ordinary token")
            .access_token,
        auth,
    }
}

fn router(
    runtime: Runtime,
    auth: &AuthManager,
    usb: UsbManager,
    usb_usage: Option<Arc<dyn UsbUsageProvider>>,
) -> Router {
    router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            usb: Some(usb),
            usb_usage,
            ..ApiFeatures::default()
        },
        None,
    )
}

async fn start_server(router: Router) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = TokioTcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback API server");
    let address = listener.local_addr().expect("API server address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    (address, server)
}

async fn receive(socket: &mut Socket) -> Value {
    loop {
        let frame = timeout(Duration::from_secs(4), socket.next())
            .await
            .expect("WebSocket response timed out")
            .expect("WebSocket closed before response")
            .expect("WebSocket receive failed");
        match frame {
            Message::Text(text) => return serde_json::from_str(&text).expect("JSON response"),
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .expect("reply to WebSocket ping"),
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send WebSocket command");
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .expect("connect WebSocket");
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    send(&mut socket, json!({"type": "auth", "access_token": token})).await;
    assert_eq!(receive(&mut socket).await["type"], "auth_ok");
    socket
}

fn assert_ack(frame: &Value, id: u64) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(
        frame["success"], true,
        "expected successful result: {frame}"
    );
    assert_eq!(frame["result"], Value::Null);
}

fn assert_error(frame: &Value, id: u64, code: &str) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], false);
    assert_eq!(frame["error"]["code"], code);
}

struct NoopScheduler;
struct NoopTask;

impl ScheduledTask for NoopTask {
    fn cancel(&self) {}
}

impl TimerScheduler for NoopScheduler {
    fn schedule(
        &self,
        _delay: Duration,
        _callback: Box<dyn FnOnce() + Send + 'static>,
    ) -> Box<dyn ScheduledTask> {
        Box::new(NoopTask)
    }
}

fn usb_owner(
    ports: impl Fn() -> Result<Vec<SerialPortInfo>, UsbError> + Send + Sync + 'static,
    matchers: Vec<USBMatcher>,
) -> (UsbIntegration, UsbManager) {
    let provider: Arc<dyn SerialPortProvider> = Arc::new(ports);
    let scheduler: Arc<dyn TimerScheduler> = Arc::new(NoopScheduler);
    let serial_policy: Arc<dyn SerialNumberPolicy> =
        Arc::new(|serial_number: Option<&str>| serial_number.is_none());
    let owner = UsbIntegration::new(
        provider,
        scheduler,
        UsbOptions::new(matchers),
        serial_policy,
    );
    let manager = owner.manager();
    (owner, manager)
}

fn test_matchers() -> Vec<USBMatcher> {
    vec![USBMatcher::new("test_usb")]
}

fn port(device: &str, resolved: &str, usb: bool) -> SerialPortInfo {
    SerialPortInfo {
        device: device.to_owned(),
        resolved_device: Some(resolved.to_owned()),
        vid: usb.then_some(0x10c4),
        pid: usb.then_some(0xea60),
        serial_number: Some("001234".to_owned()),
        manufacturer: Some("Silicon Labs".to_owned()),
        description: Some("CP2102 USB to UART".to_owned()),
        bcd_device: Some(0x0101),
        interface_description: Some("CP2102 USB to UART Bridge".to_owned()),
        interface_num: Some(0),
    }
}

fn consumer(
    kind: SerialPortConsumerKind,
    title: &str,
    active: bool,
    domain: Option<&str>,
    config_entry_id: Option<&str>,
    slug: Option<&str>,
) -> SerialPortConsumer {
    SerialPortConsumer {
        kind,
        title: title.to_owned(),
        active,
        domain: domain.map(str::to_owned),
        config_entry_id: config_entry_id.map(str::to_owned),
        slug: slug.map(str::to_owned),
    }
}

#[derive(Default)]
struct FixtureUsage {
    snapshot: UsbUsageSnapshot,
    realpaths: HashMap<String, String>,
    overrides: Vec<String>,
}

impl UsbUsageProvider for FixtureUsage {
    fn snapshot(&self) -> UsbUsageSnapshot {
        self.snapshot.clone()
    }

    fn realpath(&self, path: &str) -> String {
        self.realpaths
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_owned())
    }

    fn custom_component_domains(&self) -> Vec<String> {
        self.overrides.clone()
    }
}

#[tokio::test]
async fn all_usb_commands_require_admin_and_exact_request_shapes() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (_owner, manager) = usb_owner(|| Ok(Vec::new()), test_matchers());
    let (address, server) =
        start_server(router(runtime, &tokens.auth, manager.clone(), None)).await;

    let mut ordinary = connect(address, &tokens.ordinary).await;
    for (id, command) in [
        (1, "usb/scan"),
        (2, "usb/list_serial_ports"),
        (3, "usb/list_serial_integrations"),
    ] {
        send(&mut ordinary, json!({"id": id, "type": command})).await;
        assert_error(&receive(&mut ordinary).await, id, "unauthorized");
    }

    let mut admin = connect(address, &tokens.owner).await;
    send(
        &mut admin,
        json!({"id": 4, "type": "usb/scan", "include_usage": true}),
    )
    .await;
    assert_error(&receive(&mut admin).await, 4, "invalid_format");
    send(
        &mut admin,
        json!({"id": 5, "type": "usb/list_serial_ports", "include_usage": "yes"}),
    )
    .await;
    assert_error(&receive(&mut admin).await, 5, "invalid_format");
    send(
        &mut admin,
        json!({"id": 6, "type": "usb/list_serial_integrations", "include_usage": false}),
    )
    .await;
    assert_error(&receive(&mut admin).await, 6, "invalid_format");
    send(&mut admin, json!({"id": 7, "type": "usb/scan"})).await;
    assert_ack(&receive(&mut admin).await, 7);

    server.abort();
}

#[tokio::test]
async fn list_serial_ports_uses_fresh_usb_and_non_usb_records_without_discovery_scan() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = calls.clone();
    let (owner, manager) = usb_owner(
        move || {
            let call = calls_for_provider.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(vec![
                    port("/dev/serial/by-id/usb-first", "/dev/ttyUSB0", true),
                    port("/dev/ttyS0", "/dev/ttyS0", false),
                ])
            } else {
                Ok(vec![port("/dev/ttyUSB1", "/dev/ttyUSB1", true)])
            }
        },
        test_matchers(),
    );
    let (address, server) =
        start_server(router(runtime, &tokens.auth, manager.clone(), None)).await;
    let mut admin = connect(address, &tokens.owner).await;

    send(
        &mut admin,
        json!({"id": 1, "type": "usb/list_serial_ports"}),
    )
    .await;
    let first = receive(&mut admin).await["result"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["device"], "/dev/serial/by-id/usb-first");
    assert_eq!(first[0]["resolved_device"], "/dev/ttyUSB0");
    assert_eq!(first[0]["vid"], "10C4");
    assert_eq!(first[0]["pid"], "EA60");
    assert_eq!(first[0]["bcd_device"], 257);
    assert_eq!(first[0]["matching_integrations"], json!(["test_usb"]));
    assert_eq!(first[0]["present"], true);
    assert!(first[0].get("consumers").is_none());
    assert!(first[0].get("discovery_flows").is_none());
    assert_eq!(first[1]["device"], "/dev/ttyS0");
    assert!(first[1].get("vid").is_none());
    assert!(first[1].get("pid").is_none());
    assert_eq!(first[1]["matching_integrations"], json!([]));

    send(
        &mut admin,
        json!({"id": 2, "type": "usb/list_serial_ports"}),
    )
    .await;
    let second = receive(&mut admin).await["result"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["device"], "/dev/ttyUSB1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Listing reads a new provider snapshot, but does not run the USB
    // discovery scan path or mark its initial-scan lifecycle complete.
    assert!(!manager.initial_scan_done().unwrap());
    drop(owner);
    server.abort();
}

#[tokio::test]
async fn usb_scan_observer_suppression_and_manual_scan_request_are_preserved() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_provider = calls.clone();
    let (_owner, manager) = usb_owner(
        move || {
            calls_for_provider.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        },
        test_matchers(),
    );
    manager.set_observer_active(true).unwrap();
    let (address, server) =
        start_server(router(runtime, &tokens.auth, manager.clone(), None)).await;
    let mut admin = connect(address, &tokens.owner).await;

    send(&mut admin, json!({"id": 1, "type": "usb/scan"})).await;
    assert_ack(&receive(&mut admin).await, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    manager.set_observer_active(false).unwrap();
    send(&mut admin, json!({"id": 2, "type": "usb/scan"})).await;
    assert_ack(&receive(&mut admin).await, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(manager.initial_scan_done().unwrap());

    server.abort();
}

#[tokio::test]
async fn usage_maps_symlinks_keeps_missing_and_url_claims_and_redacts_psk() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (owner, manager) = usb_owner(
        || {
            Ok(vec![port(
                "/dev/serial/by-id/usb-test",
                "/dev/ttyACM0",
                true,
            )])
        },
        test_matchers(),
    );
    let usage = Arc::new(FixtureUsage {
        snapshot: UsbUsageSnapshot {
            consumers: vec![
                UsbUsageClaim {
                    path: "/dev/serial/by-id/usb-other-alias".to_owned(),
                    consumer: consumer(
                        SerialPortConsumerKind::ConfigEntry,
                        "Test USB",
                        true,
                        Some("test_usb"),
                        Some("entry-1"),
                        None,
                    ),
                },
                UsbUsageClaim {
                    path: "serial:///dev/ttyMISSING:115200".to_owned(),
                    consumer: consumer(
                        SerialPortConsumerKind::ConfigEntry,
                        "Missing device",
                        false,
                        Some("upb"),
                        Some("entry-2"),
                        None,
                    ),
                },
                UsbUsageClaim {
                    path: "esphome://192.0.2.5:6053/?key=secret-psk".to_owned(),
                    consumer: consumer(
                        SerialPortConsumerKind::ConfigEntry,
                        "Z-Wave JS",
                        true,
                        Some("zwave_js"),
                        Some("entry-3"),
                        None,
                    ),
                },
                UsbUsageClaim {
                    path: "esphome-hass://serial/proxy-1".to_owned(),
                    consumer: consumer(
                        SerialPortConsumerKind::ConfigEntry,
                        "ESPHome proxy",
                        true,
                        Some("esphome"),
                        Some("entry-4"),
                        None,
                    ),
                },
            ],
            discovery_flows: vec![UsbDiscoveryFlowUsage {
                device: "/dev/serial/by-id/usb-test".to_owned(),
                flow_id: "flow-usb-1".to_owned(),
                domain: "test_usb".to_owned(),
            }],
        },
        realpaths: HashMap::from([
            ("/dev/ttyACM0".to_owned(), "/dev/ttyACM0".to_owned()),
            (
                "/dev/serial/by-id/usb-other-alias".to_owned(),
                "/dev/ttyACM0".to_owned(),
            ),
            ("/dev/ttyMISSING".to_owned(), "/dev/ttyMISSING".to_owned()),
        ]),
        overrides: Vec::new(),
    });
    let (address, server) =
        start_server(router(runtime, &tokens.auth, manager, Some(usage.clone()))).await;
    let mut admin = connect(address, &tokens.owner).await;

    send(
        &mut admin,
        json!({"id": 1, "type": "usb/list_serial_ports", "include_usage": true}),
    )
    .await;
    let result = receive(&mut admin).await["result"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(result.len(), 4);
    let scanned = &result[0];
    assert_eq!(scanned["device"], "/dev/serial/by-id/usb-test");
    assert_eq!(
        scanned["consumers"],
        json!([{
            "kind": "config_entry",
            "title": "Test USB",
            "active": true,
            "domain": "test_usb",
            "config_entry_id": "entry-1",
            "slug": null,
        }])
    );
    assert_eq!(
        scanned["discovery_flows"],
        json!([{"flow_id": "flow-usb-1", "domain": "test_usb"}])
    );

    let missing = &result[1];
    assert_eq!(missing["device"], "/dev/ttyMISSING");
    assert_eq!(missing["resolved_device"], Value::Null);
    assert_eq!(missing["present"], false);
    assert_eq!(missing["consumers"][0]["config_entry_id"], "entry-2");

    let esphome = &result[2];
    assert_eq!(esphome["device"], "esphome://192.0.2.5:6053/");
    assert_eq!(esphome["present"], true);
    assert_eq!(esphome["consumers"][0]["domain"], "zwave_js");

    let hass_proxy = &result[3];
    assert_eq!(hass_proxy["device"], "esphome-hass://serial/proxy-1");
    assert_eq!(hass_proxy["present"], false);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("secret-psk")
    );
    drop(owner);
    server.abort();
}

#[tokio::test]
async fn list_serial_ports_maps_provider_errors_to_unknown_error_verbatim() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (_owner, manager) = usb_owner(
        || Err(UsbError::new("Permission denied: fixture provider")),
        test_matchers(),
    );
    let (address, server) = start_server(router(runtime, &tokens.auth, manager, None)).await;
    let mut admin = connect(address, &tokens.owner).await;

    send(
        &mut admin,
        json!({"id": 1, "type": "usb/list_serial_ports"}),
    )
    .await;
    let response = receive(&mut admin).await;
    assert_error(&response, 1, "unknown_error");
    assert_eq!(
        response["error"]["message"],
        "Permission denied: fixture provider"
    );

    server.abort();
}

#[tokio::test]
async fn selector_integrations_keep_generated_order_and_filter_custom_overrides() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (_owner, manager) = usb_owner(|| Ok(Vec::new()), test_matchers());
    let overridden = hass_usb::SERIAL_PORT_SELECTOR_INTEGRATIONS[0].to_owned();
    let usage = Arc::new(FixtureUsage {
        overrides: vec![overridden.clone()],
        ..FixtureUsage::default()
    });
    let (address, server) = start_server(router(runtime, &tokens.auth, manager, Some(usage))).await;
    let mut admin = connect(address, &tokens.owner).await;

    send(
        &mut admin,
        json!({"id": 1, "type": "usb/list_serial_integrations"}),
    )
    .await;
    let result = receive(&mut admin).await["result"]
        .as_array()
        .unwrap()
        .clone();
    let expected: Vec<_> = hass_usb::SERIAL_PORT_SELECTOR_INTEGRATIONS
        .iter()
        .filter(|domain| **domain != overridden.as_str())
        .map(|domain| json!(domain))
        .collect();
    assert_eq!(result, expected);
    let expected_domains: Vec<_> = hass_usb::SERIAL_PORT_SELECTOR_INTEGRATIONS
        .iter()
        .filter(|domain| **domain != overridden.as_str())
        .copied()
        .collect();
    assert!(expected_domains.windows(2).all(|pair| pair[0] < pair[1]));

    server.abort();
}

#[tokio::test]
async fn usb_api_keeps_only_a_weak_manager_and_reports_owner_teardown() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (owner, manager) = usb_owner(|| Ok(Vec::new()), test_matchers());
    let (address, server) = start_server(router(runtime, &tokens.auth, manager, None)).await;
    drop(owner);

    let mut admin = connect(address, &tokens.owner).await;
    send(
        &mut admin,
        json!({"id": 1, "type": "usb/list_serial_ports"}),
    )
    .await;
    let response = receive(&mut admin).await;
    assert_error(&response, 1, "unknown_error");
    assert_eq!(response["error"]["message"], "USB integration is stopped");

    server.abort();
}
