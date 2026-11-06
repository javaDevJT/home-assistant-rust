//! Real image upload, public file delivery and caller-bound profile pictures.

use std::{io::Cursor, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Error, MemoryStore, Runtime};
use hass_image_upload::ImageIntegration;
use hass_person::{PersonCreate, PersonEntityResolver, PersonIntegration, PersonUserValidator};
use hass_registry::{EntityRegistration, Registries};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use reqwest::{
    StatusCode,
    multipart::{Form, Part},
};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn receive(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(15), socket.next())
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
    receive(&mut socket).await;
    assert_eq!(
        command(&mut socket, json!({"type":"auth","access_token":token})).await["type"],
        "auth_ok"
    );
    socket
}

fn png() -> Vec<u8> {
    let mut seed = 0x1234_5678_u32;
    let image = RgbImage::from_fn(1024, 768, |_, _| {
        let mut color = [0; 3];
        for channel in &mut color {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *channel = seed as u8;
        }
        Rgb(color)
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn form(bytes: Vec<u8>, mime: &str) -> Form {
    Form::new().part(
        "file",
        Part::bytes(bytes)
            .file_name("fixture.png")
            .mime_str(mime)
            .unwrap(),
    )
}

#[tokio::test]
async fn readonly_user_uploads_real_image_public_get_ranges_and_profile_reference_are_validated() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "image-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "image-user-fixture", false)
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
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let images = ImageIntegration::start(runtime.clone(), directory.path()).unwrap();
    let image_manager = images.manager();
    let registry = Registries::new(runtime.clone()).unwrap();
    let entity_registry = registry.clone();
    let resolve: PersonEntityResolver = Arc::new(move |person| {
        let mut registration = EntityRegistration::new("person", "person", &person.id);
        registration.suggested_object_id = Some(person.name.clone());
        Ok(Some(
            entity_registry
                .get_or_create_entity(registration)?
                .entity_id,
        ))
    });
    let users = auth.clone();
    let validate_user: PersonUserValidator = Arc::new(move |id| match users.get_user(id) {
        Ok(_) => Ok(true),
        Err(Error::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    });
    let persons = PersonIntegration::start(runtime.clone(), Vec::new(), validate_user, resolve)
        .await
        .unwrap();
    persons.mark_started().unwrap();
    let person_manager = persons.manager();
    person_manager
        .create(PersonCreate {
            name: "Profile User".into(),
            user_id: Some(user.id.clone()),
            device_trackers: vec!["device_tracker.phone".into()],
            picture: None,
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
            auth: Some(auth.clone()),
            registries: Some(registry),
            persons: Some(person_manager.clone()),
            images: Some(image_manager.clone()),
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let bytes = png();
    assert!(
        bytes.len() > 1024 * 1024,
        "fixture must exceed the other API routes' body limit"
    );
    assert_eq!(
        client
            .post(format!("{base}/api/image/upload"))
            .multipart(form(bytes.clone(), "image/png"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(image_manager.list().unwrap().is_empty());
    let mut ordinary = connect(address, &user_token.access_token).await;
    let mut admin = connect(address, &owner_token.access_token).await;
    assert_eq!(
        command(&mut ordinary, json!({"id":1,"type":"image/subscribe"})).await["success"],
        true
    );
    assert_eq!(receive(&mut ordinary).await["event"], json!([]));
    let response = client
        .post(format!("{base}/api/image/upload"))
        .bearer_auth(&user_token.access_token)
        .multipart(form(bytes.clone(), "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let uploaded: Value = response.json().await.unwrap();
    let id = uploaded["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);
    assert_eq!(uploaded["filesize"], bytes.len());
    let added = receive(&mut ordinary).await;
    assert_eq!(added["event"][0]["change_type"], "added");
    assert_eq!(added["event"][0]["image_id"], id);
    let original_url = format!("{base}/api/image/serve/{id}/original");
    let original = client.get(&original_url).send().await.unwrap();
    assert_eq!(original.status(), StatusCode::OK);
    assert_eq!(original.headers()["content-type"], "image/png");
    assert_eq!(
        original.headers()["cache-control"],
        "public, max-age=2678400"
    );
    let etag = original.headers()["etag"].clone();
    assert_eq!(original.bytes().await.unwrap().as_ref(), bytes.as_slice());
    let head = client.head(&original_url).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    let modified = head.headers()["last-modified"].clone();
    assert_eq!(
        head.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        bytes.len()
    );
    assert!(head.bytes().await.unwrap().is_empty());
    let range = client
        .get(&original_url)
        .header("range", "bytes=0-15")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.bytes().await.unwrap().as_ref(), &bytes[..16]);
    let cached = client
        .get(&original_url)
        .header("if-none-match", etag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert!(cached.bytes().await.unwrap().is_empty());
    assert_eq!(
        client
            .get(&original_url)
            .header("if-match", "\"different\"")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        client
            .get(&original_url)
            .header("if-none-match", format!("W/{}", etag.to_str().unwrap()))
            .header("range", "not-a-range")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_MODIFIED
    );
    assert_eq!(
        client
            .get(&original_url)
            .header("if-none-match", "\"different\"")
            .header("if-modified-since", modified)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let thumbnail = client
        .get(format!("{base}/api/image/serve/{id}/256x256-buster"))
        .send()
        .await
        .unwrap();
    assert_eq!(thumbnail.status(), StatusCode::OK);
    let thumbnail = image::load_from_memory(&thumbnail.bytes().await.unwrap()).unwrap();
    assert_eq!((thumbnail.width(), thumbnail.height()), (256, 192));
    assert!(
        directory
            .path()
            .join("image")
            .join(&id)
            .join("256x256")
            .is_file()
    );
    assert_eq!(
        client
            .get(format!("{base}/api/image/serve/{id}/512x256"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!(
                "{base}/api/image/serve/{}/original",
                "0".repeat(32)
            ))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let picture = format!("/api/image/serve/{id}/256x256");
    let profile = command(
        &mut ordinary,
        json!({"id":2,"type":"person/update_own_profile","picture":picture}),
    )
    .await;
    assert_eq!(profile["success"], true);
    assert_eq!(profile["result"]["person"]["picture"], picture);
    assert_eq!(
        profile["result"]["person"]["device_trackers"],
        json!(["device_tracker.phone"])
    );
    let state: Value = client
        .get(format!("{base}/api/states/person.profile_user"))
        .bearer_auth(&user_token.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["attributes"]["entity_picture"], picture);
    assert_eq!(
        command(
            &mut ordinary,
            json!({"id":3,"type":"image/update","image_id":id,"name":"Denied"})
        )
        .await["error"]["code"],
        "unauthorized"
    );
    let invalid = command(&mut ordinary,json!({"id":4,"type":"person/update_own_profile","name":"Must not apply","picture":format!("https://example.test{picture}")})).await;
    assert_eq!(invalid["success"], false);
    assert_eq!(auth.get_user(&user.id).unwrap().name, "User");
    assert_eq!(
        command(
            &mut admin,
            json!({"id":1,"type":"image/update","image_id":id,"name":"Renamed"})
        )
        .await["success"],
        true
    );
    let update = receive(&mut ordinary).await;
    assert_eq!(update["event"][0]["item"]["name"], "Renamed");
    assert_eq!(
        command(
            &mut admin,
            json!({"id":2,"type":"image/delete","image_id":id})
        )
        .await["success"],
        true
    );
    assert_eq!(
        receive(&mut ordinary).await["event"][0]["change_type"],
        "removed"
    );
    assert_eq!(
        client.get(&original_url).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert!(!directory.path().join("image").join(&id).exists());
    assert_eq!(
        command(
            &mut ordinary,
            json!({"id":5,"type":"person/update_own_profile","picture":picture})
        )
        .await["success"],
        false
    );
    let oversized = vec![0_u8; hass_image_upload::MAX_SIZE + 1];
    assert_eq!(
        client
            .post(format!("{base}/api/image/upload"))
            .bearer_auth(&user_token.access_token)
            .multipart(form(oversized, "image/png"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(image_manager.list().unwrap().is_empty());
    assert_eq!(
        client
            .post(format!("{base}/api/image/upload"))
            .bearer_auth(&user_token.access_token)
            .multipart(form(bytes.clone(), "text/plain"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{base}/api/image/upload"))
            .bearer_auth(&user_token.access_token)
            .multipart(form(bytes, "image/png").text("unexpected", "value"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let empty = command(&mut ordinary, json!({"id":6,"type":"image/list"})).await;
    assert_eq!(empty["id"], 6);
    assert_eq!(empty["result"], json!([]));
    while let Ok(event) = public_events.try_recv() {
        assert_ne!(
            event.event_type,
            hass_image_upload::IMAGE_COLLECTION_UPDATED_SIGNAL
        );
    }
    ordinary.close(None).await.unwrap();
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    persons.shutdown().await.unwrap();
    images.shutdown().unwrap();
}
