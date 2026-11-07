//! Loopback protocol tests for the SSDP WebSocket subscription.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_ssdp::{SsdpIntegration, SsdpManager, decode_ssdp_packet};
use serde_json::{Value, json};
use tokio::{net::TcpListener as TokioTcpListener, sync::watch, task::JoinHandle, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const CLIENT_ID: &str = "http://localhost/";
const DEVICE_XML: &str = "<root><device><friendlyName>Fixture</friendlyName><manufacturer>Acme</manufacturer></device></root>";

struct HttpFixture {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make loopback fixture nonblocking");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking fixture request");
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = read_request(&mut stream);
                        worker_requests.fetch_add(1, Ordering::AcqRel);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{DEVICE_XML}",
                            DEVICE_XML.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn location(&self) -> String {
        format!("http://device.invalid:{}/device.xml", self.address.port())
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .resolve("device.invalid", self.address)
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("build loopback-only HTTP client")
    }

    async fn wait_for_requests(&self, count: usize) {
        timeout(Duration::from_secs(3), async {
            while self.requests.load(Ordering::Acquire) < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("description request did not reach the loopback fixture");
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join loopback HTTP fixture");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    request
}

struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
    reader: String,
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "SSDP Owner", "ssdp-owner-password")
        .expect("create owner");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "ssdp-user-password",
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
    ssdp: Option<SsdpManager>,
    shutdown: Option<watch::Receiver<bool>>,
) -> axum::Router {
    router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            ssdp,
            ..ApiFeatures::default()
        },
        shutdown,
    )
}

async fn start_server(router: axum::Router) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
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

fn decode_packet(packet: String) -> hass_ssdp::SsdpMessage {
    decode_ssdp_packet(
        packet.as_bytes(),
        None,
        "192.0.2.18:1900".parse().expect("reserved fixture address"),
        Utc::now(),
    )
    .expect("decode local SSDP fixture")
}

fn alive(udn: &str, location: &str) -> hass_ssdp::SsdpMessage {
    decode_packet(format!(
        "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=3600\r\nLOCATION: {location}\r\nNT: upnp:rootdevice\r\nNTS: ssdp:alive\r\nSERVER: Media/1\r\nUSN: {udn}::upnp:rootdevice\r\n\r\n"
    ))
}

fn byebye(udn: &str, location: &str) -> hass_ssdp::SsdpMessage {
    decode_packet(format!(
        "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nLOCATION: {location}\r\nNT: upnp:rootdevice\r\nNTS: ssdp:byebye\r\nSERVER: Media/1\r\nUSN: {udn}::upnp:rootdevice\r\n\r\n"
    ))
}

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime")
}

#[tokio::test]
async fn subscription_requires_admin_rejects_extra_fields_and_counts_with_other_streams() {
    let fixture = HttpFixture::start();
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let integration = SsdpIntegration::with_client(fixture.client(), Default::default(), None);
    let manager = integration.manager();
    let (address, server) = start_server(router(runtime, &tokens.auth, Some(manager), None)).await;

    let mut owner = connect(address, &tokens.owner).await;
    send(
        &mut owner,
        json!({"id": 1, "type": "ssdp/subscribe_discovery", "unexpected": true}),
    )
    .await;
    let extra_field = receive(&mut owner).await;
    assert_eq!(extra_field["error"]["code"], "invalid_format");

    send(&mut owner, json!({"id": 2, "type": 42})).await;
    let bad_type = receive(&mut owner).await;
    assert_eq!(bad_type["error"]["code"], "invalid_format");

    for (token, label) in [
        (&tokens.ordinary, "ordinary"),
        (&tokens.reader, "read-only"),
    ] {
        let mut socket = connect(address, token).await;
        send(
            &mut socket,
            json!({"id": 1, "type": "ssdp/subscribe_discovery"}),
        )
        .await;
        let denied = receive(&mut socket).await;
        assert_eq!(denied["error"]["code"], "unauthorized", "{label} user");
    }

    for id in 3..35 {
        send(
            &mut owner,
            json!({"id": id, "type": "subscribe_events", "event_type": "state_changed"}),
        )
        .await;
        assert_eq!(receive(&mut owner).await["success"], true);
    }
    send(
        &mut owner,
        json!({"id": 35, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    let limit = receive(&mut owner).await;
    assert_eq!(limit["error"]["code"], "subscription_limit");

    let mut invalid_id = connect(address, &tokens.owner).await;
    send(
        &mut invalid_id,
        json!({"id": "1", "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    assert_connection_closed(&mut invalid_id).await;

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn acknowledgement_precedes_cached_replay_and_live_add_remove_and_unsubscribe() {
    let fixture = HttpFixture::start();
    let integration = SsdpIntegration::with_client(fixture.client(), Default::default(), None);
    let manager = integration.manager();
    let location = fixture.location();
    manager
        .observe(&alive("uuid:cached", &location), Utc::now())
        .await
        .expect("seed cached discovery");
    fixture.wait_for_requests(1).await;

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()), None)).await;
    let mut socket = connect(address, &tokens.owner).await;

    send(
        &mut socket,
        json!({"id": 1, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    let acknowledgement = receive(&mut socket).await;
    assert_eq!(acknowledgement["id"], 1);
    assert_eq!(acknowledgement["type"], "result");
    assert_eq!(acknowledgement["success"], true);
    assert_eq!(acknowledgement["result"], Value::Null);

    let cached = receive(&mut socket).await;
    assert_eq!(cached["id"], 1);
    assert_eq!(cached["type"], "event");
    assert_eq!(cached["event"]["add"][0]["name"], "Fixture");
    assert_eq!(
        cached["event"]["add"][0]["ssdp_usn"],
        "uuid:cached::upnp:rootdevice"
    );
    assert!(cached["event"].get("initial").is_none());
    assert!(cached["event"].get("finish").is_none());

    manager
        .observe(&alive("uuid:live", &location), Utc::now())
        .await
        .expect("observe live discovery");
    let live = receive(&mut socket).await;
    assert_eq!(
        live["event"]["add"][0]["ssdp_usn"],
        "uuid:live::upnp:rootdevice"
    );

    manager
        .observe(&byebye("uuid:live", &location), Utc::now())
        .await
        .expect("observe byebye discovery");
    let removed = receive(&mut socket).await;
    assert_eq!(removed["event"]["remove"][0]["ssdp_st"], "upnp:rootdevice");
    assert_eq!(removed["event"]["remove"][0]["ssdp_location"], location);

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    manager
        .observe(&alive("uuid:after-unsubscribe", &location), Utc::now())
        .await
        .expect("observe after unsubscribe");
    assert!(
        timeout(Duration::from_millis(250), socket.next())
            .await
            .is_err()
    );

    send(
        &mut socket,
        json!({"id": 3, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    socket
        .send(Message::Close(None))
        .await
        .expect("close client socket");
    drop(socket);
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn callback_queue_overflow_reports_error_and_terminates_subscription() {
    let fixture = HttpFixture::start();
    let integration = SsdpIntegration::with_client(fixture.client(), Default::default(), None);
    let manager = integration.manager();
    let location = fixture.location();
    manager
        .observe(&alive("uuid:warm", &location), Utc::now())
        .await
        .expect("warm description cache");
    manager
        .observe(&byebye("uuid:warm", &location), Utc::now())
        .await
        .expect("remove warm device from cached tracker");

    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) =
        start_server(router(runtime, &tokens.auth, Some(manager.clone()), None)).await;
    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    tokio::time::sleep(Duration::from_millis(20)).await;

    for index in 0..1000 {
        let udn = format!("uuid:overflow-{index}");
        manager
            .observe(&alive(&udn, &location), Utc::now())
            .await
            .expect("observe overflow fixture");
    }

    let mut delivered = 0;
    loop {
        let frame = receive(&mut socket).await;
        if frame["type"] == "result" && frame["success"] == false {
            assert_eq!(frame["id"], 1);
            assert_eq!(frame["error"]["code"], "ssdp_stream_error");
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
    assert!(
        delivered <= 64,
        "bounded queue delivered {delivered} frames"
    );

    send(
        &mut socket,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    let missing = receive(&mut socket).await;
    assert_eq!(missing["error"]["code"], "not_found");

    manager
        .observe(&alive("uuid:after-overflow", &location), Utc::now())
        .await
        .expect("observe after overflow");
    assert!(
        timeout(Duration::from_millis(250), socket.next())
            .await
            .is_err()
    );

    drop(socket);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn weak_manager_reports_failure_after_ack_and_server_shutdown_closes_streams() {
    let fixture = HttpFixture::start();
    let integration = SsdpIntegration::with_client(fixture.client(), Default::default(), None);
    let manager = integration.manager();
    let runtime = new_runtime();
    let tokens = auth_tokens(&runtime);
    let (address, server) = start_server(router(runtime, &tokens.auth, Some(manager), None)).await;
    drop(integration);

    let mut socket = connect(address, &tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    let acknowledgement = receive(&mut socket).await;
    assert_eq!(acknowledgement["success"], true);
    assert_eq!(acknowledgement["result"], Value::Null);
    let failed = receive(&mut socket).await;
    assert_eq!(failed["error"]["code"], "ssdp_stream_error");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    server.abort();
    let _ = server.await;

    let fixture = HttpFixture::start();
    let integration = SsdpIntegration::with_client(fixture.client(), Default::default(), None);
    let manager = integration.manager();
    let shutdown_runtime = new_runtime();
    let shutdown_tokens = auth_tokens(&shutdown_runtime);
    let (address, server) = start_server(router(
        shutdown_runtime,
        &shutdown_tokens.auth,
        Some(manager),
        Some(shutdown_rx),
    ))
    .await;
    let mut socket = connect(address, &shutdown_tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "ssdp/subscribe_discovery"}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["success"], true);
    shutdown_tx.send(true).expect("signal API shutdown");
    assert_connection_closed(&mut socket).await;

    server.abort();
    let _ = server.await;
}
