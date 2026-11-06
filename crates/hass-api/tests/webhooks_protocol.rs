//! Real loopback coverage for public webhook dispatch and admin-only inventory.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, network::NetworkPolicy, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_webhook::{WebhookConfig, WebhookHandlerError, WebhookManager, WebhookRequest};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn receive(socket: &mut Socket) -> Value {
    let frame = timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("websocket response should arrive")
        .expect("websocket should remain open")
        .expect("websocket frame should be valid");
    serde_json::from_str(frame.to_text().expect("websocket frame should be text"))
        .expect("websocket response should be JSON")
}

async fn command(socket: &mut Socket, command: Value) -> Value {
    socket
        .send(Message::Text(command.to_string().into()))
        .await
        .expect("websocket command should send");
    receive(socket).await
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .expect("websocket should connect");
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(
            &mut socket,
            json!({"id":0,"type":"auth","access_token":token})
        )
        .await["type"],
        "auth_ok"
    );
    socket
}

#[tokio::test]
async fn public_webhook_protocol_normalizes_requests_and_hides_inventory() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "webhook-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(
            &owner.id,
            "readonly",
            "Read only",
            "webhook-user-fixture",
            false,
        )
        .unwrap();
    auth.update_user(
        &owner.id,
        &user.id,
        None,
        None,
        Some(&["system-read-only".into()]),
        None,
    )
    .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let mut public_events = runtime.subscribe();
    let manager = WebhookManager::new();
    let (sender, mut received) = mpsc::channel::<WebhookRequest>(4);
    let mut config = WebhookConfig::new("test", "private callback");
    config.local_only = true;
    config.allowed_methods = Some(vec!["POST".into()]);
    let _registration = manager
        .register("admin-only-webhook-secret", config, move |request| {
            let sender = sender.clone();
            async move {
                sender
                    .send(request)
                    .await
                    .map_err(|_| WebhookHandlerError)?;
                Ok(None)
            }
        })
        .unwrap();
    let _failing_registration = manager
        .register(
            "callback-failure-secret",
            WebhookConfig::new("test", "failing callback"),
            |_| async { Err(WebhookHandlerError) },
        )
        .unwrap();

    let app = router_with_features(
        ApiState {
            runtime,
            access_token: "webhook-admin-token".into(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth),
            network: NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
            webhooks: Some(manager.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let base = format!("http://{address}");
    let endpoint = format!("{base}/api/webhook/admin-only-webhook-secret");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();

    let json_response = client
        .post(format!("{endpoint}?origin=json-query&origin=second-query"))
        .header("x-fixture", "header-value")
        .json(&json!({"message":"private-body"}))
        .send()
        .await
        .unwrap();
    assert_eq!(json_response.status(), StatusCode::OK);
    assert!(json_response.bytes().await.unwrap().is_empty());
    let json_request = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(json_request.method, "POST");
    assert_eq!(json_request.webhook_id(), "admin-only-webhook-secret");
    assert_eq!(json_request.query.get("origin").unwrap(), "json-query");
    assert_eq!(
        json_request.headers.get("x-fixture").unwrap(),
        "header-value"
    );
    assert_eq!(
        json_request.json.as_ref().unwrap()["message"],
        "private-body"
    );
    assert_eq!(json_request.form.len(), 0);
    assert_eq!(
        json_request.origin.client_ip.unwrap().to_string(),
        "127.0.0.1"
    );
    assert!(json_request.origin.is_local);
    assert_eq!(json_request.body, br#"{"message":"private-body"}"#);

    let form_response = client
        .post(format!("{endpoint}?origin=form-query"))
        .form(&[("hello", "form-value")])
        .send()
        .await
        .unwrap();
    assert_eq!(form_response.status(), StatusCode::OK);
    let form_request = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(form_request.json.is_none());
    assert_eq!(form_request.form.get("hello").unwrap(), "form-value");
    assert_eq!(form_request.query.get("origin").unwrap(), "form-query");

    let malformed_json = client
        .post(&endpoint)
        .header("content-type", "application/json")
        .body("not-json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed_json.status(), StatusCode::OK);
    assert!(malformed_json.bytes().await.unwrap().is_empty());
    assert!(received.try_recv().is_err());

    let remote = client
        .post(&endpoint)
        .header("x-forwarded-for", "203.0.113.14")
        .body("remote-body")
        .send()
        .await
        .unwrap();
    assert_eq!(remote.status(), StatusCode::OK);
    assert!(received.try_recv().is_err());

    assert_eq!(
        client.get(&endpoint).send().await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let head = client.head(&endpoint).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.bytes().await.unwrap().is_empty());
    assert!(received.try_recv().is_err());
    assert_eq!(
        client
            .post(format!("{base}/api/webhook/not-registered"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let callback_failure = client
        .post(format!("{base}/api/webhook/callback-failure-secret"))
        .body("failure-payload-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(callback_failure.status(), StatusCode::OK);
    assert!(callback_failure.bytes().await.unwrap().is_empty());
    assert_eq!(
        client
            .post(&endpoint)
            .body(vec![b'x'; 1024 * 1024 + 1])
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(received.try_recv().is_err());

    let mut admin = connect(address, "webhook-admin-token").await;
    let inventory = command(&mut admin, json!({"id":1,"type":"webhook/list"})).await;
    assert_eq!(inventory["success"], true);
    assert_eq!(inventory["result"].as_array().unwrap().len(), 2);
    let listed = &inventory["result"][0];
    assert_eq!(listed["webhook_id"], "admin-only-webhook-secret");
    assert_eq!(listed["domain"], "test");
    assert_eq!(listed["name"], "private callback");
    let inventory_text = inventory.to_string();
    assert!(!inventory_text.contains("private-body"));
    assert!(!inventory_text.contains("header-value"));
    let invalid_inventory = command(
        &mut admin,
        json!({"id":2,"type":"webhook/list","unexpected":true}),
    )
    .await;
    assert_eq!(invalid_inventory["success"], false);

    let websocket_dispatch = command(
        &mut admin,
        json!({
            "id": 3,
            "type": "webhook/handle",
            "webhook_id": "admin-only-webhook-secret",
            "method": "POST",
            "query": "origin=websocket-query",
            "headers": {
                "Content-Type": "application/json",
                "X-Webhook-Fixture": "websocket-header"
            },
            "body": "{\"message\":\"websocket-body-secret\"}"
        }),
    )
    .await;
    assert_eq!(websocket_dispatch["success"], true);
    assert_eq!(websocket_dispatch["result"]["status"], 200);
    let websocket_request = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(websocket_request.method, "POST");
    assert_eq!(
        websocket_request.query.get("origin").unwrap(),
        "websocket-query"
    );
    assert_eq!(
        websocket_request.headers.get("x-webhook-fixture").unwrap(),
        "websocket-header"
    );
    assert_eq!(
        websocket_request.json.as_ref().unwrap()["message"],
        "websocket-body-secret"
    );
    assert_eq!(
        websocket_request.origin.client_ip.unwrap().to_string(),
        "127.0.0.1"
    );
    assert!(websocket_request.origin.is_local);
    admin.close(None).await.unwrap();
    let mut ordinary = connect(address, &user_token.access_token).await;
    let denied_inventory = command(&mut ordinary, json!({"id":1,"type":"webhook/list"})).await;
    assert_eq!(denied_inventory["success"], false);
    assert_eq!(denied_inventory["error"]["code"], "unauthorized");
    let accepted = command(
        &mut ordinary,
        json!({
            "id":2,"type":"webhook/handle","webhook_id":"admin-only-webhook-secret",
            "method":"POST","headers":{"content-type":"application/json"},
            "body":"{\"message\":\"readonly-capability-request\"}"
        }),
    )
    .await;
    assert_eq!(
        accepted["success"], true,
        "webhook/handle has no upstream admin requirement"
    );
    assert_eq!(accepted["result"]["status"], 200);
    let request = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request.json.unwrap()["message"],
        "readonly-capability-request"
    );
    ordinary.close(None).await.unwrap();

    while let Ok(event) = public_events.try_recv() {
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(!serialized.contains("admin-only-webhook-secret"));
        assert!(!serialized.contains("callback-failure-secret"));
        assert!(!serialized.contains("private-body"));
        assert!(!serialized.contains("remote-body"));
        assert!(!serialized.contains("failure-payload-secret"));
        assert!(!serialized.contains("websocket-body-secret"));
        assert!(!serialized.contains("readonly-capability-request"));
    }

    server.abort();
    let _ = server.await;
}
