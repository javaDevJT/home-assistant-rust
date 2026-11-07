//! Loopback protocol fixtures for the native Network WebSocket commands.
//!
//! The injected providers make every adapter and route result
//! deterministic; the only sockets opened here are the loopback WebSocket
//! fixture sockets.

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_config::ConfigEntries;
use hass_core::{MemoryStore, Runtime};
use hass_network::{
    AdapterAddresses, AdapterEnumerator, Ipv4AddressRecord, Ipv6AddressRecord, NetworkIntegration,
    NetworkManager, RouteSourceResolver,
};
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const CLIENT_ID: &str = "http://localhost/";

struct Tokens {
    auth: AuthManager,
    owner: String,
    ordinary: String,
    reader: String,
}

struct FixtureEnumerator;

impl AdapterEnumerator for FixtureEnumerator {
    fn enumerate(&self) -> hass_core::Result<Vec<AdapterAddresses>> {
        Ok(fixture_adapters())
    }
}

struct FixtureRouteResolver;

impl RouteSourceResolver for FixtureRouteResolver {
    fn source_ip(&self, _target: Ipv4Addr) -> Option<Ipv4Addr> {
        Some(Ipv4Addr::new(10, 0, 0, 2))
    }
}

fn fixture_adapters() -> Vec<AdapterAddresses> {
    vec![
        AdapterAddresses {
            name: "fixture-a".into(),
            index: Some(3),
            ipv4: vec![Ipv4AddressRecord {
                address: Ipv4Addr::new(192, 168, 1, 10),
                network_prefix: 24,
            }],
            ipv6: vec![Ipv6AddressRecord {
                address: "fe80::3".parse().unwrap(),
                flowinfo: 7,
                scope_id: 3,
                network_prefix: 64,
            }],
        },
        AdapterAddresses {
            name: "fixture-b".into(),
            index: Some(7),
            ipv4: vec![Ipv4AddressRecord {
                address: Ipv4Addr::new(10, 0, 0, 2),
                network_prefix: 24,
            }],
            ipv6: Vec::new(),
        },
    ]
}

fn start_network(runtime: Runtime) -> (NetworkIntegration, NetworkManager) {
    let integration = NetworkIntegration::start(
        runtime,
        Arc::new(FixtureEnumerator),
        Arc::new(FixtureRouteResolver),
    )
    .unwrap();
    let manager = integration.manager();
    (integration, manager)
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Network Owner", "network-owner-password")
        .unwrap();
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "network-user-password",
            false,
        )
        .unwrap();
    let reader_groups = vec!["system-read-only".to_owned()];
    let reader = auth
        .create_user_without_credentials(&owner.id, "Read-only User", Some(&reader_groups), None)
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
        // These values are deliberately different from persisted CoreConfig.
        config: json!({
            "internal_url": "https://fabricated.internal.invalid",
            "external_url": "https://fabricated.external.invalid",
        }),
    }
}

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

async fn command(socket: &mut Socket, id: u64, kind: &str, extra: Value) -> Value {
    let mut request = json!({ "id": id, "type": kind });
    if let (Some(target), Some(fields)) = (request.as_object_mut(), extra.as_object()) {
        target.extend(fields.clone());
    }
    send(socket, request).await;
    receive(socket).await
}

async fn result(socket: &mut Socket, id: u64, kind: &str, extra: Value) -> Value {
    let frame = command(socket, id, kind, extra).await;
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], true, "{frame}");
    frame["result"].clone()
}

async fn assert_invalid(socket: &mut Socket, id: u64, kind: &str, extra: Value) {
    let frame = command(socket, id, kind, extra).await;
    assert_eq!(frame["success"], false, "{frame}");
    assert_eq!(frame["error"]["code"], "invalid_format");
    assert_eq!(frame["error"]["message"], "Invalid network command");
}

async fn blocking<T, F>(operation: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation).await.unwrap()
}

#[tokio::test]
async fn websocket_commands_match_roles_wire_shapes_and_adapter_selection() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let mut core = entries.core_config();
    core.internal_url = Some("https://saved.internal.example".into());
    core.external_url = Some("https://saved.external.example".into());
    entries.update_core_config(core).unwrap();
    let (integration, manager) = start_network(runtime.clone());
    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            config_entries: Some(entries),
            network_adapters: Some(manager),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;

    let mut owner = connect(address, &tokens.owner).await;
    let snapshot = result(&mut owner, 1, "network", json!({})).await;
    assert_eq!(snapshot["configured_adapters"], json!([]));
    assert_eq!(snapshot["adapters"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["adapters"][0]["name"], "fixture-a");
    assert_eq!(snapshot["adapters"][0]["index"], 3);
    assert_eq!(
        snapshot["adapters"][0]["ipv4"][0],
        json!({
            "address": "192.168.1.10",
            "network_prefix": 24,
        })
    );
    assert_eq!(
        snapshot["adapters"][0]["ipv6"][0],
        json!({
            "address": "fe80::3",
            "flowinfo": 7,
            "scope_id": 3,
            "network_prefix": 64,
        })
    );
    assert_eq!(snapshot["adapters"][1]["default"], true);
    assert_eq!(snapshot["adapters"][1]["enabled"], true);

    let configured = result(
        &mut owner,
        2,
        "network/configure",
        json!({ "config": { "configured_adapters": ["fixture-a"] } }),
    )
    .await;
    assert_eq!(configured, json!({ "configured_adapters": ["fixture-a"] }));
    let selected = result(&mut owner, 3, "network", json!({})).await;
    assert_eq!(selected["adapters"][0]["enabled"], true);
    assert_eq!(selected["adapters"][1]["enabled"], false);

    // Exact matching enables surviving configured names; an all-unknown list
    // falls back to automatic source selection.
    let mixed = result(
        &mut owner,
        4,
        "network/configure",
        json!({ "config": { "configured_adapters": ["missing", "fixture-a"] } }),
    )
    .await;
    assert_eq!(
        mixed["configured_adapters"],
        json!(["missing", "fixture-a"])
    );
    let selected = result(&mut owner, 5, "network", json!({})).await;
    assert_eq!(selected["adapters"][0]["enabled"], true);
    assert_eq!(selected["adapters"][1]["enabled"], false);

    let unknown = result(
        &mut owner,
        6,
        "network/configure",
        json!({ "config": { "configured_adapters": ["missing"] } }),
    )
    .await;
    assert_eq!(unknown["configured_adapters"], json!(["missing"]));
    let selected = result(&mut owner, 7, "network", json!({})).await;
    assert_eq!(selected["adapters"][0]["enabled"], false);
    assert_eq!(selected["adapters"][1]["enabled"], true);

    let mut ordinary = connect(address, &tokens.ordinary).await;
    let denied = command(&mut ordinary, 8, "network", json!({})).await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");
    let denied = command(
        &mut ordinary,
        9,
        "network/configure",
        json!({ "config": { "configured_adapters": [] } }),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");

    let mut reader = connect(address, &tokens.reader).await;
    let denied = command(&mut reader, 10, "network", json!({})).await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");
    let denied = command(
        &mut reader,
        11,
        "network/configure",
        json!({ "config": { "configured_adapters": [] } }),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");

    // `network/url` is available to every authenticated role and reads the
    // persisted CoreConfig, not the deliberately conflicting API metadata.
    for (id, socket) in [(12, &mut owner), (13, &mut ordinary), (14, &mut reader)] {
        assert_eq!(
            result(socket, id, "network/url", json!({})).await,
            json!({
                "internal": "https://saved.internal.example",
                "external": "https://saved.external.example",
                "cloud": null,
            })
        );
    }

    // These commands are correlated replies only; no Network event is pushed.
    assert!(
        timeout(Duration::from_millis(100), owner.next())
            .await
            .is_err()
    );
    owner.close(None).await.unwrap();
    ordinary.close(None).await.unwrap();
    reader.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    drop(integration);
}

#[tokio::test]
async fn validation_rejects_unknown_fields_and_wrong_nested_types() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let (integration, manager) = start_network(runtime.clone());
    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            network_adapters: Some(manager),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let mut owner = connect(address, &tokens.owner).await;

    assert_invalid(&mut owner, 1, "network", json!({ "extra": true })).await;
    assert_invalid(&mut owner, 2, "network/configure", json!({ "extra": true })).await;
    assert_invalid(
        &mut owner,
        3,
        "network/configure",
        json!({ "config": { "extra": true } }),
    )
    .await;
    assert_invalid(
        &mut owner,
        4,
        "network/configure",
        json!({ "config": null }),
    )
    .await;
    assert_invalid(
        &mut owner,
        5,
        "network/configure",
        json!({ "config": { "configured_adapters": "fixture-a" } }),
    )
    .await;
    assert_invalid(
        &mut owner,
        6,
        "network/configure",
        json!({ "config": { "configured_adapters": ["fixture-a", 3] } }),
    )
    .await;
    assert_invalid(&mut owner, 7, "network/url", json!({ "extra": true })).await;

    // Both omitted levels default exactly as the pinned command schema does.
    assert_eq!(
        result(&mut owner, 8, "network/configure", json!({})).await,
        json!({ "configured_adapters": [] })
    );
    assert_eq!(
        result(&mut owner, 9, "network/configure", json!({ "config": {} })).await,
        json!({ "configured_adapters": [] })
    );

    owner.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    drop(integration);
}

#[tokio::test]
async fn saved_preferences_survive_owner_restart_and_old_manager_goes_weak() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let (integration, manager) = start_network(runtime.clone());
    let configured = blocking({
        let manager = manager.clone();
        move || manager.configure(vec!["fixture-a".into()]).unwrap()
    })
    .await;
    assert_eq!(configured, vec!["fixture-a"]);

    drop(integration);
    let stale_manager = manager.clone();
    assert!(blocking(move || stale_manager.adapters().is_err()).await);
    drop(manager);
    drop(runtime);

    // Reopening the same Store proves persistence through a new Runtime and a
    // new strong integration owner, rather than a retained in-memory manager.
    let restarted_runtime = Runtime::new(store).unwrap();
    let (_restarted_integration, restarted_manager) = start_network(restarted_runtime);
    let (configured, adapters) = blocking(move || {
        Ok::<_, hass_core::Error>((
            restarted_manager.configured_adapters()?,
            restarted_manager.adapters()?,
        ))
    })
    .await
    .unwrap();
    assert_eq!(configured, vec!["fixture-a"]);
    assert!(
        adapters
            .iter()
            .find(|adapter| adapter.name == "fixture-a")
            .unwrap()
            .enabled
    );
    assert!(
        !adapters
            .iter()
            .find(|adapter| adapter.name == "fixture-b")
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn missing_config_returns_null_urls_and_missing_manager_is_generic() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let tokens = auth_tokens(&runtime);
    let app = router_with_features(
        api_state(runtime),
        ApiFeatures {
            auth: Some(tokens.auth.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let mut owner = connect(address, &tokens.owner).await;
    assert_eq!(
        result(&mut owner, 1, "network/url", json!({})).await,
        json!({ "internal": null, "external": null, "cloud": null })
    );
    let unavailable = command(&mut owner, 2, "network", json!({})).await;
    assert_eq!(unavailable["success"], false);
    assert_eq!(unavailable["error"]["code"], "not_found");
    assert_eq!(
        unavailable["error"]["message"],
        "Network integration is not configured"
    );
    owner.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}
