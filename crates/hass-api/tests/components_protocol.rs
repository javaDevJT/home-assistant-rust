//! Loopback protocol coverage for dynamic loaded-component reporting.

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_core::{MemoryStore, Runtime, ServiceCall, ServiceResult};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const TOKEN: &str = "component-protocol-fixture-token";

async fn start_server(router: axum::Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
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

async fn command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    receive(socket).await
}

async fn connect(address: SocketAddr) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(&mut socket, json!({"type":"auth","access_token":TOKEN})).await["type"],
        "auth_ok"
    );
    socket
}

fn api_state(runtime: Runtime, config: Value) -> ApiState {
    ApiState {
        runtime,
        access_token: TOKEN.to_owned(),
        config,
    }
}

async fn get_json(client: &Client, address: SocketAddr, path: &str) -> reqwest::Response {
    client
        .get(format!("http://{address}{path}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn dynamic_receipts_are_live_sorted_shared_and_isolated_by_runtime() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let runtime_clone = runtime.clone();
    let receipt_handle = runtime_clone.loaded_components();
    let app = router_with_features(
        api_state(
            runtime.clone(),
            json!({
                "version":"component-fixture",
                "components":["declared.static", "fake.registered"]
            }),
        ),
        ApiFeatures {
            loaded_components: Some(receipt_handle.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let client = Client::new();

    let unauthorized = client
        .get(format!("http://{address}/api/components"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let empty_components = get_json(&client, address, "/api/components").await;
    assert_eq!(empty_components.status(), StatusCode::OK);
    assert_eq!(empty_components.json::<Value>().await.unwrap(), json!([]));
    let config = get_json(&client, address, "/api/config")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(config["version"], "component-fixture");
    assert_eq!(config["components"], json!([]));

    let mut socket = connect(address).await;
    let initial_ws = command(&mut socket, json!({"id":1,"type":"get_config"})).await;
    assert_eq!(initial_ws["success"], true);
    assert_eq!(initial_ws["result"]["components"], json!([]));

    // Add receipts after the server is already serving requests, through a
    // Runtime clone. Both clones and the API feature hold the same ledger.
    runtime_clone
        .loaded_components()
        .record_successful_component("spotify.media_player");
    receipt_handle.record_successful_component("mqtt.sensor");
    let expected = vec!["mqtt.sensor".to_owned(), "spotify.media_player".to_owned()];
    let expected_json = json!(["mqtt.sensor", "spotify.media_player"]);
    assert_eq!(runtime.loaded_components().components(), expected);
    let components = get_json(&client, address, "/api/components")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(components, expected_json);
    let config = get_json(&client, address, "/api/config")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(config["components"], expected_json);
    let updated_ws = command(&mut socket, json!({"id":2,"type":"get_config"})).await;
    assert_eq!(updated_ws["success"], true);
    assert_eq!(updated_ws["result"]["components"], expected_json);
    let invalid_ws = command(
        &mut socket,
        json!({"id":3,"type":"get_config","unexpected":true}),
    )
    .await;
    assert_eq!(invalid_ws["success"], false);
    assert_eq!(invalid_ws["error"]["code"], "invalid_format");

    runtime
        .register_service(
            "fixture",
            "noop",
            json!({}),
            Arc::new(|_runtime, call: &ServiceCall| {
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: None,
                    context: call.context.clone(),
                })
            }),
        )
        .unwrap();
    let service = client
        .post(format!("http://{address}/api/services/fixture/noop"))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(service.status(), StatusCode::OK);
    let after_service = get_json(&client, address, "/api/components")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(after_service, expected_json);

    // A new Runtime models a process restart: static declarations are still
    // present in its supplied config, but its explicit receipt provider starts
    // empty until that Runtime records successful setup.
    let restarted = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let restarted_components = restarted.loaded_components();
    let restarted_app = router_with_features(
        api_state(restarted, json!({"components":["declared.static"]})),
        ApiFeatures {
            loaded_components: Some(restarted_components.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (restarted_address, restarted_server) = start_server(restarted_app).await;
    let restarted_snapshot = get_json(&client, restarted_address, "/api/components")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(restarted_snapshot, json!([]));
    let restarted_config = get_json(&client, restarted_address, "/api/config")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(restarted_config["components"], json!([]));
    restarted_server.abort();

    server.abort();
}

#[tokio::test]
async fn static_callers_keep_configured_names_and_missing_names_default_empty() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let app = router_with_features(
        api_state(
            runtime,
            json!({
                "version":"static-fixture",
                "components":["sensor", "spotify.media_player"]
            }),
        ),
        ApiFeatures::default(),
        None,
    );
    let (address, server) = start_server(app).await;
    let client = Client::new();
    let static_names = json!(["sensor", "spotify.media_player"]);
    let components = get_json(&client, address, "/api/components")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(components, static_names);
    let config = get_json(&client, address, "/api/config")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(config["components"], static_names);

    let mut socket = connect(address).await;
    let ws_config = command(&mut socket, json!({"id":1,"type":"get_config"})).await;
    assert_eq!(ws_config["result"]["components"], static_names);
    server.abort();

    let missing_runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let missing_app = router_with_features(
        api_state(missing_runtime, json!({"version":"missing-static-fixture"})),
        ApiFeatures::default(),
        None,
    );
    let (missing_address, missing_server) = start_server(missing_app).await;
    let missing_components = get_json(&client, missing_address, "/api/components")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(missing_components, json!([]));
    let missing_config = get_json(&client, missing_address, "/api/config")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(missing_config, json!({"version":"missing-static-fixture"}));
    missing_server.abort();
}
