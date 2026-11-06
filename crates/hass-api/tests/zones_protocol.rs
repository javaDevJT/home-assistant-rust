//! Authenticated native zone collection CRUD and private subscription protocol.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_zone::{HomeZoneConfig, ZoneConfig, ZoneIntegration};
use serde_json::{Value, json};
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

async fn command(socket: &mut Socket, command: Value) -> Value {
    socket
        .send(Message::Text(command.to_string().into()))
        .await
        .unwrap();
    receive(socket).await
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
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

fn zone(name: &str) -> ZoneConfig {
    serde_json::from_value(json!({"name":name,"latitude":42.0,"longitude":-83.0})).unwrap()
}

#[tokio::test]
async fn editable_zone_feed_excludes_yaml_and_home_and_enforces_admin_mutations() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "zone-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "zone-user-fixture", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let integration = ZoneIntegration::start(
        runtime.clone(),
        HomeZoneConfig {
            name: "Home".into(),
            latitude: 0.0,
            longitude: 0.0,
            zones: vec![zone("YAML")],
        },
    )
    .await
    .unwrap();
    let manager = integration.manager();
    let existing = manager.create(zone("Existing")).unwrap();
    let mut public_events = runtime.subscribe();
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth),
            zones: Some(manager.clone()),
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
        .unwrap();
    });
    let mut admin = connect(address, &owner_token.access_token).await;
    let mut ordinary = connect(address, &user_token.access_token).await;
    let list = command(&mut ordinary, json!({"id":1,"type":"zone/list"})).await;
    assert_eq!(list["success"], true);
    assert_eq!(list["result"].as_array().unwrap().len(), 1);
    assert_eq!(list["result"][0]["id"], existing.id);
    let denied = command(
        &mut ordinary,
        json!({"id":2,"type":"zone/create","name":"Denied","latitude":0,"longitude":0}),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");
    assert_eq!(manager.list().unwrap().len(), 1);
    assert_eq!(
        command(&mut ordinary, json!({"id":3,"type":"zone/subscribe"})).await["success"],
        true
    );
    let initial = receive(&mut ordinary).await;
    assert_eq!(initial["id"], 3);
    assert_eq!(initial["event"].as_array().unwrap().len(), 1);
    assert_eq!(initial["event"][0]["change_type"], "added");
    assert_eq!(initial["event"][0]["zone_id"], existing.id);
    let created = command(
        &mut admin,
        json!({"id":1,"type":"zone/create","name":"Wire","latitude":40,"longitude":-80}),
    )
    .await;
    assert_eq!(created["success"], true);
    let id = created["result"]["id"].as_str().unwrap().to_owned();
    let added = receive(&mut ordinary).await;
    assert_eq!(added["event"][0]["change_type"], "added");
    assert_eq!(added["event"][0]["zone_id"], id);
    let updated = command(
        &mut admin,
        json!({"id":2,"type":"zone/update","zone_id":id,"name":"Renamed","passive":true}),
    )
    .await;
    assert_eq!(updated["result"]["name"], "Renamed");
    let update_event = receive(&mut ordinary).await;
    assert_eq!(update_event["event"][0]["change_type"], "updated");
    assert_eq!(update_event["event"][0]["item"]["passive"], true);
    assert_eq!(
        command(
            &mut admin,
            json!({"id":3,"type":"zone/update","zone_id":id,"latitude":100}),
        )
        .await["success"],
        false
    );
    // Invalid input produces no queued subscription event or stored mutation.
    let list = command(&mut ordinary, json!({"id":4,"type":"zone/list"})).await;
    assert_eq!(list["id"], 4);
    assert_eq!(manager.get(&id).unwrap().unwrap().config.latitude, 40.0);
    assert_eq!(
        command(
            &mut admin,
            json!({"id":4,"type":"zone/delete","zone_id":id})
        )
        .await["success"],
        true
    );
    let removed = receive(&mut ordinary).await;
    assert_eq!(removed["event"][0]["change_type"], "removed");
    assert_eq!(removed["event"][0]["zone_id"], id);
    assert_eq!(
        command(
            &mut ordinary,
            json!({"id":5,"type":"unsubscribe_events","subscription":3}),
        )
        .await["success"],
        true
    );
    manager.create(zone("After unsubscribe")).unwrap();
    let list = command(&mut ordinary, json!({"id":6,"type":"zone/list"})).await;
    assert_eq!(list["id"], 6);
    assert_eq!(list["result"].as_array().unwrap().len(), 2);
    while let Ok(event) = public_events.try_recv() {
        assert_ne!(event.event_type, "zone_collection_updated");
    }
    ordinary.close(None).await.unwrap();
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    integration.shutdown().await.unwrap();
}
