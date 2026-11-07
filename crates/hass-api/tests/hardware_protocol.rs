//! Loopback protocol fixtures for the hardware WebSocket commands.
//!
//! These fixtures use only an injected metrics provider and synthetic provider
//! rows; they never read host hardware.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_hardware::{
    BoardInfo, HardwareConfig, HardwareInfo, HardwareIntegration, HardwareManager,
    RawSystemMetrics, USBInfo,
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener as TokioTcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "hardware-protocol-fixture";
type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Server = JoinHandle<std::io::Result<()>>;

#[derive(Clone)]
struct Tokens {
    auth: AuthManager,
    owner_user_id: String,
    ordinary: String,
    ordinary_user_id: String,
    reader: String,
}

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime")
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "Hardware Owner", "hardware-owner-password")
        .expect("create owner");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "hardware-user-password",
            false,
        )
        .expect("create ordinary user");
    let reader = auth
        .create_user_without_credentials(
            &owner.id,
            "Read-only User",
            Some(&["system-read-only".to_owned()]),
            None,
        )
        .expect("create read-only user");
    Tokens {
        owner_user_id: owner.id,
        ordinary: auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .expect("issue ordinary-user token")
            .access_token,
        ordinary_user_id: ordinary.id,
        reader: auth
            .issue_tokens(&reader.id, CLIENT_ID, None)
            .expect("issue reader token")
            .access_token,
        auth,
    }
}

fn hardware_integration(
    config: Option<HardwareConfig>,
) -> (HardwareIntegration, HardwareManager, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let provider_reads = Arc::clone(&reads);
    let metrics: Arc<dyn hass_hardware::MetricsProvider> = Arc::new(move || {
        provider_reads.fetch_add(1, Ordering::SeqCst);
        Ok(RawSystemMetrics {
            cpu_percent: 123.4,
            total_bytes: 30 * 1024 * 1024,
            available_bytes: 10 * 1024 * 1024,
            memory_used_percent: 50.0,
        })
    });
    let integration = match config {
        Some(config) => HardwareIntegration::with_config(metrics, config),
        None => HardwareIntegration::new(metrics),
    }
    .expect("create hardware integration with fake metrics");
    let manager = integration.manager();
    (integration, manager, reads)
}

fn router(runtime: Runtime, auth: &AuthManager, hardware: HardwareManager) -> Router {
    router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            hardware: Some(hardware),
            ..ApiFeatures::default()
        },
        None,
    )
}

async fn start_server(router: Router) -> (SocketAddr, Server) {
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

async fn next_json(socket: &mut Socket) -> Value {
    loop {
        match socket
            .next()
            .await
            .expect("WebSocket closed")
            .expect("WebSocket receive")
        {
            Message::Text(text) => return serde_json::from_str(&text).expect("JSON response"),
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .expect("reply to WebSocket ping"),
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn receive(socket: &mut Socket) -> Value {
    timeout(Duration::from_secs(4), next_json(socket))
        .await
        .expect("WebSocket response timed out")
}

async fn receive_status(socket: &mut Socket) -> Value {
    timeout(Duration::from_secs(7), next_json(socket))
        .await
        .expect("system-status event timed out")
}

async fn assert_no_frame(socket: &mut Socket, duration: Duration) {
    let received = timeout(duration, socket.next()).await;
    assert!(
        received.is_err(),
        "received a frame before the first sample: {received:?}"
    );
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

async fn assert_connection_closed(socket: &mut Socket) {
    let result = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("WebSocket did not close");
    assert!(matches!(
        result,
        None | Some(Ok(Message::Close(_))) | Some(Err(_))
    ));
}

fn assert_ack(frame: &Value, id: u64) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], true, "unexpected response: {frame}");
    assert_eq!(frame["result"], Value::Null);
}

fn assert_error(frame: &Value, id: u64, code: &str) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], false);
    assert_eq!(frame["error"]["code"], code);
}

async fn stop_server(server: Server, integration: &mut HardwareIntegration) {
    server.abort();
    let _ = server.await;
    integration
        .shutdown()
        .await
        .expect("stop fake hardware integration");
}

fn test_hardware_row() -> HardwareInfo {
    HardwareInfo {
        name: Some("Fixture board".to_owned()),
        board: Some(BoardInfo {
            hassio_board_id: None,
            manufacturer: "fixture-maker".to_owned(),
            model: None,
            revision: None,
        }),
        config_entries: Some(vec!["entry-1".to_owned()]),
        dongle: Some(USBInfo {
            vid: "10c4".to_owned(),
            pid: "ea60".to_owned(),
            serial_number: None,
            manufacturer: Some("USB fixture".to_owned()),
            description: None,
        }),
        url: None,
    }
}

#[tokio::test]
async fn info_returns_an_empty_list_without_providers_to_an_authenticated_user() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, _) = hardware_integration(None);
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.ordinary).await;

    send(
        &mut socket,
        json!({"id": 1, "type": "hardware/info", "unexpected": true}),
    )
    .await;
    assert_error(&receive(&mut socket).await, 1, "invalid_format");
    send(
        &mut socket,
        json!({
            "id": 2,
            "type": "hardware/subscribe_system_status",
            "unexpected": true
        }),
    )
    .await;
    assert_error(&receive(&mut socket).await, 2, "invalid_format");

    send(&mut socket, json!({"id": 3, "type": "hardware/info"})).await;
    let response = receive(&mut socket).await;
    assert_eq!(response["id"], 3);
    assert_eq!(response["success"], true);
    assert_eq!(response["result"], json!({"hardware": []}));

    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn info_preserves_typed_nullable_fields_and_omits_only_failed_providers() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, _) = hardware_integration(None);
    let _failed = manager
        .register_provider(
            "failed_fixture",
            Arc::new(|| -> hass_core::Result<Vec<HardwareInfo>> {
                Err(hass_core::Error::Service("fixture unavailable".to_owned()))
            }),
        )
        .expect("register failing provider");
    let _record = manager
        .register_provider(
            "valid_fixture",
            Arc::new(|| -> hass_core::Result<Vec<HardwareInfo>> { Ok(vec![test_hardware_row()]) }),
        )
        .expect("register valid provider");
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.reader).await;

    send(&mut socket, json!({"id": 1, "type": "hardware/info"})).await;
    let response = receive(&mut socket).await;
    assert_eq!(response["success"], true);
    assert_eq!(
        response["result"],
        json!({
            "hardware": [{
                "name": "Fixture board",
                "board": {
                    "hassio_board_id": null,
                    "manufacturer": "fixture-maker",
                    "model": null,
                    "revision": null,
                },
                "config_entries": ["entry-1"],
                "dongle": {
                    "vid": "10c4",
                    "pid": "ea60",
                    "serial_number": null,
                    "manufacturer": "USB fixture",
                    "description": null,
                },
                "url": null,
            }]
        })
    );

    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn status_waits_for_the_five_second_sample_and_sends_one_private_event() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, reads) = hardware_integration(None);
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.ordinary).await;

    send(
        &mut socket,
        json!({"id": 9, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 9);
    assert_no_frame(&mut socket, Duration::from_millis(250)).await;
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no immediate metrics read");

    let event = receive_status(&mut socket).await;
    assert_eq!(event["id"], 9);
    assert_eq!(event["type"], "event");
    assert_eq!(event["event"]["cpu_percent"], 123);
    assert_eq!(event["event"]["memory_used_percent"], 50.0);
    assert_eq!(event["event"]["memory_used_mb"], 20.0);
    assert_eq!(event["event"]["memory_free_mb"], 10.0);
    assert_eq!(event["event"].as_object().unwrap().len(), 5);
    let timestamp = chrono::DateTime::parse_from_rfc3339(
        event["event"]["timestamp"]
            .as_str()
            .expect("UTC timestamp string"),
    )
    .expect("ISO timestamp");
    assert_eq!(timestamp.offset().local_minus_utc(), 0);
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn subscribers_share_one_sampler_and_the_last_unsubscribe_stops_reads() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, reads) = hardware_integration(Some(HardwareConfig {
        sample_interval: Duration::from_millis(800),
        status_channel_capacity: 32,
    }));
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut first = connect(address, &tokens.ordinary).await;
    let mut second = connect(address, &tokens.ordinary).await;

    send(
        &mut first,
        json!({"id": 1, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut first).await, 1);
    send(
        &mut second,
        json!({"id": 7, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut second).await, 7);

    let first_event = receive_status(&mut first).await;
    let second_event = receive_status(&mut second).await;
    assert_eq!(first_event["id"], 1);
    assert_eq!(second_event["id"], 7);
    assert_eq!(
        first_event["event"]["timestamp"],
        second_event["event"]["timestamp"]
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1, "one shared sample");

    send(
        &mut first,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_ack(&receive(&mut first).await, 2);
    let next_event = receive_status(&mut second).await;
    assert_eq!(next_event["id"], 7);
    assert!(reads.load(Ordering::SeqCst) >= 2);

    send(
        &mut second,
        json!({"id": 8, "type": "unsubscribe_events", "subscription": 7}),
    )
    .await;
    assert_ack(&receive(&mut second).await, 8);
    let reads_after_last_unsubscribe = reads.load(Ordering::SeqCst);
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(reads.load(Ordering::SeqCst), reads_after_last_unsubscribe);

    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn lag_reports_the_missed_count_and_removes_only_that_subscription() {
    let runtime = new_runtime();
    let event_runtime = runtime.clone();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, _) = hardware_integration(Some(HardwareConfig {
        sample_interval: Duration::from_millis(5),
        status_channel_capacity: 1,
    }));
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.ordinary).await;

    send(
        &mut socket,
        json!({"id":1,"type":"subscribe_events","event_type":"state_changed"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);

    send(
        &mut socket,
        json!({"id": 2, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 2);
    let error = receive(&mut socket).await;
    assert_error(&error, 2, "event_stream_lagged");
    let message = error["error"]["message"]
        .as_str()
        .expect("lag error message");
    assert!(message.contains("missed"), "lag count missing: {message}");
    let missed = message
        .split("missed ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|count| count.parse::<u64>().ok())
        .expect("numeric missed-event count");
    assert!(missed > 0);

    event_runtime
        .set_state(
            "sensor.hardware_fixture",
            "ok",
            serde_json::Map::new(),
            hass_core::Context::default(),
            false,
        )
        .unwrap();
    let surviving = receive(&mut socket).await;
    assert_eq!(
        surviving["id"], 1,
        "a different subscription must survive the terminal hardware error"
    );
    assert_eq!(surviving["event"]["event_type"], "state_changed");

    send(&mut socket, json!({"id": 3, "type": "ping"})).await;
    assert_eq!(receive(&mut socket).await["type"], "pong");
    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn status_delivery_revalidates_the_authenticated_session() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, _) = hardware_integration(Some(HardwareConfig {
        sample_interval: Duration::from_millis(200),
        status_channel_capacity: 32,
    }));
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.ordinary).await;

    send(
        &mut socket,
        json!({"id": 1, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    tokens
        .auth
        .revoke_user_sessions(&tokens.owner_user_id, &tokens.ordinary_user_id)
        .expect("revoke ordinary user's session");

    assert_connection_closed(&mut socket).await;
    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn revoked_session_closes_a_silent_failed_metrics_subscription() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let metrics: Arc<dyn hass_hardware::MetricsProvider> = Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Err(hass_core::Error::Service(
            "injected unavailable metrics".into(),
        ))
    });
    let mut integration = HardwareIntegration::with_config(
        metrics,
        HardwareConfig {
            sample_interval: Duration::from_millis(20),
            status_channel_capacity: 32,
        },
    )
    .unwrap();
    let (address, server) =
        start_server(router(runtime, &tokens.auth, integration.manager())).await;
    let mut socket = connect(address, &tokens.ordinary).await;
    send(
        &mut socket,
        json!({"id":1,"type":"hardware/subscribe_system_status"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    timeout(Duration::from_secs(2), async {
        while reads.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokens
        .auth
        .revoke_user_sessions(&tokens.owner_user_id, &tokens.ordinary_user_id)
        .unwrap();
    assert_connection_closed(&mut socket).await;
    sleep(Duration::from_millis(50)).await;
    let after_close = reads.load(Ordering::SeqCst);
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        after_close,
        "revocation must release the sampler guard even without an event"
    );
    stop_server(server, &mut integration).await;
}

#[tokio::test]
async fn status_subscriptions_share_the_thirty_two_command_limit() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (mut integration, manager, _) = hardware_integration(Some(HardwareConfig {
        sample_interval: Duration::from_secs(30),
        status_channel_capacity: 32,
    }));
    let (address, server) = start_server(router(runtime, &tokens.auth, manager)).await;
    let mut socket = connect(address, &tokens.ordinary).await;

    for id in 1..=32 {
        send(
            &mut socket,
            json!({"id": id, "type": "hardware/subscribe_system_status"}),
        )
        .await;
        assert_ack(&receive(&mut socket).await, id);
    }
    send(
        &mut socket,
        json!({"id": 33, "type": "hardware/subscribe_system_status"}),
    )
    .await;
    assert_error(&receive(&mut socket).await, 33, "subscription_limit");

    stop_server(server, &mut integration).await;
}
