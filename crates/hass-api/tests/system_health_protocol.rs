//! Loopback contract tests for the System Health WebSocket transport.
//!
//! Keep this file under `tests/drafts` until the API feature is wired into the
//! workspace; Cargo must not auto-discover it before that integration lands.

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Error as CoreError, MemoryStore, Runtime};
use hass_system_health::{
    HealthFuture, SystemHealthCallback, SystemHealthData, SystemHealthInfo,
    SystemHealthIntegration, SystemHealthValue,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const CLIENT_ID: &str = "http://localhost/";

async fn start_server(router: axum::Router) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
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
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn assert_connection_closed(socket: &mut Socket) {
    let result = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("WebSocket did not close after invalid authentication or command");
    assert!(result.is_none() || matches!(result, Some(Ok(Message::Close(_))) | Some(Err(_))));
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    send(
        &mut socket,
        json!({ "type": "auth", "access_token": token }),
    )
    .await;
    assert_eq!(receive(&mut socket).await["type"], "auth_ok");
    socket
}

struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
    reader: String,
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Health Owner", "health-owner-password")
        .unwrap();
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "health-user-password",
            false,
        )
        .unwrap();
    let read_only_groups = vec!["system-read-only".to_owned()];
    let reader = auth
        .create_user_without_credentials(&owner.id, "Read-only User", Some(&read_only_groups), None)
        .unwrap();
    Tokens {
        owner: auth
            .issue_tokens(&owner.id, CLIENT_ID, None)
            .unwrap()
            .access_token,
        ordinary: auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .unwrap()
            .access_token,
        reader: auth
            .issue_tokens(&reader.id, CLIENT_ID, None)
            .unwrap()
            .access_token,
        auth,
    }
}

fn api_state(runtime: Runtime) -> ApiState {
    ApiState {
        runtime,
        access_token: String::new(),
        config: json!({}),
    }
}

fn json_value(value: Value) -> SystemHealthValue {
    SystemHealthValue::Immediate(SystemHealthData::Json(value))
}

fn callback(info: BTreeMap<String, Value>) -> SystemHealthCallback {
    provider_callback(move || {
        let info = info
            .iter()
            .map(|(key, value)| (key.clone(), json_value(value.clone())))
            .collect();
        async move { Ok(info) }
    })
}

fn provider_callback<F, Fut>(factory: F) -> SystemHealthCallback
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = hass_core::Result<SystemHealthInfo>> + Send + 'static,
{
    Arc::new(move || Box::pin(factory()) as HealthFuture<SystemHealthInfo>)
}

async fn collect_one_shot(socket: &mut Socket, id: u64) -> Vec<Value> {
    send(socket, json!({ "id": id, "type": "system_health/info" })).await;
    let acknowledgement = receive(socket).await;
    assert_eq!(acknowledgement["id"], id);
    assert_eq!(acknowledgement["type"], "result");
    assert_eq!(acknowledgement["success"], true);
    assert_eq!(acknowledgement["result"], Value::Null);

    let mut events = Vec::new();
    loop {
        let frame = receive(socket).await;
        assert_eq!(frame["id"], id);
        assert_eq!(frame["type"], "event");
        let finished = frame["event"]["type"] == "finish";
        events.push(frame);
        if finished {
            return events;
        }
    }
}

#[tokio::test]
async fn health_stream_acknowledges_after_collection_and_finishes_deferred_updates() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let integration = SystemHealthIntegration::new();
    let manager = integration.manager();
    let replaced_calls = Arc::new(AtomicUsize::new(0));
    let active_calls = Arc::new(AtomicUsize::new(0));

    let replaced_counter = replaced_calls.clone();
    integration
        .register_info(
            "zeta",
            provider_callback(move || {
                let replaced_counter = replaced_counter.clone();
                async move {
                    replaced_counter.fetch_add(1, Ordering::Relaxed);
                    Ok(BTreeMap::from([("old".into(), json_value(json!("stale")))]))
                }
            }),
            None,
        )
        .unwrap();

    let (started_tx, mut started_rx) = mpsc::channel::<String>(2);
    let started_tx_zeta = started_tx.clone();
    let callback_started = Arc::new(Notify::new());
    let callback_release = Arc::new(Notify::new());
    let success_release = Arc::new(Notify::new());
    let failure_release = Arc::new(Notify::new());
    let success_release_callback = success_release.clone();
    let failure_release_callback = failure_release.clone();
    let callback_started_callback = callback_started.clone();
    let callback_release_callback = callback_release.clone();
    let active_counter = active_calls.clone();
    let alpha_callback = provider_callback(move || {
        let active_counter = active_counter.clone();
        let started_tx = started_tx.clone();
        let release = success_release_callback.clone();
        let callback_started = callback_started_callback.clone();
        let callback_release = callback_release_callback.clone();
        async move {
            active_counter.fetch_add(1, Ordering::Relaxed);
            callback_started.notify_one();
            callback_release.notified().await;
            let started = started_tx.clone();
            let deferred: HealthFuture<SystemHealthData> = Box::pin(async move {
                let _ = started.send("alpha/later".to_owned()).await;
                release.notified().await;
                Ok(SystemHealthData::DateTime(
                    chrono::DateTime::parse_from_rfc3339("2026-11-06T12:34:56+00:00")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ))
            });
            Ok(BTreeMap::from([
                ("instant".into(), json_value(json!({"ready": true}))),
                ("later".into(), SystemHealthValue::Deferred(deferred)),
            ]))
        }
    });
    integration
        .register_info("alpha", alpha_callback, None)
        .unwrap();

    let fail_release_callback = failure_release_callback.clone();
    integration
        .register_info(
            "zeta",
            provider_callback(move || {
                let started_tx = started_tx_zeta.clone();
                let release = fail_release_callback.clone();
                async move {
                    let started = started_tx.clone();
                    let deferred: HealthFuture<SystemHealthData> = Box::pin(async move {
                        let _ = started.send("zeta/broken".to_owned()).await;
                        release.notified().await;
                        Err(CoreError::Service("private provider detail".into()))
                    });
                    Ok(BTreeMap::from([(
                        "broken".into(),
                        SystemHealthValue::from_task(tokio::spawn(deferred)),
                    )]))
                }
            }),
            Some("https://example.invalid/manage".to_owned()),
        )
        .unwrap();

    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            system_health: Some(manager),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({ "id": 1, "type": "system_health/info" }),
    )
    .await;

    // The callback collection precedes the acknowledgement; deferred fields
    // are represented as pending in the immediately following initial event.
    timeout(Duration::from_secs(2), callback_started.notified())
        .await
        .expect("provider callback did not start");
    assert!(
        timeout(Duration::from_millis(150), receive(&mut socket))
            .await
            .is_err()
    );
    callback_release.notify_one();
    let acknowledgement = receive(&mut socket).await;
    assert_eq!(acknowledgement["result"], Value::Null);
    let initial = receive(&mut socket).await;
    assert_eq!(initial["event"]["type"], "initial");
    let domains = initial["event"]["data"].as_object().unwrap();
    assert_eq!(
        domains.keys().map(String::as_str).collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(domains["alpha"]["info"]["instant"], json!({"ready": true}));
    assert_eq!(domains["alpha"]["info"]["later"]["type"], "pending");
    assert_eq!(domains["zeta"]["info"]["broken"]["type"], "pending");
    assert_eq!(
        domains["zeta"]["manage_url"],
        "https://example.invalid/manage"
    );
    assert!(domains["zeta"]["info"].get("old").is_none());

    let started_a = timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let started_b = timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(started_a, started_b);
    success_release.notify_one();
    failure_release.notify_one();

    let mut updates = BTreeMap::new();
    loop {
        let frame = receive(&mut socket).await;
        assert_eq!(frame["id"], 1);
        assert_eq!(frame["type"], "event");
        let event = &frame["event"];
        if event["type"] == "finish" {
            break;
        }
        assert_eq!(event["type"], "update");
        updates.insert(event["key"].as_str().unwrap().to_owned(), event.clone());
    }
    assert_eq!(updates.len(), 2);
    assert_eq!(updates["later"]["success"], true);
    assert_eq!(updates["later"]["data"]["type"], "date");
    assert_eq!(updates["broken"]["success"], false);
    assert_eq!(
        updates["broken"]["error"],
        json!({"type":"failed","error":"unknown"})
    );
    assert!(
        !serde_json::to_string(&updates)
            .unwrap()
            .contains("private provider detail")
    );
    assert_eq!(replaced_calls.load(Ordering::Relaxed), 0);
    assert_eq!(active_calls.load(Ordering::Relaxed), 1);

    // A normally completed stream is removed, so its ID is not still
    // unsubscribe-able after `finish`.
    send(
        &mut socket,
        json!({ "id": 2, "type": "unsubscribe_events", "subscription": 1 }),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], false);

    server.abort();
}

#[tokio::test]
async fn system_health_is_readable_by_admin_ordinary_and_read_only_identities() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let integration = SystemHealthIntegration::new();
    integration
        .register_info(
            "fixture",
            callback(BTreeMap::from([("status".into(), json!("ok"))])),
            None,
        )
        .unwrap();
    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            system_health: Some(integration.manager()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;

    for token in [&tokens.owner, &tokens.ordinary, &tokens.reader] {
        let mut socket = connect(address, token).await;
        let events = collect_one_shot(&mut socket, 1).await;
        assert_eq!(events[0]["event"]["type"], "initial");
        assert_eq!(
            events[0]["event"]["data"]["fixture"]["info"]["status"],
            "ok"
        );
        assert_eq!(events[1]["event"]["type"], "finish");
        socket.close(None).await.unwrap();
    }
    server.abort();
}

#[tokio::test]
async fn strict_command_validation_missing_feature_and_invalid_auth_are_rejected() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let integration = SystemHealthIntegration::new();
    integration
        .register_info(
            "fixture",
            callback(BTreeMap::from([("status".into(), json!("ok"))])),
            None,
        )
        .unwrap();
    let app = router_with_features(
        api_state(runtime.clone()),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            system_health: Some(integration.manager()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({ "id": 1, "type": "system_health/info", "extra": true }),
    )
    .await;
    let malformed = receive(&mut socket).await;
    assert_eq!(malformed["success"], false);
    assert_eq!(malformed["error"]["code"], "invalid_format");
    let events = collect_one_shot(&mut socket, 2).await;
    assert_eq!(events.last().unwrap()["event"]["type"], "finish");
    send(
        &mut socket,
        json!({ "id": 2, "type": "system_health/info" }),
    )
    .await;
    assert_eq!(
        receive(&mut socket).await["error"]["code"],
        "invalid_format"
    );
    socket.close(None).await.unwrap();

    let mut boolean_id = connect(address, &tokens.owner).await;
    send(
        &mut boolean_id,
        json!({ "id": true, "type": "system_health/info" }),
    )
    .await;
    assert_connection_closed(&mut boolean_id).await;

    let (mut anonymous, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut anonymous).await["type"], "auth_required");
    send(
        &mut anonymous,
        json!({ "id": 1, "type": "system_health/info" }),
    )
    .await;
    assert_connection_closed(&mut anonymous).await;
    server.abort();

    let no_feature_runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let no_feature_auth = auth_tokens(&no_feature_runtime);
    let app = router_with_features(
        api_state(no_feature_runtime),
        ApiFeatures {
            auth: Some(no_feature_auth.auth.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let mut socket = connect(address, &no_feature_auth.owner).await;
    send(
        &mut socket,
        json!({ "id": 1, "type": "system_health/info" }),
    )
    .await;
    let unavailable = receive(&mut socket).await;
    assert_eq!(unavailable["success"], false);
    assert!(unavailable.get("event").is_none());
    server.abort();

    // A stale weak API handle gets a generic not-found response and no
    // provider details after the app-owned integration owner is dropped.
    let dead_runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let dead_tokens = auth_tokens(&dead_runtime);
    let integration = SystemHealthIntegration::new();
    let manager = integration.manager();
    let app = router_with_features(
        api_state(dead_runtime),
        ApiFeatures {
            auth: Some(dead_tokens.auth.clone()),
            system_health: Some(manager),
            ..ApiFeatures::default()
        },
        None,
    );
    drop(integration);
    let (address, server) = start_server(app).await;
    let mut socket = connect(address, &dead_tokens.owner).await;
    send(
        &mut socket,
        json!({ "id": 1, "type": "system_health/info" }),
    )
    .await;
    let dead_manager = receive(&mut socket).await;
    assert_eq!(dead_manager["success"], false);
    assert_eq!(dead_manager["error"]["code"], "not_found");
    assert_eq!(
        dead_manager["error"]["message"],
        "System Health manager is unavailable"
    );
    server.abort();

    let invalid_runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let invalid_tokens = auth_tokens(&invalid_runtime);
    let app = router_with_features(
        api_state(invalid_runtime),
        ApiFeatures {
            auth: Some(invalid_tokens.auth),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let (mut invalid, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut invalid).await["type"], "auth_required");
    send(
        &mut invalid,
        json!({ "type": "auth", "access_token": "invalid-token" }),
    )
    .await;
    assert_eq!(receive(&mut invalid).await["type"], "auth_invalid");
    server.abort();
}

struct DropSignal(Arc<AtomicUsize>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct PendingProvider {
    integration: SystemHealthIntegration,
    started: tokio::sync::oneshot::Receiver<()>,
    dropped: Arc<AtomicUsize>,
}

fn pending_provider() -> PendingProvider {
    let integration = SystemHealthIntegration::new();
    let dropped = Arc::new(AtomicUsize::new(0));
    let dropped_callback = dropped.clone();
    let (started_tx, started) = tokio::sync::oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let callback_started = started_tx.clone();
    integration
        .register_info(
            "pending",
            provider_callback(move || {
                let dropped = dropped_callback.clone();
                let started = callback_started.clone();
                async move {
                    let deferred: HealthFuture<SystemHealthData> = Box::pin(async move {
                        let _drop_signal = DropSignal(dropped);
                        let started = { started.lock().unwrap().take() };
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        std::future::pending::<()>().await;
                        #[allow(unreachable_code)]
                        Ok(SystemHealthData::Json(Value::Null))
                    });
                    Ok(BTreeMap::from([(
                        "forever".into(),
                        SystemHealthValue::from_task(tokio::spawn(deferred)),
                    )]))
                }
            }),
            None,
        )
        .unwrap();
    PendingProvider {
        integration,
        started,
        dropped,
    }
}

async fn connect_pending_stream(
    shutdown: Option<watch::Receiver<bool>>,
) -> (
    SocketAddr,
    JoinHandle<std::io::Result<()>>,
    Socket,
    tokio::sync::oneshot::Receiver<()>,
    Arc<AtomicUsize>,
    SystemHealthIntegration,
) {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let provider = pending_provider();
    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth),
            system_health: Some(provider.integration.manager()),
            ..ApiFeatures::default()
        },
        shutdown,
    );
    let (address, server) = start_server(app).await;
    let socket = connect(address, &tokens.owner).await;
    (
        address,
        server,
        socket,
        provider.started,
        provider.dropped,
        provider.integration,
    )
}

async fn start_pending(socket: &mut Socket, started: tokio::sync::oneshot::Receiver<()>) {
    send(socket, json!({ "id": 1, "type": "system_health/info" })).await;
    let acknowledgement = receive(socket).await;
    assert_eq!(acknowledgement["success"], true);
    assert_eq!(receive(socket).await["event"]["type"], "initial");
    timeout(Duration::from_secs(2), started)
        .await
        .expect("deferred future did not start")
        .unwrap();
}

async fn wait_for_drop(dropped: &AtomicUsize) {
    timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deferred future was not dropped");
}

#[tokio::test]
async fn unsubscribe_socket_close_and_server_shutdown_cancel_deferred_work() {
    // Explicit unsubscribe aborts and joins the stream task; no finish follows.
    let (_address, server, mut socket, started, dropped, _owner) =
        connect_pending_stream(None).await;
    start_pending(&mut socket, started).await;
    send(
        &mut socket,
        json!({ "id": 2, "type": "unsubscribe_events", "subscription": 1 }),
    )
    .await;
    let unsubscribe = receive(&mut socket).await;
    assert_eq!(unsubscribe["success"], true);
    wait_for_drop(&dropped).await;
    assert!(
        timeout(Duration::from_millis(150), socket.next())
            .await
            .is_err()
    );
    server.abort();

    // Closing the client socket also cancels its outstanding provider future.
    let (_, server, mut socket, started, dropped, _owner) = connect_pending_stream(None).await;
    start_pending(&mut socket, started).await;
    socket.send(Message::Close(None)).await.unwrap();
    wait_for_drop(&dropped).await;
    server.abort();

    // App shutdown closes active sockets and joins their streams.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (_, server, mut socket, started, dropped, _owner) =
        connect_pending_stream(Some(shutdown_rx)).await;
    start_pending(&mut socket, started).await;
    shutdown_tx.send(true).unwrap();
    wait_for_drop(&dropped).await;
    server.abort();
}
