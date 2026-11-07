//! Loopback protocol fixtures for the DHCP WebSocket subscription.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_dhcp::{DhcpAddressData, DhcpIntegration, DhcpManager, DhcpObservation};
use serde_json::{Value, json};
use tokio::{net::TcpListener as TokioTcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "dhcp-protocol-fixture";
type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
    reader: String,
}

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime")
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "DHCP Owner", "dhcp-owner-password")
        .expect("create owner");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "dhcp-user-password",
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
        owner: auth
            .issue_tokens(&owner.id, CLIENT_ID, None)
            .expect("issue owner token")
            .access_token,
        ordinary: auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .expect("issue ordinary token")
            .access_token,
        reader: auth
            .issue_tokens(&reader.id, CLIENT_ID, None)
            .expect("issue read-only token")
            .access_token,
        auth,
    }
}

fn router(runtime: Runtime, auth: &AuthManager, dhcp: Option<DhcpManager>) -> Router {
    router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            dhcp,
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

fn ingest(manager: &DhcpManager, ip: &str, hostname: &str, mac: &str) {
    manager
        .ingest(
            DhcpObservation {
                ip: ip.to_owned(),
                hostname: hostname.to_owned(),
                mac: mac.to_owned(),
            },
            false,
        )
        .expect("ingest synthetic DHCP observation");
}

#[tokio::test]
async fn command_is_admin_only_and_has_an_exact_schema() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let integration = DhcpIntegration::new(None, None);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(integration.manager()))).await;

    let mut owner = connect(address, &tokens.owner).await;
    send(
        &mut owner,
        json!({
            "id": 1,
            "type": "dhcp/subscribe_discovery",
            "unexpected": true
        }),
    )
    .await;
    assert_error(&receive(&mut owner).await, 1, "invalid_format");

    let mut invalid_id = connect(address, &tokens.owner).await;
    send(
        &mut invalid_id,
        json!({"id": "4", "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_connection_closed(&mut invalid_id).await;

    send(
        &mut owner,
        json!({"id": 2, "type": "dhcp/subscribe_discover"}),
    )
    .await;
    assert_error(&receive(&mut owner).await, 2, "unknown_command");

    send(
        &mut owner,
        json!({"id": 3, "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut owner).await, 3);
    let empty_snapshot = receive(&mut owner).await;
    assert_eq!(empty_snapshot["id"], 3);
    assert_eq!(empty_snapshot["type"], "event");
    assert_eq!(empty_snapshot["event"], json!({"add": []}));

    for (token, label) in [
        (&tokens.ordinary, "ordinary"),
        (&tokens.reader, "read-only"),
    ] {
        let mut socket = connect(address, token).await;
        send(
            &mut socket,
            json!({"id": 1, "type": "dhcp/subscribe_discovery"}),
        )
        .await;
        let denied = receive(&mut socket).await;
        assert_eq!(denied["id"], 1, "{label} user");
        assert_eq!(denied["error"]["code"], "unauthorized", "{label} user");
        drop(socket);
    }

    drop(owner);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn snapshot_and_updates_use_raw_cache_data_and_source_mac_formatting() {
    let integration = DhcpIntegration::new(None, None);
    let manager = integration.manager();
    ingest(&manager, "2001:0db8::0001", "DeskTop", "aa:bb:cc:dd:ee:ff");

    let cached = manager.address_data_snapshot().expect("raw DHCP cache");
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].0, "aabbccddeeff");
    assert_eq!(
        cached[0].1,
        DhcpAddressData {
            ip: "2001:db8::1".to_owned(),
            hostname: "DeskTop".to_owned(),
        }
    );

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()))).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    assert_eq!(
        receive(&mut socket).await,
        json!({
            "id": 1,
            "type": "event",
            "event": {
                "add": [{
                    "mac_address": "AA:BB:CC:DD:EE:FF",
                    "hostname": "DeskTop",
                    "ip_address": "2001:db8::1"
                }]
            }
        })
    );
    assert!(
        timeout(Duration::from_millis(120), socket.next())
            .await
            .is_err()
    );

    ingest(&manager, "198.51.100.4", "NewHost", "a1:b2:c3:d4:e5:f6");
    assert_eq!(
        receive(&mut socket).await,
        json!({
            "id": 1,
            "type": "event",
            "event": {
                "add": [{
                    "mac_address": "A1:B2:C3:D4:E5:F6",
                    "hostname": "NewHost",
                    "ip_address": "198.51.100.4"
                }]
            }
        })
    );

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    ingest(
        &manager,
        "198.51.100.5",
        "AfterUnsubscribe",
        "a1:b2:c3:d4:e5:f7",
    );
    assert!(
        timeout(Duration::from_millis(160), socket.next())
            .await
            .is_err()
    );

    drop(socket);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn bounded_queue_reports_overflow() {
    let integration = DhcpIntegration::new(None, None);
    let manager = integration.manager();
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()))).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    assert_eq!(receive(&mut socket).await["event"], json!({"add": []}));

    // The protocol pump is not polled while this synchronous loop fills the
    // 64-frame stream queue. The next update must deactivate the stream and
    // leave a visible error instead of dropping it silently.
    for index in 0..65 {
        ingest(
            &manager,
            &format!("192.0.2.{}", index + 1),
            "overflow",
            &format!("02:00:00:00:00:{index:02x}"),
        );
    }
    for _ in 0..64 {
        let frame = receive(&mut socket).await;
        assert_eq!(frame["id"], 1);
        assert_eq!(frame["type"], "event");
    }
    assert_error(&receive(&mut socket).await, 1, "dhcp_stream_error");

    drop(socket);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn dhcp_subscription_uses_the_shared_thirty_two_subscription_limit() {
    let integration = DhcpIntegration::new(None, None);
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(integration.manager()))).await;
    let mut socket = connect(address, &tokens.owner).await;

    for id in 1..=31 {
        send(&mut socket, json!({"id": id, "type": "subscribe_events"})).await;
        assert_ack(&receive(&mut socket).await, id);
    }
    send(
        &mut socket,
        json!({"id": 32, "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 32);
    assert_eq!(receive(&mut socket).await["event"], json!({"add": []}));

    send(&mut socket, json!({"id": 33, "type": "subscribe_events"})).await;
    assert_error(&receive(&mut socket).await, 33, "subscription_limit");

    drop(socket);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn unsubscribe_cancels_snapshot_and_tolerates_sdk_owner_teardown() {
    let integration = DhcpIntegration::new(None, None);
    let manager = integration.manager();
    for index in 0..1800 {
        ingest(
            &manager,
            "192.0.2.1",
            "snapshot",
            &format!("02:00:00:00:{:02x}:{:02x}", index >> 8, index & 0xff),
        );
    }
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()))).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "dhcp/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    loop {
        let frame = receive(&mut socket).await;
        if frame["id"] == 2 {
            assert_eq!(frame["success"], true);
            break;
        }
        // A completed snapshot may win the race with unsubscribe, but a
        // partial add-list frame must never be emitted.
        assert_eq!(frame["id"], 1);
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["event"]["add"].as_array().unwrap().len(), 1800);
    }

    drop(integration);
    assert!(manager.address_data_snapshot().is_err());
    assert!(
        timeout(Duration::from_millis(120), socket.next())
            .await
            .is_err()
    );

    drop(socket);
    server.abort();
    let _ = server.await;
}
