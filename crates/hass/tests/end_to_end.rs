//! Acceptance tests through the compiled process, HTTP, and a real SQLite file.

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{net::TcpListener, path::Path, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

const TOKEN: &str = "rust-rewrite-test-token-never-use-in-production-123456789";

struct Server {
    child: Child,
    url: String,
    client: Client,
}

impl Server {
    async fn start(config: &Path) -> Self {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut child = Command::new(env!("CARGO_BIN_EXE_hass"))
            .arg("--config")
            .arg(config)
            .arg("--port")
            .arg(port.to_string())
            .env("HASS_TOKEN", TOKEN)
            .env_remove("HASS_TOKEN_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if let Ok(response) = client.get(format!("{url}/health")).send().await {
                assert_eq!(response.status(), StatusCode::OK);
                return Self { child, url, client };
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("server exited before readiness: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server did not become ready");
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn wait_state(&self, entity_id: &str, expected: &str) {
        let mut last = Value::Null;
        for _ in 0..100 {
            let state: Value = self
                .get(&format!("/api/states/{entity_id}"))
                .await
                .json()
                .await
                .unwrap();
            if state["state"] == expected {
                return;
            }
            last = state;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{entity_id} did not reach {expected}; last response: {last}");
    }
}

fn write_config(dir: &Path) -> std::path::PathBuf {
    let config = dir.join("configuration.yaml");
    std::fs::write(
        dir.join("templates.yaml"),
        r#"
- sensor:
    - name: Doubled
      unique_id: process_doubled_count
      state: "{{ states('counter.presses') | int(0) * 2 }}"
"#,
    )
    .unwrap();
    std::fs::write(
        &config,
        r#"
recorder:
  db_path: state.sqlite
template: !include templates.yaml
group:
  demo_controls:
    name: Demo controls
    entities:
      - input_boolean.demo
input_boolean:
  demo: {}
counter:
  presses:
    initial: 0
input_text:
  notification_message:
    initial: waiting
person:
  - id: resident_unique_id
    name: Resident
    device_trackers:
      - device_tracker.phone
automation:
  - id: demo_toggle
    triggers:
      - trigger: event
        event_type: demo_toggle
    actions:
      - action: input_boolean.toggle
        target:
          entity_id: input_boolean.demo
      - action: counter.increment
        target:
          entity_id: counter.presses
  - id: notification_message
    triggers:
      - trigger: persistent_notification
        notification_id: process_fixture
        update_type: added
    actions:
      - action: input_text.set_value
        target:
          entity_id: input_text.notification_message
        data:
          value: "{{ trigger.notification.message }}"
"#,
    )
    .unwrap();
    config
}

#[tokio::test]
async fn real_process_image_upload_and_public_asset_survive_hard_restart() {
    let directory = tempfile::tempdir().unwrap();
    let config = write_config(directory.path());
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32,
        16,
        image::Rgb([10, 100, 200]),
    ))
    .write_to(&mut encoded, image::ImageFormat::Png)
    .unwrap();
    let original = encoded.into_inner();
    let mut server = Server::start(&config).await;
    let part = reqwest::multipart::Part::bytes(original.clone())
        .file_name("process.png")
        .mime_str("image/png")
        .unwrap();
    let uploaded = server
        .client
        .post(format!("{}/api/image/upload", server.url))
        .bearer_auth(TOKEN)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    let metadata: Value = uploaded.json().await.unwrap();
    let id = metadata["id"].as_str().unwrap();
    let path = format!("/api/image/serve/{id}/original");
    let response = server
        .client
        .get(format!("{}{path}", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        original.as_slice()
    );
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restored = Server::start(&config).await;
    let response = restored
        .client
        .get(format!("{}{path}", restored.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        original.as_slice()
    );
    let thumb = restored
        .client
        .get(format!("{}/api/image/serve/{id}/512x512", restored.url))
        .send()
        .await
        .unwrap();
    assert_eq!(thumb.status(), StatusCode::OK);
    let decoded = image::load_from_memory(&thumb.bytes().await.unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (32, 16));
    assert!(
        directory
            .path()
            .join("image")
            .join(id)
            .join("original")
            .is_file()
    );
    restored.child.kill().await.unwrap();
    restored.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_tracker_reports_drive_person_and_zone_occupancy() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let mut server = Server::start(&config).await;
    server.wait_state("person.resident", "unknown").await;
    assert_eq!(
        server
            .post(
                "/api/services/device_tracker/see",
                json!({
                    "dev_id":"phone","gps":[0.0,0.0],"gps_accuracy":0,"source_type":"gps"
                })
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("device_tracker.phone", "home").await;
    server.wait_state("person.resident", "home").await;
    server.wait_state("zone.home", "1").await;
    let person: Value = server
        .get("/api/states/person.resident")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(person["attributes"]["source"], "device_tracker.phone");
    assert_eq!(person["attributes"]["in_zones"], json!(["zone.home"]));
    assert_eq!(
        server
            .post(
                "/api/services/device_tracker/see",
                json!({
                    "dev_id":"phone","gps":[1.0,1.0],"gps_accuracy":0,"source_type":"gps"
                })
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("device_tracker.phone", "not_home").await;
    server.wait_state("person.resident", "not_home").await;
    server.wait_state("zone.home", "0").await;
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restored = Server::start(&config).await;
    restored.wait_state("person.resident", "not_home").await;
    assert_eq!(
        restored
            .get("/api/states/person.resident_unique_id")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    restored.child.kill().await.unwrap();
    restored.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_notification_trigger_uses_private_service_payload() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let mut server = Server::start(&config).await;
    server
        .wait_state("input_text.notification_message", "waiting")
        .await;
    assert_eq!(
        server
            .post(
                "/api/services/persistent_notification/create",
                json!({"notification_id":"process_fixture","message":"native notification"}),
            )
            .await
            .status(),
        StatusCode::OK
    );
    server
        .wait_state("input_text.notification_message", "native notification")
        .await;
    assert_eq!(
        server
            .post(
                "/api/services/persistent_notification/dismiss_all",
                json!({}),
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_zone_reload_occupancy_and_saved_home_location() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let zones_path = dir.path().join("zones.yaml");
    std::fs::write(
        &zones_path,
        "- name: Work\n  latitude: 42\n  longitude: -83\n",
    )
    .unwrap();
    let mut source = std::fs::read_to_string(&config).unwrap();
    source.push_str("\nzone: !include zones.yaml\n");
    std::fs::write(&config, source).unwrap();
    let mut server = Server::start(&config).await;
    server.wait_state("zone.home", "0").await;
    server.wait_state("zone.work", "0").await;
    assert_eq!(
        server
            .post(
                "/api/states/person.fixture",
                json!({"state":"Work","attributes":{"in_zones":["zone.work"]}}),
            )
            .await
            .status(),
        StatusCode::CREATED
    );
    server.wait_state("zone.work", "1").await;
    let zone: Value = server
        .get("/api/states/zone.work")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(zone["attributes"]["persons"], json!(["person.fixture"]));

    // Reload resolves the include again and validates only this component.
    std::fs::write(
        &zones_path,
        "- name: New Work\n  latitude: 43\n  longitude: -84\n",
    )
    .unwrap();
    assert_eq!(
        server
            .post("/api/services/zone/reload", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("zone.new_work", "0").await;
    assert_eq!(
        server.get("/api/states/zone.work").await.status(),
        StatusCode::NOT_FOUND
    );

    let ws_url = format!(
        "{}/api/websocket",
        server.url.replacen("http://", "ws://", 1)
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    socket.next().await.unwrap().unwrap();
    socket
        .send(Message::Text(
            json!({"type":"auth","access_token":TOKEN})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let accepted = socket.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(accepted.to_text().unwrap()).unwrap()["type"],
        "auth_ok"
    );
    socket
        .send(Message::Text(
            json!({"id":1,"type":"config/core/update","location_name":"Saved Home","latitude":41.5,"longitude":-82.5})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let updated = socket.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(updated.to_text().unwrap()).unwrap()["success"],
        true
    );
    for attempt in 0..100 {
        let home: Value = server
            .get("/api/states/zone.home")
            .await
            .json()
            .await
            .unwrap();
        if home["attributes"]["latitude"] == 41.5 {
            assert_eq!(home["attributes"]["longitude"], -82.5);
            assert_eq!(home["attributes"]["friendly_name"], "Saved Home");
            break;
        }
        assert!(
            attempt < 99,
            "zone.home did not follow the accepted core update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restored = Server::start(&config).await;
    let home: Value = restored
        .get("/api/states/zone.home")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(home["attributes"]["latitude"], 41.5);
    assert_eq!(home["attributes"]["friendly_name"], "Saved Home");
    restored.child.kill().await.unwrap();
    restored.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_auth_services_automation_history_and_crash_restore() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let mut server = Server::start(&config).await;
    let denied = server
        .client
        .get(format!("{}/api/states", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    server.wait_state("input_boolean.demo", "off").await;
    server.wait_state("sensor.doubled", "0").await;
    server.wait_state("group.demo_controls", "off").await;
    let created = server
        .post(
            "/api/states/sensor.example",
            json!({"state":"42", "attributes":{"unit_of_measurement":"°C"}}),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert!(created.headers().contains_key("location"));
    assert_eq!(
        server
            .post("/api/states/sensor.example", json!({"state":"43"}))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .post(
                "/api/services/input_boolean/turn_on",
                json!({"entity_id":"input_boolean.demo"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("input_boolean.demo", "on").await;
    server.wait_state("group.demo_controls", "on").await;
    assert_eq!(server.post("/api/services/group/set",json!({"object_id":"temporary","entities":["input_boolean.demo"],"name":"Temporary controls"})).await.status(),StatusCode::OK);
    server.wait_state("group.temporary", "on").await;
    assert_eq!(
        server
            .post(
                "/api/services/homeassistant/turn_off",
                json!({"entity_id":"group.temporary"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("input_boolean.demo", "off").await;
    server.wait_state("group.demo_controls", "off").await;
    assert_eq!(
        server
            .post(
                "/api/services/homeassistant/turn_on",
                json!({"entity_id":"group.demo_controls"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("input_boolean.demo", "on").await;
    assert_eq!(
        server
            .post(
                "/api/services/group/remove",
                json!({"object_id":"temporary"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server.get("/api/states/group.temporary").await.status(),
        StatusCode::NOT_FOUND
    );
    let listeners: Value = server
        .get("/api/events")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listeners
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["event"] == "*" && row["listener_count"].as_u64().unwrap_or(0) > 0)
    );
    assert_eq!(
        server
            .post("/api/events/demo_toggle", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    server.wait_state("counter.presses", "1").await;
    server.wait_state("sensor.doubled", "2").await;
    server.wait_state("input_boolean.demo", "off").await;
    server.wait_state("group.demo_controls", "off").await;
    let history_response = server
        .get("/api/history/period?filter_entity_id=input_boolean.demo")
        .await;
    assert_eq!(history_response.status(), StatusCode::OK);
    let history: Value = history_response.json().await.unwrap();
    let rows = history[0]
        .as_array()
        .expect("history array for the selected entity");
    assert!(rows.iter().any(|state| state["state"] == "on"));
    assert!(rows.iter().any(|state| state["state"] == "off"));
    // A hard process termination exercises persisted WAL state without relying
    // on a graceful flush or an in-memory test store.
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restarted = Server::start(&config).await;
    restarted.wait_state("counter.presses", "1").await;
    restarted.wait_state("sensor.doubled", "2").await;
    restarted.wait_state("input_boolean.demo", "off").await;
    restarted.wait_state("group.demo_controls", "off").await;
    restarted.wait_state("sensor.example", "43").await;
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
}

#[test]
fn check_validates_without_creating_data_or_requiring_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hass"))
        .arg("--config")
        .arg(&config)
        .arg("--check")
        .env_remove("HASS_TOKEN")
        .env_remove("HASS_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("state.sqlite").exists());
}

#[test]
fn unsupported_integration_is_a_validation_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("configuration.yaml");
    std::fs::write(&config, "not_a_real_integration: {}\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hass"))
        .arg("--config")
        .arg(&config)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported configuration key"));
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_exits_with_an_authenticated_websocket_open() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let mut server = Server::start(&config).await;
    let ws_url = format!(
        "{}/api/websocket",
        server.url.replacen("http://", "ws://", 1)
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let required = socket.next().await.unwrap().unwrap();
    let required: Value = serde_json::from_str(required.to_text().unwrap()).unwrap();
    assert_eq!(required["type"], "auth_required");
    socket
        .send(Message::Text(
            json!({"type":"auth", "access_token":TOKEN})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let accepted = socket.next().await.unwrap().unwrap();
    let accepted: Value = serde_json::from_str(accepted.to_text().unwrap()).unwrap();
    assert_eq!(accepted["type"], "auth_ok");
    let pid = server.child.id().unwrap();
    assert!(
        std::process::Command::new("/bin/kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(Duration::from_secs(5), server.child.wait())
        .await
        .expect("SIGTERM must exit even with an upgraded socket open")
        .unwrap();
    assert!(status.success());
}
