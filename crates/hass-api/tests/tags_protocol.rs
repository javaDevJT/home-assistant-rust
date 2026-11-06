//! Authenticated native tag collection CRUD and private subscription protocol.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use hass_entity::EntityLifecycle;
use hass_registry::Registries;
use hass_tag::{TagCreate, TagIntegration};
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

#[tokio::test]
async fn tag_list_and_subscribe_are_readable_but_crud_is_admin_only() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "tag-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "tag-user-fixture", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let integration =
        TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
    let manager = integration.manager();
    let existing = manager
        .create(TagCreate {
            tag_id: Some("fixture-tag".into()),
            name: Some("Kitchen tag".into()),
            ..TagCreate::default()
        })
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
            registries: Some(registries),
            tags: Some(manager.clone()),
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

    let listed = command(&mut ordinary, json!({"id":1,"type":"tag/list"})).await;
    assert_eq!(listed["success"], true);
    assert_eq!(listed["result"][0]["id"], existing.id);
    assert_eq!(listed["result"][0]["name"], "Kitchen tag");

    let denied = command(
        &mut ordinary,
        json!({"id":2,"type":"tag/create","tag_id":"denied"}),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");

    let subscribe = command(&mut ordinary, json!({"id":3,"type":"tag/subscribe"})).await;
    assert_eq!(subscribe["success"], true);
    let initial = receive(&mut ordinary).await;
    assert_eq!(initial["id"], 3);
    assert_eq!(initial["event"][0]["change_type"], "added");
    assert_eq!(initial["event"][0]["tag_id"], existing.id);

    let created = command(
        &mut admin,
        json!({"id":1,"type":"tag/create","tag_id":"wire-tag","name":"Entry tag","description":"front door"}),
    )
    .await;
    assert_eq!(created["success"], true);
    let id = created["result"]["id"].as_str().unwrap().to_owned();
    let added = receive(&mut ordinary).await;
    assert_eq!(added["event"][0]["change_type"], "added");
    assert_eq!(added["event"][0]["tag_id"], id);

    let duplicate = command(
        &mut admin,
        json!({"id":2,"type":"tag/create","tag_id":"fixture-tag"}),
    )
    .await;
    assert_eq!(duplicate["success"], false);
    assert_eq!(duplicate["error"]["code"], "home_assistant_error");

    let denied_update = command(
        &mut ordinary,
        json!({"id":4,"type":"tag/update","tag_id":id,"name":"Denied"}),
    )
    .await;
    assert_eq!(denied_update["success"], false);
    assert_eq!(denied_update["error"]["code"], "unauthorized");

    let updated = command(
        &mut admin,
        json!({"id":3,"type":"tag/update","tag_id":id,"name":"Renamed tag","description":"updated"}),
    )
    .await;
    assert_eq!(updated["success"], true);
    assert_eq!(updated["result"]["name"], "Renamed tag");
    let update_event = receive(&mut ordinary).await;
    assert_eq!(update_event["event"][0]["change_type"], "updated");
    assert_eq!(update_event["event"][0]["item"]["description"], "updated");

    let denied_delete = command(
        &mut ordinary,
        json!({"id":5,"type":"tag/delete","tag_id":id}),
    )
    .await;
    assert_eq!(denied_delete["success"], false);
    assert_eq!(denied_delete["error"]["code"], "unauthorized");
    let deleted = command(&mut admin, json!({"id":4,"type":"tag/delete","tag_id":id})).await;
    assert_eq!(deleted["success"], true);
    let removed = receive(&mut ordinary).await;
    assert_eq!(removed["event"][0]["change_type"], "removed");
    assert_eq!(removed["event"][0]["tag_id"], id);

    let generated = command(
        &mut admin,
        json!({"id":5,"type":"tag/create","tag_id":"","last_scanned":"2024-2-9 3:04"}),
    )
    .await;
    assert_eq!(generated["success"], true);
    let generated_id = generated["result"]["id"].as_str().unwrap().to_owned();
    assert!(!generated_id.is_empty());
    assert_eq!(generated["result"]["last_scanned"], "2024-02-09T03:04:00");
    let generated_event = receive(&mut ordinary).await;
    assert_eq!(generated_event["event"][0]["change_type"], "added");
    assert_eq!(generated_event["event"][0]["tag_id"], generated_id);

    let empty_id_delete =
        command(&mut admin, json!({"id":6,"type":"tag/delete","tag_id":""})).await;
    assert_eq!(empty_id_delete["success"], false);
    assert_eq!(empty_id_delete["error"]["code"], "not_found");
    let generated_delete = command(
        &mut admin,
        json!({"id":7,"type":"tag/delete","tag_id":generated_id}),
    )
    .await;
    assert_eq!(generated_delete["success"], true);
    let removed_generated = receive(&mut ordinary).await;
    assert_eq!(removed_generated["event"][0]["change_type"], "removed");
    let no_scan_command = command(
        &mut admin,
        json!({"id":8,"type":"tag/scan","tag_id":"fixture-tag"}),
    )
    .await;
    assert_eq!(no_scan_command["success"], false);
    assert_eq!(no_scan_command["error"]["code"], "unknown_command");
    let unsubscribed = command(
        &mut ordinary,
        json!({"id":6,"type":"unsubscribe_events","subscription":3}),
    )
    .await;
    assert_eq!(unsubscribed["success"], true);

    let coerced = command(
        &mut admin,
        json!({"id":9,"type":"tag/create","tag_id":42,"name":" ","description":false,"device_id":1e20}),
    ).await;
    assert_eq!(coerced["success"], true);
    assert_eq!(coerced["result"]["id"], "42");
    assert_eq!(coerced["result"]["name"], " ");
    assert_eq!(coerced["result"]["description"], "False");
    assert_eq!(coerced["result"]["device_id"], "1e+20");
    let strict_id = command(
        &mut admin,
        json!({"id":10,"type":"tag/update","tag_id":42,"description":"unchanged"}),
    )
    .await;
    assert_eq!(strict_id["error"]["code"], "invalid_format");
    let update_scalars = command(
        &mut admin,
        json!({"id":11,"type":"tag/update","tag_id":"42","description":1.25,"device_id":true}),
    )
    .await;
    assert_eq!(update_scalars["success"], true);
    assert_eq!(update_scalars["result"]["description"], "1.25");
    assert_eq!(update_scalars["result"]["device_id"], "True");
    let strict_name = command(
        &mut admin,
        json!({"id":12,"type":"tag/update","tag_id":"42","name":true}),
    )
    .await;
    assert_eq!(strict_name["error"]["code"], "invalid_format");
    let invalid_description = command(
        &mut admin,
        json!({"id":13,"type":"tag/update","tag_id":"42","description":[]}),
    )
    .await;
    assert_eq!(invalid_description["error"]["code"], "invalid_format");
    assert_eq!(
        manager.get("42").unwrap().unwrap().description.as_deref(),
        Some("1.25")
    );

    while let Ok(event) = public_events.try_recv() {
        assert_ne!(event.event_type, "tag_collection_updated");
    }

    ordinary.close(None).await.unwrap();
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    integration.shutdown().unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn actual_composite_registry_is_readable_and_alias_mutations_report_not_allowed() {
    use hass_registry::DeviceRegistration;
    use std::collections::BTreeSet;
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "composite-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "composite-user-fixture", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let original = Registries::new(runtime.clone()).unwrap();
    let device = original
        .get_or_create_device(DeviceRegistration {
            config_entry_id: Some("entry-a".into()),
            identifiers: BTreeSet::from([("fixture".into(), "shared".into())]),
            ..DeviceRegistration::default()
        })
        .unwrap();
    let mut snapshot = runtime.get_data("core.registries").unwrap().unwrap();
    snapshot["version"] = json!(1);
    snapshot["devices"][0]["config_entries"] = json!(["entry-a", "entry-b"]);
    snapshot["devices"][0]["config_entries_subentries"] =
        json!({"entry-a":[null],"entry-b":[null]});
    runtime.set_data("core.registries", &snapshot).unwrap();
    drop(original);
    let registries = Registries::new(runtime.clone()).unwrap();
    let expected = serde_json::to_value(registries.composite_splits()).unwrap();
    assert_eq!(
        expected[&device.id]["split_ids"].as_array().unwrap().len(),
        2
    );
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth),
            registries: Some(registries),
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
    let mut ordinary = connect(address, &user_token.access_token).await;
    let mut admin = connect(address, &owner_token.access_token).await;
    let listed = command(
        &mut ordinary,
        json!({"id":1,"type":"config/device_registry/list_composite_splits"}),
    )
    .await;
    assert_eq!(listed["success"], true, "{listed}");
    assert_eq!(listed["result"], expected);
    let denied = command(
        &mut ordinary,
        json!({"id":2,"type":"config/device_registry/remove","device_id":device.id}),
    )
    .await;
    assert_eq!(denied["error"]["code"], "unauthorized");
    let update = command(&mut admin,json!({"id":1,"type":"config/device_registry/update","device_id":device.id,"name_by_user":"invalid"})).await;
    assert_eq!(update["error"]["code"], "not_allowed");
    assert_eq!(
        update["error"]["message"],
        "Cannot update a composite device"
    );
    let remove = command(
        &mut admin,
        json!({"id":2,"type":"config/device_registry/remove","device_id":device.id}),
    )
    .await;
    assert_eq!(remove["error"]["code"], "not_allowed");
    assert_eq!(
        remove["error"]["message"],
        "Cannot remove a composite device"
    );
    ordinary.close(None).await.unwrap();
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}
