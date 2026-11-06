//! Real HTTP mutation and authenticated WebSocket notification feed checks.
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_notifications::NotificationIntegration;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
async fn receive(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(4), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}
async fn command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    receive(socket).await
}

#[tokio::test]
async fn ordinary_user_receives_atomic_current_and_live_private_updates_and_unsubscribes() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "notification-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(
            &owner.id,
            "user",
            "User",
            "notification-user-fixture",
            false,
        )
        .unwrap();
    let tokens = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let mut integration = NotificationIntegration::start(runtime.clone()).unwrap();
    let manager = integration.manager();
    manager
        .create("Before subscribe", None, Some("existing"))
        .unwrap();
    let mut public_events = runtime.subscribe();
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth),
            notifications: Some(manager.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let base = format!("http://{address}/api/services/persistent_notification");
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(
            &mut socket,
            json!({"type":"auth","access_token":tokens.access_token})
        )
        .await["type"],
        "auth_ok"
    );
    let ack = command(
        &mut socket,
        json!({"id":1,"type":"persistent_notification/subscribe"}),
    )
    .await;
    assert_eq!(ack["success"], true);
    assert!(ack["result"].is_null());
    let initial = receive(&mut socket).await;
    assert_eq!(initial["id"], 1);
    assert_eq!(initial["event"]["type"], "current");
    assert_eq!(
        initial["event"]["notifications"]["existing"]["message"],
        "Before subscribe"
    );
    let response=client.post(format!("{base}/create")).bearer_auth(&tokens.access_token).json(&json!({"message":"Created on the native service","title":"Fixture","notification_id":"wire"})).send().await.unwrap();
    assert!(response.status().is_success());
    let added = receive(&mut socket).await;
    assert_eq!(added["event"]["type"], "added");
    assert_eq!(added["event"]["notifications"]["wire"]["title"], "Fixture");
    assert!(added["event"]["notifications"]["wire"]["created_at"].is_string());
    let response = client
        .post(format!("{base}/create"))
        .bearer_auth(&tokens.access_token)
        .json(&json!({"message":"Updated","notification_id":"wire"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let updated = receive(&mut socket).await;
    assert_eq!(updated["event"]["type"], "updated");
    assert_eq!(
        updated["event"]["notifications"]["wire"]["message"],
        "Updated"
    );
    assert!(updated["event"]["notifications"]["wire"]["title"].is_null());
    let list = command(
        &mut socket,
        json!({"id":2,"type":"persistent_notification/get"}),
    )
    .await;
    assert_eq!(list["success"], true);
    assert_eq!(list["result"][0]["notification_id"], "existing");
    assert_eq!(list["result"][1]["notification_id"], "wire");
    let response = client
        .post(format!("{base}/dismiss"))
        .bearer_auth(&tokens.access_token)
        .json(&json!({"notification_id":"wire"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let removed = receive(&mut socket).await;
    assert_eq!(removed["event"]["type"], "removed");
    assert_eq!(
        removed["event"]["notifications"]["wire"]["message"],
        "Updated"
    );
    let ack = command(
        &mut socket,
        json!({"id":3,"type":"unsubscribe_events","subscription":1}),
    )
    .await;
    assert_eq!(ack["success"], true);
    let response = client
        .post(format!("{base}/create"))
        .bearer_auth(&tokens.access_token)
        .json(&json!({"message":"After unsubscribe","notification_id":"after"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    // A stale notification frame would arrive before this command response.
    let list = command(
        &mut socket,
        json!({"id":4,"type":"persistent_notification/get"}),
    )
    .await;
    assert_eq!(list["id"], 4);
    assert_eq!(list["result"][1]["notification_id"], "after");
    while let Ok(event) = public_events.try_recv() {
        assert_ne!(
            event.event_type,
            hass_notifications::SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED
        );
    }
    socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    integration.shutdown().unwrap();
}
