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
input_boolean:
  demo:
    initial: false
counter:
  presses:
    initial: 0
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
"#,
    )
    .unwrap();
    config
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
