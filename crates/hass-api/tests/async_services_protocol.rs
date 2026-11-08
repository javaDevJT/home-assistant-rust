use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{
    AsyncServiceHandler, Error, MemoryStore, Runtime, ServiceCall, ServiceHandler,
    ServiceResponseSupport, ServiceResult,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "http://localhost:3000/";
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

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
    let frame = timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("WebSocket response timed out")
        .expect("WebSocket closed before response")
        .expect("WebSocket receive failed");
    serde_json::from_str(frame.to_text().expect("expected a text WebSocket frame")).unwrap()
}

async fn command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    receive(socket).await
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(&mut socket, json!({"type":"auth","access_token":token})).await["type"],
        "auth_ok"
    );
    socket
}

fn app_state(runtime: Runtime, access_token: &str) -> ApiState {
    ApiState {
        runtime,
        access_token: access_token.to_owned(),
        config: json!({"version":"async-service-fixture"}),
    }
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn http_service(
    client: &Client,
    address: SocketAddr,
    token: &str,
    service: &str,
    query: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let query = query.unwrap_or_default();
    client
        .post(format!(
            "http://{address}/api/services/fixture/{service}{query}"
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn result_for(call: &ServiceCall, response: Option<Value>) -> ServiceResult {
    ServiceResult {
        changed_states: Vec::new(),
        response,
        context: call.context.clone(),
    }
}

fn register_async_echo(
    runtime: &Runtime,
    service: &str,
    support: ServiceResponseSupport,
    calls: Arc<AtomicUsize>,
) {
    let response_support = support;
    let handler: AsyncServiceHandler = Arc::new(move |runtime, call| {
        let calls = calls.clone();
        let response_support = response_support;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            // Reenter the runtime after an await. The dispatcher must not hold
            // the service registry lock while an async provider is running.
            let _services = runtime.services();
            let response = if response_support == ServiceResponseSupport::None {
                None
            } else {
                Some(json!({
                    "echo": call.data.get("value").cloned().unwrap_or(Value::Null)
                }))
            };
            Ok(result_for(&call, response))
        })
    });
    runtime
        .register_async_service_with_response_support(
            "fixture",
            service,
            json!({}),
            support,
            handler,
        )
        .unwrap();
}

fn register_sync_echo(
    runtime: &Runtime,
    service: &str,
    support: ServiceResponseSupport,
    calls: Arc<AtomicUsize>,
) {
    let handler: ServiceHandler = Arc::new(move |_runtime, call| {
        calls.fetch_add(1, Ordering::SeqCst);
        let response = Some(json!({"echo":call.data.get("value").cloned().unwrap_or(Value::Null)}));
        Ok(result_for(call, response))
    });
    runtime
        .register_service_with_response_support("fixture", service, json!({}), support, handler)
        .unwrap();
}

#[tokio::test]
async fn http_and_websocket_enforce_response_capabilities_and_keep_envelopes() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let none_calls = Arc::new(AtomicUsize::new(0));
    let optional_calls = Arc::new(AtomicUsize::new(0));
    let only_calls = Arc::new(AtomicUsize::new(0));
    register_async_echo(
        &runtime,
        "none",
        ServiceResponseSupport::None,
        none_calls.clone(),
    );
    register_sync_echo(
        &runtime,
        "optional",
        ServiceResponseSupport::Optional,
        optional_calls.clone(),
    );
    register_async_echo(
        &runtime,
        "only",
        ServiceResponseSupport::Only,
        only_calls.clone(),
    );

    let app = router_with_features(
        app_state(runtime, "service-fixture-token"),
        ApiFeatures::default(),
        None,
    );
    let (address, server) = start_server(app).await;
    let client = client();

    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "none",
        None,
        json!({"value":"http-none"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), json!([]));
    assert_eq!(none_calls.load(Ordering::SeqCst), 1);

    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "none",
        Some("?return_response"),
        json!({"value":"not-dispatched"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap()["message"],
        "Service does not support responses. Remove return_response from request."
    );
    assert_eq!(none_calls.load(Ordering::SeqCst), 1);

    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "optional",
        None,
        json!({"value":"optional-http"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), json!([]));
    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "optional",
        Some("?return_response"),
        json!({"value":"optional-http-response"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"changed_states":[],"service_response":{"echo":"optional-http-response"}})
    );

    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "only",
        None,
        json!({"value":"not-dispatched"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap()["message"],
        "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters."
    );
    assert_eq!(only_calls.load(Ordering::SeqCst), 0);
    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "only",
        Some("?return_response"),
        json!({"value":"only-http"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"changed_states":[],"service_response":{"echo":"only-http"}})
    );

    let mut socket = connect(address, "service-fixture-token").await;
    let response = command(
        &mut socket,
        json!({"id":1,"type":"call_service","domain":"fixture","service":"none"}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["changed_states"], json!([]));
    assert!(response["result"]["context"].is_object());
    assert_eq!(response["result"]["response"], Value::Null);

    let response = command(
        &mut socket,
        json!({"id":2,"type":"call_service","domain":"fixture","service":"none","return_response":true}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "service_validation_error");
    assert_eq!(
        response["error"]["message"],
        "Service does not support responses. Remove return_response from request."
    );
    assert_eq!(none_calls.load(Ordering::SeqCst), 2);

    let response = command(
        &mut socket,
        json!({"id":3,"type":"call_service","domain":"fixture","service":"optional","service_data":{"value":"optional-ws"}}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert!(response["result"]["context"].is_object());
    assert_eq!(response["result"]["response"], Value::Null);
    let response = command(
        &mut socket,
        json!({"id":4,"type":"call_service","domain":"fixture","service":"optional","return_response":true,"service_data":{"value":"optional-ws-response"}}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(
        response["result"]["response"],
        json!({"echo":"optional-ws-response"})
    );

    let response = command(
        &mut socket,
        json!({"id":5,"type":"call_service","domain":"fixture","service":"only"}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "service_validation_error");
    assert_eq!(
        response["error"]["message"],
        "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters."
    );
    assert_eq!(only_calls.load(Ordering::SeqCst), 1);
    let response = command(
        &mut socket,
        json!({"id":6,"type":"call_service","domain":"fixture","service":"only","return_response":true,"service_data":{"value":"only-ws"}}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["response"], json!({"echo":"only-ws"}));

    let before_invalid = optional_calls.load(Ordering::SeqCst);
    let response = command(
        &mut socket,
        json!({"id":7,"type":"call_service","domain":"fixture","service":"optional","return_response":"true"}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "invalid_format");
    assert_eq!(
        response["error"]["message"],
        "return_response must be a boolean"
    );
    let response = command(
        &mut socket,
        json!({"id":8,"type":"call_service","domain":"fixture","service":"optional","extra":true}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "invalid_format");
    assert_eq!(response["error"]["message"], "Unexpected fields");
    assert_eq!(optional_calls.load(Ordering::SeqCst), before_invalid);

    let response = http_service(
        &client,
        address,
        "service-fixture-token",
        "missing",
        None,
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.json::<Value>().await.unwrap()["message"],
        "Service not found."
    );
    let response = command(
        &mut socket,
        json!({"id":9,"type":"call_service","domain":"fixture","service":"missing"}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "service_not_found");
    assert_eq!(response["error"]["message"], "Service not found");

    socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn service_authorization_context_and_target_resolution_are_shared() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Service Owner", "service-owner-password")
        .unwrap();
    let controller_groups = vec!["system-users".to_owned()];
    let controller = auth
        .create_user_without_credentials(
            &owner.id,
            "Service controller",
            Some(&controller_groups),
            None,
        )
        .unwrap();
    let reader_groups = vec!["system-read-only".to_owned()];
    let reader = auth
        .create_user_without_credentials(&owner.id, "Service reader", Some(&reader_groups), None)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let controller_token = auth
        .issue_tokens(&controller.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let reader_token = auth
        .issue_tokens(&reader.id, CLIENT_ID, None)
        .unwrap()
        .access_token;

    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let resolver_counter = resolver_calls.clone();
    let resolver =
        runtime.install_service_target_resolver_if_absent(Arc::new(move |_runtime, call| {
            resolver_counter.fetch_add(1, Ordering::SeqCst);
            if call.target.remove("area_id").is_some() {
                call.target
                    .insert("entity_id".into(), json!(["light.fixture"]));
            }
            Ok(true)
        }));
    assert!(resolver.is_some());

    let calls = Arc::new(AtomicUsize::new(0));
    let contexts = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let call_counter = calls.clone();
    let observed_contexts = contexts.clone();
    let handler: AsyncServiceHandler = Arc::new(move |runtime, call| {
        let call_counter = call_counter.clone();
        let observed_contexts = observed_contexts.clone();
        Box::pin(async move {
            call_counter.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let probe = call
                .data
                .get("probe")
                .and_then(Value::as_str)
                .expect("fixture probe name")
                .to_owned();
            // A write-lock reentry catches a dispatcher that holds a service
            // registry lock while awaiting this handler.
            let registration_name = format!("probe_{probe}");
            let unused_handler: ServiceHandler =
                Arc::new(|_runtime, call| Ok(result_for(call, None)));
            runtime.register_service("reentry", &registration_name, json!({}), unused_handler)?;
            observed_contexts
                .lock()
                .unwrap()
                .push(call.context.user_id.clone());
            assert_eq!(
                call.target.get("entity_id"),
                Some(&json!(["light.fixture"]))
            );
            assert!(!call.target.contains_key("area_id"));
            Ok(result_for(&call, Some(json!({"called":true}))))
        })
    });
    runtime
        .register_async_service_with_response_support(
            "fixture",
            "echo",
            json!({}),
            ServiceResponseSupport::Optional,
            handler,
        )
        .unwrap();
    let admin_calls = Arc::new(AtomicUsize::new(0));
    let admin_counter = admin_calls.clone();
    let admin_handler: ServiceHandler = Arc::new(move |_runtime, call| {
        admin_counter.fetch_add(1, Ordering::SeqCst);
        Ok(result_for(call, None))
    });
    runtime
        .register_service_with_response_support(
            "recorder",
            "maintenance",
            json!({}),
            ServiceResponseSupport::None,
            admin_handler,
        )
        .unwrap();

    let app = router_with_features(
        app_state(runtime.clone(), ""),
        ApiFeatures {
            auth: Some(auth),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let client = client();

    for (token, probe) in [
        (&controller_token, "controller_http"),
        (&owner_token, "owner_http"),
    ] {
        let response = client
            .post(format!("http://{address}/api/services/fixture/echo"))
            .bearer_auth(token)
            .json(&json!({"target":{"area_id":"fixture-area"},"probe":probe}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!([]));
    }

    let mut controller_socket = connect(address, &controller_token).await;
    let response = command(
        &mut controller_socket,
        json!({"id":1,"type":"call_service","domain":"fixture","service":"echo","target":{"area_id":"fixture-area"},"service_data":{"probe":"controller_ws"}}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["context"]["user_id"], controller.id);
    let denied = command(
        &mut controller_socket,
        json!({"id":2,"type":"call_service","domain":"recorder","service":"maintenance"}),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");
    let denied_http = client
        .post(format!(
            "http://{address}/api/services/recorder/maintenance"
        ))
        .bearer_auth(&controller_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied_http.status(), StatusCode::FORBIDDEN);

    let mut owner_socket = connect(address, &owner_token).await;
    let response = command(
        &mut owner_socket,
        json!({"id":1,"type":"call_service","domain":"recorder","service":"maintenance"}),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["context"]["user_id"], owner.id);

    let reader_response = client
        .post(format!("http://{address}/api/services/fixture/echo"))
        .bearer_auth(&reader_token)
        .json(&json!({"probe":"reader_http"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reader_response.status(), StatusCode::FORBIDDEN);
    let mut reader_socket = connect(address, &reader_token).await;
    let denied = command(
        &mut reader_socket,
        json!({"id":1,"type":"call_service","domain":"fixture","service":"echo"}),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");

    let admin_response = client
        .post(format!(
            "http://{address}/api/services/recorder/maintenance"
        ))
        .bearer_auth(&owner_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(admin_response.status(), StatusCode::OK);

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(admin_calls.load(Ordering::SeqCst), 2);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 5);
    assert_eq!(
        *contexts.lock().unwrap(),
        vec![
            Some(controller.id.clone()),
            Some(owner.id.clone()),
            Some(controller.id.clone()),
        ]
    );

    controller_socket.close(None).await.unwrap();
    owner_socket.close(None).await.unwrap();
    reader_socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    drop(resolver);
}

struct DropSignal(Arc<AtomicUsize>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn api_shutdown_drops_blocked_http_and_websocket_service_futures() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let started_notify = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let handler_started = started.clone();
    let handler_notify = started_notify.clone();
    let handler_dropped = dropped.clone();
    let handler: AsyncServiceHandler = Arc::new(move |_runtime, _call| {
        let handler_started = handler_started.clone();
        let handler_notify = handler_notify.clone();
        let handler_dropped = handler_dropped.clone();
        Box::pin(async move {
            let _drop_signal = DropSignal(handler_dropped);
            handler_started.fetch_add(1, Ordering::SeqCst);
            handler_notify.notify_one();
            std::future::pending::<()>().await;
            unreachable!("pending service future should be cancelled by shutdown")
        })
    });
    runtime
        .register_async_service_with_response_support(
            "fixture",
            "blocked",
            json!({}),
            ServiceResponseSupport::None,
            handler,
        )
        .unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = router_with_features(
        app_state(runtime, "shutdown-fixture-token"),
        ApiFeatures::default(),
        Some(shutdown_rx),
    );
    let (address, server) = start_server(app).await;
    let client = client();
    let http_client = client.clone();
    let http = tokio::spawn(async move {
        http_client
            .post(format!("http://{address}/api/services/fixture/blocked"))
            .bearer_auth("shutdown-fixture-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
    });
    let mut socket = connect(address, "shutdown-fixture-token").await;
    socket
        .send(Message::Text(
            json!({"id":1,"type":"call_service","domain":"fixture","service":"blocked"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    timeout(Duration::from_secs(4), async {
        while started.load(Ordering::SeqCst) < 2 {
            started_notify.notified().await;
        }
    })
    .await
    .expect("both transports should enter the async handler before shutdown");
    shutdown_tx.send(true).unwrap();

    let http_response = timeout(Duration::from_secs(4), http)
        .await
        .expect("HTTP service call must stop waiting during shutdown")
        .unwrap();
    assert_eq!(http_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        http_response.json::<Value>().await.unwrap()["message"],
        "API is shutting down."
    );
    let ws_response = receive(&mut socket).await;
    assert_eq!(ws_response["success"], false);
    assert_eq!(ws_response["error"]["code"], "home_assistant_error");
    assert_eq!(ws_response["error"]["message"], "API is shutting down.");
    timeout(Duration::from_secs(3), async {
        while dropped.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("service futures must be dropped when transport shutdown wins");

    socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn service_provider_errors_do_not_leak_request_or_provider_details() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let handler: ServiceHandler =
        Arc::new(|_runtime, _call| Err(Error::Service("provider-private-detail".into())));
    runtime
        .register_service_with_response_support(
            "fixture",
            "failure",
            json!({}),
            ServiceResponseSupport::None,
            handler,
        )
        .unwrap();
    let app = router_with_features(
        app_state(runtime, "error-fixture-token"),
        ApiFeatures::default(),
        None,
    );
    let (address, server) = start_server(app).await;
    let client = client();

    let response = http_service(
        &client,
        address,
        "error-fixture-token",
        "failure",
        None,
        json!({"secret":"request-private-detail"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["message"], "Internal server error");
    assert!(!body.to_string().contains("request-private-detail"));
    assert!(!body.to_string().contains("provider-private-detail"));

    let mut socket = connect(address, "error-fixture-token").await;
    let response = command(
        &mut socket,
        json!({"id":1,"type":"call_service","domain":"fixture","service":"failure","service_data":{"secret":"request-private-detail"}}),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "service_error");
    assert_eq!(response["error"]["message"], "Service call failed");
    assert!(!response.to_string().contains("request-private-detail"));
    assert!(!response.to_string().contains("provider-private-detail"));

    socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn semantic_target_and_user_facing_provider_errors_have_source_error_categories() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    runtime
        .register_service(
            "fixture",
            "user_error",
            json!({}),
            Arc::new(|_, _| Err(Error::HomeAssistant("The player is not ready.".into()))),
        )
        .unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let observed = effects.clone();
    runtime
        .register_entity_async_service(
            "fixture-owner",
            "fixture",
            "targeted",
            json!({}),
            Arc::new(|id| id == "media_player.known"),
            Arc::new(move |_, call| {
                let observed = observed.clone();
                Box::pin(async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context,
                    })
                })
            }),
        )
        .unwrap();
    let app = router_with_features(
        app_state(runtime.clone(), "source-error-token"),
        ApiFeatures::default(),
        None,
    );
    let (address, server) = start_server(app).await;
    let client = client();
    let result = http_service(
        &client,
        address,
        "source-error-token",
        "user_error",
        None,
        json!({}),
    )
    .await;
    assert_eq!(result.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // HomeAssistantError is deliberately user-facing in the pinned API; its
    // message is preserved, unlike an untyped internal provider failure.
    assert_eq!(
        result.json::<Value>().await.unwrap()["message"],
        "The player is not ready."
    );
    let mut socket = connect(address, "source-error-token").await;
    let result = command(
        &mut socket,
        json!({"id":1,"type":"call_service","domain":"fixture","service":"user_error"}),
    )
    .await;
    assert_eq!(result["error"]["code"], "home_assistant_error");
    let result = command(&mut socket, json!({"id":2,"type":"call_service","domain":"fixture","service":"targeted","target":{"entity_id":"media_player.missing"}})).await;
    assert_eq!(result["error"]["code"], "service_validation_error");
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let result = command(&mut socket, json!({"id":3,"type":"call_service","domain":"fixture","service":"targeted","return_response":"invalid"})).await;
    assert_eq!(result["error"]["code"], "invalid_format");
    socket.close(None).await.unwrap();
    runtime.shutdown_service_tasks_async().await.unwrap();
    server.abort();
    let _ = server.await;
}
