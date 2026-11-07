//! Real WebSocket, tracing output, and retained-log contract coverage.

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Context, MemoryStore, Runtime};
use hass_logger::{
    IntegrationCatalog, IntegrationCatalogProvider, LoggerConfig, LoggerIntegration, LoggerLevel,
};
use hass_system_log::{LogReader, SystemLogConfig, SystemLogIntegration};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::watch,
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::Instrument;
use tracing_subscriber::prelude::*;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

async fn receive(socket: &mut Socket) -> Value {
    loop {
        let frame = timeout(
            Duration::from_secs(4),
            socket
                .next()
                .instrument(tracing::debug_span!("hass_raw_stream_delivery")),
        )
        .await
        .expect("WebSocket response timed out")
        .expect("WebSocket closed before response")
        .expect("WebSocket receive failed");
        match frame {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .instrument(tracing::debug_span!("hass_raw_stream_delivery"))
                    .await
                    .unwrap();
            }
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn command(socket: &mut Socket, command: Value) -> Value {
    socket
        .send(Message::Text(command.to_string().into()))
        .instrument(tracing::debug_span!("hass_raw_stream_delivery"))
        .await
        .unwrap();
    receive(socket).await
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .instrument(tracing::debug_span!("hass_raw_stream_delivery"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(&mut socket, json!({"type":"auth","access_token":token})).await["type"],
        "auth_ok"
    );
    socket
}

async fn start_server(
    router: axum::Router,
) -> (SocketAddr, JoinHandle<Result<(), std::io::Error>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    (address, server)
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

async fn write_log(
    socket: &mut Socket,
    id: u64,
    message: &str,
    level: &str,
    logger: &str,
) -> Value {
    command(
        socket,
        json!({
            "id": id,
            "type": "call_service",
            "domain": "system_log",
            "service": "write",
            "service_data": {
                "message": message,
                "level": level,
                "logger": logger,
            }
        }),
    )
    .await
}

async fn wait_for_raw_lines(socket: &mut Socket, id: u64, needles: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut lines = Vec::new();
    while Instant::now() < deadline {
        let frame = timeout(
            deadline.saturating_duration_since(Instant::now()),
            receive(socket),
        )
        .await
        .expect("raw log event timed out");
        assert_eq!(frame["id"], id);
        assert_eq!(frame["type"], "event");
        let batch = frame["event"]
            .as_array()
            .expect("raw log event payload must be an array");
        lines.extend(batch.iter().map(|line| line.as_str().unwrap().to_owned()));
        if needles
            .iter()
            .all(|needle| lines.iter().any(|line| line.contains(needle)))
        {
            return lines;
        }
    }
    panic!("raw stream did not contain {needles:?}: {lines:?}");
}

async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
    timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::Relaxed) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected asynchronous reader was not polled");
}

#[tokio::test]
async fn admin_raw_stream_and_list_share_real_filtered_console_records() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "system-log-owner-password")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "system-log-user-password", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();

    let catalog: IntegrationCatalogProvider = Arc::new(|| {
        IntegrationCatalog::from([(
            "system_log_protocol".into(),
            vec!["system_log_protocol".into()],
        )])
    });
    let logger = LoggerIntegration::start(
        runtime.clone(),
        LoggerConfig {
            default: Some(LoggerLevel::NotSet),
            filters: BTreeMap::from([
                (
                    "system_log_protocol.dynamic".into(),
                    vec!["^suppressed".into()],
                ),
                // A stale/static log target would reject the admitted message.
                ("log".into(), vec!["^critical".into()]),
            ]),
            ..LoggerConfig::default()
        },
        catalog.clone(),
        Arc::new(|_: &Context| Ok(())),
    )
    .unwrap();
    let system_log =
        SystemLogIntegration::start(runtime.clone(), SystemLogConfig::default(), catalog).unwrap();
    let read_calls = Arc::new(AtomicUsize::new(0));
    let read_counter = read_calls.clone();
    let pending_read_drops = Arc::new(AtomicUsize::new(0));
    let drop_counter = pending_read_drops.clone();
    let pending_read_starts = Arc::new(AtomicUsize::new(0));
    let started_counter = pending_read_starts.clone();
    let log_reader: LogReader = Arc::new(move || {
        if read_counter.fetch_add(1, Ordering::Relaxed) == 0 {
            Box::pin(async {
                Ok(vec![
                    "2026-11-06T12:00:00.000Z INFO [system_log_protocol.reader] backlog fixture"
                        .to_owned(),
                ])
            })
        } else {
            let drop_counter = drop_counter.clone();
            let started_counter = started_counter.clone();
            Box::pin(async move {
                let _drop_counter = DropCounter(drop_counter);
                started_counter.fetch_add(1, Ordering::Relaxed);
                std::future::pending::<hass_core::Result<Vec<String>>>().await
            })
        }
    });
    system_log
        .manager()
        .register_log_reader(log_reader)
        .unwrap();

    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = Capture(output.clone());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .with_filter(logger.filter()),
        )
        .with(system_log.layer().with_filter(logger.filter()))
        .try_init()
        .expect("installing the test tracing subscriber");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = router_with_features(
        ApiState {
            runtime: runtime.clone(),
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            logger: Some(logger.manager()),
            system_log: Some(system_log.manager()),
            ..ApiFeatures::default()
        },
        Some(shutdown_rx),
    );
    let (address, server) = start_server(app).await;
    let mut admin = connect(address, &owner_token.access_token).await;
    let mut ordinary = connect(address, &user_token.access_token).await;

    for (id, request) in [
        (1, json!({"id":1,"type":"system_log/list"})),
        (2, json!({"id":2,"type":"system_log/subscribe_raw"})),
    ] {
        let denied = command(&mut ordinary, request).await;
        assert_eq!(denied["id"], id);
        assert_eq!(denied["error"]["code"], "unauthorized");
    }

    let invalid = command(
        &mut admin,
        json!({"id":1,"type":"system_log/subscribe_raw","integration":7}),
    )
    .await;
    assert_eq!(invalid["error"]["code"], "invalid_format");
    let unknown = command(
        &mut admin,
        json!({
            "id":2,
            "type":"system_log/subscribe_raw",
            "integration":"missing_integration"
        }),
    )
    .await;
    assert_eq!(unknown["error"]["code"], "not_found");
    assert_eq!(unknown["error"]["message"], "Integration not found");

    let ack = command(
        &mut admin,
        json!({
            "id":3,
            "type":"system_log/subscribe_raw",
            "integration":"system_log_protocol"
        }),
    )
    .await;
    assert_eq!(ack["success"], true);
    assert_eq!(ack["result"], Value::Null);
    let initial = receive(&mut admin).await;
    assert_eq!(initial["id"], 3);
    assert_eq!(initial["type"], "event");
    assert!(
        initial["event"].as_array().unwrap().iter().any(|line| line
            == "2026-11-06T12:00:00.000Z INFO [system_log_protocol.reader] backlog fixture"),
        "initial backlog: {}",
        initial["event"]
    );

    tracing::warn!(target: "system_log_protocol.trace", "trace warning fixture");
    let written = write_log(
        &mut ordinary,
        3,
        "critical service fixture",
        "critical",
        "system_log_protocol.dynamic",
    )
    .await;
    assert_eq!(written["success"], true);
    let raw = wait_for_raw_lines(
        &mut admin,
        3,
        &["trace warning fixture", "critical service fixture"],
    )
    .await;
    let trace_line = raw
        .iter()
        .find(|line| line.contains("trace warning fixture"))
        .unwrap();
    let critical_line = raw
        .iter()
        .find(|line| line.contains("critical service fixture"))
        .unwrap();
    assert!(trace_line.contains("system_log_protocol.trace"));
    assert!(critical_line.contains("system_log_protocol.dynamic"));
    assert!(critical_line.contains("CRITICAL"));

    let rows = command(&mut admin, json!({"id":4,"type":"system_log/list"})).await;
    assert_eq!(rows["success"], true);
    let entries = rows["result"].as_array().unwrap();
    let critical = entries
        .iter()
        .find(|entry| entry["name"] == "system_log_protocol.dynamic")
        .unwrap();
    assert_eq!(critical["level"], "CRITICAL");
    assert_eq!(critical["message"], json!(["critical service fixture"]));
    assert!(critical["timestamp"].is_number());
    assert!(critical["first_occurred"].is_number());
    assert!(
        critical["source"]
            .as_array()
            .is_some_and(|source| source.len() == 2)
    );
    assert_eq!(critical["exception"], "");
    assert_eq!(critical["count"], 1);
    let warning = entries
        .iter()
        .find(|entry| entry["name"] == "system_log_protocol.trace")
        .unwrap();
    assert_eq!(warning["level"], "WARNING");
    assert_eq!(warning["message"], json!(["trace warning fixture"]));

    let suppressed = write_log(
        &mut ordinary,
        4,
        "suppressed dynamic fixture",
        "warning",
        "system_log_protocol.dynamic",
    )
    .await;
    assert_eq!(suppressed["success"], true);
    let rows = command(&mut admin, json!({"id":5,"type":"system_log/list"})).await;
    assert!(!rows["result"].as_array().unwrap().iter().any(|entry| {
        entry["message"]
            .to_string()
            .contains("suppressed dynamic fixture")
    }));

    let clear = command(
        &mut ordinary,
        json!({
            "id":5,
            "type":"call_service",
            "domain":"system_log",
            "service":"clear",
            "service_data":{}
        }),
    )
    .await;
    assert_eq!(clear["success"], true);
    let rows = command(&mut admin, json!({"id":6,"type":"system_log/list"})).await;
    assert_eq!(rows["result"], json!([]));

    let live_after_clear = write_log(
        &mut ordinary,
        6,
        "live after clear fixture",
        "info",
        "system_log_protocol.dynamic",
    )
    .await;
    assert_eq!(live_after_clear["success"], true);
    wait_for_raw_lines(&mut admin, 3, &["live after clear fixture"]).await;
    let rows = command(&mut admin, json!({"id":7,"type":"system_log/list"})).await;
    assert_eq!(rows["result"], json!([]));

    // This uses the real WebSocket service path while raw delivery and idle
    // socket reads continue. The action itself must remain outside the
    // transport-only span so its DEBUG record is still capturable.
    let debug_write = write_log(
        &mut ordinary,
        8,
        "debug service fixture",
        "debug",
        "system_log_protocol.dynamic",
    )
    .await;
    assert_eq!(debug_write["success"], true);
    let debug_raw = wait_for_raw_lines(&mut admin, 3, &["debug service fixture"]).await;
    let debug_line = debug_raw
        .iter()
        .find(|line| line.contains("debug service fixture"))
        .expect("real DEBUG system_log.write should reach the raw stream");
    assert!(debug_line.contains("DEBUG"));
    assert!(debug_line.contains("system_log_protocol.dynamic"));

    // Both peers share the process-wide tracing subscriber in this test.
    // Client I/O uses the same transport-only scope to keep its own tungstenite
    // TRACE records out of the server's logger; the server's pending read polls
    // are still exercised here and must not echo onto the subscribed stream.
    match timeout(Duration::from_millis(1_200), receive(&mut admin)).await {
        Err(_) => {}
        Ok(frame) => panic!("raw stream echoed transport activity while idle: {frame}"),
    }

    let unsubscribed = command(
        &mut admin,
        json!({"id":8,"type":"unsubscribe_events","subscription":3}),
    )
    .await;
    assert_eq!(unsubscribed["success"], true);
    let after_unsubscribe = write_log(
        &mut ordinary,
        9,
        "after unsubscribe fixture",
        "warning",
        "system_log_protocol.dynamic",
    )
    .await;
    assert_eq!(after_unsubscribe["success"], true);
    let ping = command(&mut admin, json!({"id":9,"type":"ping"})).await;
    assert_eq!(ping["id"], 9);
    assert_eq!(ping["type"], "pong");

    let console = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert_eq!(
        count(
            &console,
            "WARN system_log_protocol.trace: trace warning fixture"
        ),
        1,
        "console records containing the warning: {}",
        console
            .lines()
            .filter(|line| line.contains("trace warning fixture"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        count(
            &console,
            "ERROR hass_system_log::external: critical service fixture"
        ),
        1
    );
    assert_eq!(
        count(
            &console,
            "DEBUG hass_system_log::external: debug service fixture"
        ),
        1
    );
    assert!(console.contains("system_log_protocol.dynamic"));
    assert!(console.contains("CRITICAL"));
    assert!(!console.contains("hass_system_log::external: suppressed dynamic fixture"));

    let mut pending_admin = connect(address, &owner_token.access_token).await;
    let pending_ack = command(
        &mut pending_admin,
        json!({"id":1,"type":"system_log/subscribe_raw"}),
    )
    .await;
    assert_eq!(pending_ack["success"], true);
    wait_for_counter(&pending_read_starts, 1).await;
    let pending_unsubscribe = command(
        &mut pending_admin,
        json!({"id":2,"type":"unsubscribe_events","subscription":1}),
    )
    .await;
    assert_eq!(pending_unsubscribe["success"], true);
    assert_eq!(pending_read_drops.load(Ordering::Relaxed), 1);
    drop(pending_admin);

    let mut closing_admin = connect(address, &owner_token.access_token).await;
    let close_ack = command(
        &mut closing_admin,
        json!({"id":1,"type":"system_log/subscribe_raw"}),
    )
    .await;
    assert_eq!(close_ack["success"], true);
    wait_for_counter(&pending_read_starts, 2).await;
    closing_admin
        .send(Message::Close(None))
        .instrument(tracing::debug_span!("hass_raw_stream_delivery"))
        .await
        .unwrap();
    wait_for_counter(&pending_read_drops, 2).await;
    drop(closing_admin);

    let mut shutdown_admin = connect(address, &owner_token.access_token).await;
    let shutdown_ack = command(
        &mut shutdown_admin,
        json!({"id":1,"type":"system_log/subscribe_raw"}),
    )
    .await;
    assert_eq!(shutdown_ack["success"], true);
    wait_for_counter(&pending_read_starts, 3).await;
    shutdown_tx.send(true).unwrap();
    let closed = timeout(
        Duration::from_secs(2),
        shutdown_admin
            .next()
            .instrument(tracing::debug_span!("hass_raw_stream_delivery")),
    )
    .await
    .expect("shutdown did not close raw WebSocket")
    .expect("raw WebSocket ended without a close frame")
    .expect("raw WebSocket close failed");
    assert!(matches!(closed, Message::Close(_)));
    assert_eq!(pending_read_drops.load(Ordering::Relaxed), 3);

    server.abort();
    let _ = server.await;
    system_log.shutdown().await.unwrap();
    logger.shutdown().unwrap();
}
