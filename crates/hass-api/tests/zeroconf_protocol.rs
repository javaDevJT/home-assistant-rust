//! Loopback protocol fixtures for the Zeroconf WebSocket subscription.
use std::{
    collections::{HashMap, HashSet},
    future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Result, Runtime};
use hass_zeroconf::{
    HOME_ASSISTANT_SERVICE_TYPE, RawResolvedServiceInfo, RawTxtEntry, ResolverFuture,
    ScopedIpAddress, ServiceStateChange, ZeroconfBackend, ZeroconfIntegration, ZeroconfManager,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener as TokioTcpListener, sync::watch, task::JoinHandle, time::timeout};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "zeroconf-protocol-fixture";
type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type RecordKey = (String, String);

struct FakeBackend {
    records: Mutex<HashMap<RecordKey, RawResolvedServiceInfo>>,
    ptr_names: Mutex<HashMap<String, Vec<String>>>,
    pending: Mutex<HashSet<RecordKey>>,
    resolve_started: Arc<tokio::sync::Notify>,
    resolve_dropped: Arc<AtomicBool>,
    snapshot_gate: Mutex<Option<(String, Arc<SnapshotGate>)>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            ptr_names: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            resolve_started: Arc::new(tokio::sync::Notify::new()),
            resolve_dropped: Arc::new(AtomicBool::new(false)),
            snapshot_gate: Mutex::new(None),
        }
    }
}

impl FakeBackend {
    fn put_record(&self, record: RawResolvedServiceInfo) {
        self.records
            .lock()
            .expect("record lock")
            .insert((record.service_type.clone(), record.name.clone()), record);
    }

    fn add_ptr_record(&self, record: RawResolvedServiceInfo) {
        self.ptr_names
            .lock()
            .expect("PTR lock")
            .entry(record.service_type.clone())
            .or_default()
            .push(record.name.clone());
        self.put_record(record);
    }

    fn add_ptr_name(&self, service_type: &str, name: &str) {
        self.ptr_names
            .lock()
            .expect("PTR lock")
            .entry(service_type.to_owned())
            .or_default()
            .push(name.to_owned());
        self.pending
            .lock()
            .expect("pending lock")
            .insert((service_type.to_owned(), name.to_owned()));
    }

    fn block_snapshot_names(&self, service_type: String) -> Arc<SnapshotGate> {
        let gate = Arc::new(SnapshotGate::default());
        *self.snapshot_gate.lock().expect("snapshot gate lock") =
            Some((service_type, Arc::clone(&gate)));
        gate
    }
}

impl ZeroconfBackend for FakeBackend {
    fn complete_cached(&self, service_type: &str, name: &str) -> Option<RawResolvedServiceInfo> {
        self.records
            .lock()
            .expect("record lock")
            .get(&(service_type.to_owned(), name.to_owned()))
            .cloned()
    }

    fn cached_names(&self, service_type: &str) -> Vec<String> {
        let gate = self
            .snapshot_gate
            .lock()
            .expect("snapshot gate lock")
            .as_ref()
            .filter(|(blocked_type, _)| blocked_type == service_type)
            .map(|(_, gate)| Arc::clone(gate));
        if let Some(gate) = gate
            && !gate.used.swap(true, Ordering::AcqRel)
        {
            gate.wait();
            return vec![format!("stale.{service_type}")];
        }
        self.ptr_names
            .lock()
            .expect("PTR lock")
            .get(service_type)
            .cloned()
            .unwrap_or_default()
    }

    fn resolve(&self, service_type: String, name: String) -> ResolverFuture {
        let key = (service_type, name);
        if self.pending.lock().expect("pending lock").contains(&key) {
            let started = Arc::clone(&self.resolve_started);
            let dropped = Arc::clone(&self.resolve_dropped);
            return Box::pin(async move {
                struct OnDrop(Arc<AtomicBool>);
                impl Drop for OnDrop {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _on_drop = OnDrop(dropped);
                started.notify_one();
                future::pending::<Result<Option<RawResolvedServiceInfo>>>().await
            });
        }
        let record = self.records.lock().expect("record lock").get(&key).cloned();
        Box::pin(async move { Ok(record) })
    }
}

struct SnapshotGate {
    entered: AtomicBool,
    used: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl Default for SnapshotGate {
    fn default() -> Self {
        Self {
            entered: AtomicBool::new(false),
            used: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        }
    }
}

impl SnapshotGate {
    fn wait(&self) {
        let mut released = self.released.lock().expect("gate lock");
        self.entered.store(true, Ordering::Release);
        while !*released {
            released = self.changed.wait(released).expect("wait for gate");
        }
    }

    fn release(&self) {
        *self.released.lock().expect("gate lock") = true;
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
    reader: String,
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "Zeroconf Owner", "zeroconf-owner-password")
        .expect("create owner");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "zeroconf-user-password",
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

fn router(
    runtime: Runtime,
    auth: &AuthManager,
    zeroconf: Option<ZeroconfManager>,
    shutdown: Option<watch::Receiver<bool>>,
) -> Router {
    router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            zeroconf,
            ..ApiFeatures::default()
        },
        shutdown,
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

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime")
}

fn service_record(service_type: &str, name: &str) -> RawResolvedServiceInfo {
    RawResolvedServiceInfo {
        ip_addresses: vec![
            ScopedIpAddress::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 44)), 0),
            ScopedIpAddress::new(IpAddr::V6("fe80::44".parse::<Ipv6Addr>().unwrap()), 7),
        ],
        port: Some(4321),
        hostname: Some("fixture.local.".to_owned()),
        service_type: service_type.to_owned(),
        name: name.to_owned(),
        txt_entries: vec![
            RawTxtEntry {
                key: b"flag".to_vec(),
                value: None,
            },
            RawTxtEntry {
                key: b"empty".to_vec(),
                value: Some(Vec::new()),
            },
            RawTxtEntry {
                key: b"raw".to_vec(),
                value: Some(vec![0xff]),
            },
        ],
    }
}

fn integration(backend: Arc<FakeBackend>) -> ZeroconfIntegration {
    ZeroconfIntegration::new(backend, None).expect("create Zeroconf fixture integration")
}

fn assert_ack(frame: &Value, id: u64) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], true);
    assert_eq!(frame["result"], Value::Null);
}

#[tokio::test]
async fn subscribe_discovery_requires_admin_and_rejects_extra_fields() {
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let integration = integration(Arc::new(FakeBackend::default()));
    let (address, server) = start_server(router(
        runtime,
        &tokens.auth,
        Some(integration.manager()),
        None,
    ))
    .await;

    let mut owner = connect(address, &tokens.owner).await;
    send(
        &mut owner,
        json!({
            "id": 1,
            "type": "zeroconf/subscribe_discovery",
            "unexpected": true
        }),
    )
    .await;
    assert_eq!(receive(&mut owner).await["error"]["code"], "invalid_format");

    for (token, label) in [
        (&tokens.ordinary, "ordinary"),
        (&tokens.reader, "read-only"),
    ] {
        let mut socket = connect(address, token).await;
        send(
            &mut socket,
            json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
        )
        .await;
        let denied = receive(&mut socket).await;
        assert_eq!(denied["error"]["code"], "unauthorized", "{label} user");
    }

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn acknowledgement_precedes_snapshot_and_live_events_have_source_shapes() {
    let backend = Arc::new(FakeBackend::default());
    let integration = integration(Arc::clone(&backend));
    let manager = integration.manager();
    let generated_type = manager
        .service_types()
        .expect("generated Zeroconf types")
        .into_iter()
        .next()
        .expect("at least one generated service type");
    backend.add_ptr_record(service_record(&generated_type, "cached.fixture"));
    backend.add_ptr_record(service_record(
        HOME_ASSISTANT_SERVICE_TYPE,
        "homeassistant.fixture",
    ));

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()), None)).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;

    // A listener is prepared synchronously before this result. Cache replay
    // starts afterward, so no event can precede the acknowledgement.
    assert_ack(&receive(&mut socket).await, 1);
    let cached = receive(&mut socket).await;
    assert_eq!(cached["id"], 1);
    assert_eq!(cached["type"], "event");
    assert_eq!(cached["event"].as_object().unwrap().len(), 1);
    let cached_item = &cached["event"]["add"][0];
    assert_eq!(cached_item["name"], "cached.fixture");
    assert_eq!(cached_item["type"], generated_type);
    assert_eq!(cached_item["port"], 4321);
    assert_eq!(cached_item["properties"]["flag"], Value::Null);
    assert_eq!(cached_item["properties"]["empty"], "");
    assert_eq!(cached_item["properties"]["raw"], "�");
    assert_eq!(
        cached_item["ip_addresses"],
        json!(["192.0.2.44", "fe80::44%7"])
    );
    let mut add_fields = cached_item
        .as_object()
        .expect("add record object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    add_fields.sort();
    assert_eq!(
        add_fields,
        vec![
            "ip_addresses".to_owned(),
            "name".to_owned(),
            "port".to_owned(),
            "properties".to_owned(),
            "type".to_owned(),
        ]
    );
    assert!(cached["event"].get("initial").is_none());
    assert!(cached["event"].get("finish").is_none());
    assert!(
        timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err()
    );

    // Special browser types are not in the generated matcher list, so their
    // cache rows and update callbacks do not produce add events.
    manager
        .service_update(
            HOME_ASSISTANT_SERVICE_TYPE,
            "homeassistant.fixture",
            ServiceStateChange::Added,
        )
        .expect("deliver special-type update");

    let live_record = service_record(&generated_type, "live.fixture");
    backend.put_record(live_record);
    manager
        .service_update(
            generated_type.clone(),
            "live.fixture",
            ServiceStateChange::Added,
        )
        .expect("deliver generated-type update");
    let live = receive(&mut socket).await;
    assert_eq!(live["event"]["add"][0]["name"], "live.fixture");
    assert_eq!(live["event"]["add"][0]["type"], generated_type);

    manager
        .service_update(generated_type, "live.fixture", ServiceStateChange::Removed)
        .expect("deliver removal");
    let removed = receive(&mut socket).await;
    let removed_item = &removed["event"]["remove"][0];
    assert_eq!(removed_item, &json!({"name": "live.fixture"}));
    assert_eq!(removed_item.as_object().unwrap().len(), 1);

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    manager
        .service_update(
            HOME_ASSISTANT_SERVICE_TYPE,
            "homeassistant.fixture",
            ServiceStateChange::Added,
        )
        .expect("deliver after unsubscribe");
    assert!(
        timeout(Duration::from_millis(150), socket.next())
            .await
            .is_err()
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn queue_overflow_is_bounded_and_reports_a_terminal_subscription_error() {
    let backend = Arc::new(FakeBackend::default());
    let integration = integration(Arc::clone(&backend));
    let manager = integration.manager();
    let service_type = manager
        .service_types()
        .expect("generated Zeroconf types")
        .into_iter()
        .next()
        .expect("at least one generated service type");
    for index in 0..100 {
        backend.put_record(service_record(
            &service_type,
            &format!("overflow-{index}.fixture"),
        ));
    }
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()), None)).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);

    // The current-thread fixture does not let the socket pump run while these
    // synchronous SDK callbacks fill the queue.
    for index in 0..100 {
        manager
            .service_update(
                service_type.clone(),
                format!("overflow-{index}.fixture"),
                ServiceStateChange::Added,
            )
            .expect("deliver overflow update");
    }

    let mut delivered = 0;
    loop {
        let frame = receive(&mut socket).await;
        if frame["type"] == "result" && frame["success"] == false {
            assert_eq!(frame["id"], 1);
            assert_eq!(frame["error"]["code"], "zeroconf_stream_error");
            assert!(
                frame["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("queue overflowed"))
            );
            break;
        }
        assert_eq!(frame["type"], "event");
        assert!(frame["event"]["add"].is_array());
        delivered += 1;
    }
    assert_eq!(delivered, 64, "only the bounded queue is delivered");

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["error"]["code"], "not_found");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn unsubscribe_cancels_and_joins_a_pending_snapshot_resolution() {
    let backend = Arc::new(FakeBackend::default());
    let integration = integration(Arc::clone(&backend));
    let manager = integration.manager();
    let service_type = manager
        .service_types()
        .expect("generated Zeroconf types")
        .into_iter()
        .next()
        .expect("at least one generated service type");
    backend.add_ptr_name(&service_type, "pending.fixture");

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()), None)).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);
    timeout(Duration::from_secs(2), backend.resolve_started.notified())
        .await
        .expect("snapshot resolver did not start");

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    assert!(backend.resolve_dropped.load(Ordering::Acquire));

    let after_unsubscribe = service_record(&service_type, "after-unsubscribe.fixture");
    backend.put_record(after_unsubscribe);
    manager
        .service_update(
            service_type,
            "after-unsubscribe.fixture",
            ServiceStateChange::Added,
        )
        .expect("deliver after unsubscribe");
    assert!(
        timeout(Duration::from_millis(150), socket.next())
            .await
            .is_err()
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn client_close_and_server_shutdown_close_loopback_subscriptions() {
    let backend = Arc::new(FakeBackend::default());
    let integration = integration(backend);
    let manager = integration.manager();
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (address, server) = start_server(router(
        runtime,
        &tokens.auth,
        Some(manager),
        Some(shutdown_rx),
    ))
    .await;

    let mut closing_socket = connect(address, &tokens.owner).await;
    send(
        &mut closing_socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut closing_socket).await, 1);
    closing_socket
        .send(Message::Close(None))
        .await
        .expect("send client close");
    assert_connection_closed(&mut closing_socket).await;

    let mut shutdown_socket = connect(address, &tokens.owner).await;
    send(
        &mut shutdown_socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut shutdown_socket).await, 1);
    shutdown_tx.send(true).expect("signal API shutdown");
    assert_connection_closed(&mut shutdown_socket).await;

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weak_manager_owner_removed_during_snapshot_reports_failure_after_ack() {
    let backend = Arc::new(FakeBackend::default());
    let integration = integration(Arc::clone(&backend));
    let manager = integration.manager();
    let service_type = manager
        .service_types()
        .expect("generated Zeroconf types")
        .into_iter()
        .next()
        .expect("at least one generated service type");
    let gate = backend.block_snapshot_names(service_type);

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) = start_server(router(runtime, &tokens.auth, Some(manager), None)).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "zeroconf/subscribe_discovery"}),
    )
    .await;
    assert_ack(&receive(&mut socket).await, 1);

    timeout(Duration::from_secs(2), async {
        while !gate.entered.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("snapshot did not reach the blocked cache lookup");
    drop(integration);
    gate.release();

    let failed = receive(&mut socket).await;
    assert_eq!(failed["type"], "result");
    assert_eq!(failed["id"], 1);
    assert_eq!(failed["success"], false);
    assert_eq!(failed["error"]["code"], "zeroconf_stream_error");
    assert!(
        failed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Zeroconf"))
    );

    server.abort();
    let _ = server.await;
}
