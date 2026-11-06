//! Person collection authority, caller-bound profile edits and auth unlinking.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Error, MemoryStore, Runtime};
use hass_person::{
    PersonConfig, PersonCreate, PersonEntityResolver, PersonIntegration, PersonUserValidator,
};
use hass_registry::{EntityRegistration, Registries};
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
async fn person_collection_preserves_profile_trackers_and_applies_removed_user_defaults() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "person-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "person-user-fixture", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let registry = Registries::new(runtime.clone()).unwrap();
    let entity_registry = registry.clone();
    let resolve: PersonEntityResolver = Arc::new(move |person| {
        let mut registration = EntityRegistration::new("person", "person", &person.id);
        registration.suggested_object_id = Some(person.name.clone());
        let entry = entity_registry.get_or_create_entity(registration)?;
        Ok(entry.disabled_by.is_none().then_some(entry.entity_id))
    });
    let users = auth.clone();
    let validate_user: PersonUserValidator = Arc::new(move |id| match users.get_user(id) {
        Ok(_) => Ok(true),
        Err(Error::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    });
    let yaml: PersonConfig =
        serde_json::from_value(json!({"id":"1234","name":"YAML Person"})).unwrap();
    let integration = PersonIntegration::start(runtime.clone(), vec![yaml], validate_user, resolve)
        .await
        .unwrap();
    integration.mark_started().unwrap();
    assert!(runtime.get_state("person.yaml_person").is_some());
    assert!(runtime.get_state("person.1234").is_none());
    let manager = integration.manager();
    let linked = manager
        .create(PersonCreate {
            name: "Storage Person".into(),
            device_trackers: vec!["device_tracker.phone".into()],
            user_id: Some(user.id.clone()),
            picture: Some("/old-picture".into()),
        })
        .unwrap();
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            persons: Some(manager.clone()),
            registries: Some(registry),
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
    let listed = command(&mut ordinary, json!({"id":1,"type":"person/list"})).await;
    assert_eq!(listed["success"], true);
    assert_eq!(listed["result"]["storage"][0]["id"], linked.id);
    assert_eq!(listed["result"]["config"][0]["id"], "1234");
    assert_eq!(
        command(
            &mut ordinary,
            json!({"id":2,"type":"person/update","person_id":linked.id,"name":"Forbidden"})
        )
        .await["error"]["code"],
        "unauthorized"
    );
    assert_eq!(
        command(&mut ordinary, json!({"id":3,"type":"person/subscribe"})).await["success"],
        true
    );
    let initial = receive(&mut ordinary).await;
    assert_eq!(initial["event"].as_array().unwrap().len(), 1);
    assert_eq!(initial["event"][0]["person_id"], linked.id);
    let invalid = command(&mut ordinary, json!({"id":4,"type":"person/update_own_profile","name":"Must not apply","picture":"https://example.test/unuploaded"})).await;
    assert_eq!(invalid["success"], false);
    assert_eq!(auth.get_user(&user.id).unwrap().name, "User");
    let changed = command(
        &mut ordinary,
        json!({"id":5,"type":"person/update_own_profile","name":"  Updated User  ","picture":null}),
    )
    .await;
    assert_eq!(changed["success"], true);
    assert_eq!(changed["result"]["user_name"], "Updated User");
    assert_eq!(changed["result"]["person"]["name"], "Updated User");
    assert_eq!(
        changed["result"]["person"]["device_trackers"],
        json!(["device_tracker.phone"])
    );
    assert!(changed["result"]["person"]["picture"].is_null());
    let event = receive(&mut ordinary).await;
    assert_eq!(event["event"][0]["change_type"], "updated");
    assert_eq!(event["event"][0]["person_id"], linked.id);
    assert_eq!(
        manager.get(&linked.id).unwrap().unwrap().user_id.as_deref(),
        Some(user.id.as_str())
    );
    ordinary.close(None).await.unwrap();

    assert_eq!(
        command(&mut admin, json!({"id":1,"type":"person/subscribe"})).await["success"],
        true
    );
    receive(&mut admin).await;
    auth.delete_user(&owner.id, &user.id).unwrap();
    let unlinked = receive(&mut admin).await;
    assert_eq!(unlinked["event"][0]["person_id"], linked.id);
    assert_eq!(unlinked["event"][0]["change_type"], "updated");
    assert!(manager.get(&linked.id).unwrap().unwrap().user_id.is_none());
    // Pinned user removal submits only user_id=None; the normal update
    // schema resets omitted device_trackers, unlike the own-profile path.
    assert!(
        manager
            .get(&linked.id)
            .unwrap()
            .unwrap()
            .device_trackers
            .is_empty()
    );
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    integration.shutdown().await.unwrap();
}
