//! Acceptance tests through the compiled process, HTTP, and a real SQLite file.

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const TOKEN: &str = "rust-rewrite-test-token-never-use-in-production-123456789";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_command(socket: &mut Socket, value: Value) -> Value {
    use futures_util::{SinkExt, StreamExt};
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            value.to_string().into(),
        ))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

async fn authenticated_socket(server: &Server) -> Socket {
    use futures_util::StreamExt;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "{}/api/websocket",
        server.url.replacen("http://", "ws://", 1)
    ))
    .await
    .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(frame.to_text().unwrap()).unwrap()["type"],
        "auth_required"
    );
    assert_eq!(
        ws_command(&mut socket, json!({"type":"auth","access_token":TOKEN})).await["type"],
        "auth_ok"
    );
    socket
}

struct Server {
    child: Child,
    url: String,
    client: Client,
    _stdout: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hass"))
            .arg("--config")
            .arg(config)
            .arg("--port")
            .arg("0")
            .env("HASS_TOKEN", TOKEN)
            .env("RUST_LOG", "hass=info,hass_automation=info")
            .env_remove("HASS_TOKEN_FILE")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Let the child bind its own ephemeral port atomically. Reserving and
        // dropping a parent socket races with concurrent process fixtures.
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("Rust runtime listening")
                    && let Some((_, address)) = line.split_once("127.0.0.1:")
                {
                    let port: String = address.chars().take_while(char::is_ascii_digit).collect();
                    return port.parse::<u16>().unwrap();
                }
            }
            panic!(
                "server exited before announcing its listener: {:?}",
                child.wait().await.unwrap()
            );
        })
        .await
        .expect("server did not announce its listener");
        let stdout =
            tokio::spawn(async move { while matches!(lines.next_line().await, Ok(Some(_))) {} });
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if let Ok(response) = client.get(format!("{url}/health")).send().await {
                assert_eq!(response.status(), StatusCode::OK);
                return Self {
                    child,
                    url,
                    client,
                    _stdout: stdout,
                };
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

#[cfg(unix)]
#[tokio::test]
async fn real_process_hardware_info_uses_loaded_providers_and_joins_idle_sampler() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(&config, "hardware:\nrecorder:\n  db_path: state.sqlite\n").unwrap();
    let mut server = Server::start(&config).await;
    let configuration: Value = server.get("/api/config").await.json().await.unwrap();
    assert!(
        configuration["components"]
            .as_array()
            .unwrap()
            .contains(&json!("hardware"))
    );
    let mut socket = authenticated_socket(&server).await;
    let info = ws_command(&mut socket, json!({"id":1,"type":"hardware/info"})).await;
    assert_eq!(info["success"], true, "{info}");
    assert_eq!(info["result"], json!({"hardware":[]}));
    // No status subscription is created: the actual native metrics provider
    // stays idle and this fixture reads no host CPU/memory measurements.
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
        .expect("hardware owner must drain on SIGTERM")
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn real_process_system_health_reports_native_runtime_and_saved_timezone() {
    use futures_util::StreamExt;

    async fn report(socket: &mut Socket, id: u64) -> Value {
        let acknowledgement =
            ws_command(socket, json!({"id":id,"type":"system_health/info"})).await;
        assert_eq!(acknowledgement["success"], true, "{acknowledgement}");
        assert_eq!(acknowledgement["result"], Value::Null);
        let initial = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let initial: Value = serde_json::from_str(initial.to_text().unwrap()).unwrap();
        assert_eq!(initial["id"], id);
        assert_eq!(initial["event"]["type"], "initial", "{initial}");
        let finish = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let finish: Value = serde_json::from_str(finish.to_text().unwrap()).unwrap();
        assert_eq!(
            finish,
            json!({"id":id,"type":"event","event":{"type":"finish"}})
        );
        initial["event"]["data"]["homeassistant"]["info"].clone()
    }

    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(
        &config,
        "homeassistant:\n  time_zone: America/Detroit\nrecorder:\n  db_path: state.sqlite\n",
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(
        report(&mut socket, 1).await,
        json!({"version":"core-0.1.0-rust","timezone":"America/Detroit"})
    );
    let updated = ws_command(
        &mut socket,
        json!({"id":2,"type":"config/core/update","time_zone":"Europe/London"}),
    )
    .await;
    assert_eq!(updated["success"], true, "{updated}");
    assert_eq!(
        report(&mut socket, 3).await,
        json!({"version":"core-0.1.0-rust","timezone":"Europe/London"})
    );
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();

    let mut restarted = Server::start(&config).await;
    let mut socket = authenticated_socket(&restarted).await;
    assert_eq!(
        report(&mut socket, 1).await,
        json!({"version":"core-0.1.0-rust","timezone":"Europe/London"})
    );
    socket.close(None).await.unwrap();
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_repairs_sdk_storage_events_confirm_and_restart() {
    use futures_util::{SinkExt, StreamExt};
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(&config, "recorder:\n  db_path: state.sqlite\n").unwrap();
    let database = directory.path().join("state.sqlite");
    // A native SDK producer seeds a persistent fixture through the actual
    // registry and SQLite writer, before handing the database to the child.
    // No issue-creation endpoint or production provider activity is invented.
    tokio::task::spawn_blocking(move || {
        let runtime = hass_core::Runtime::new(std::sync::Arc::new(
            hass_storage::SqliteStore::open(database).unwrap(),
        ))
        .unwrap();
        let entries = hass_config::ConfigEntries::new(runtime.clone()).unwrap();
        let owner =
            hass_repairs::RepairsIntegration::start(runtime, entries, "2026.12.0.dev0").unwrap();
        let mut issue = hass_repairs::IssueDraft::new(
            "tag",
            "sdk_process_fixture",
            true,
            "warning",
            "sdk_fixture",
        );
        issue.is_persistent = true;
        issue.data = Some(serde_json::Map::from_iter([(
            "record".into(),
            json!("tempdir test"),
        )]));
        issue.translation_placeholders = Some(serde_json::Map::from_iter([(
            "name".into(),
            json!("Fixture"),
        )]));
        owner.manager().create_issue(issue).unwrap();
    })
    .await
    .unwrap();

    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    let listed = ws_command(&mut socket, json!({"id":1,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["success"], true, "{listed}");
    assert_eq!(
        listed["result"]["issues"][0]["issue_id"],
        "sdk_process_fixture"
    );
    assert_eq!(
        ws_command(
            &mut socket,
            json!({"id":2,"type":"subscribe_events","event_type":"repairs_issue_registry_updated"})
        )
        .await["success"],
        true
    );
    socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":3,"type":"repairs/ignore_issue","domain":"tag","issue_id":"sdk_process_fixture","ignore":true}).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut acknowledged = false;
        let mut notified = false;
        while !acknowledged || !notified {
            let frame = socket.next().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            if message["type"] == "result" && message["id"] == 3 {
                assert_eq!(message["success"], true, "{message}");
                acknowledged = true;
            } else {
                assert_eq!(message["type"], "event", "{message}");
                assert_eq!(message["id"], 2, "{message}");
                assert_eq!(
                    message["event"]["data"],
                    json!({"action":"update","domain":"tag","issue_id":"sdk_process_fixture"})
                );
                notified = true;
            }
        }
    })
    .await
    .expect("ignore acknowledgement and its actual registry event must arrive");
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();

    let history_database = directory.path().join("state.sqlite");
    tokio::task::spawn_blocking(move || {
        let store = hass_storage::SqliteStore::open(history_database).unwrap();
        let events = hass_core::Store::event_history_query(
            &store,
            &hass_core::EventHistoryQuery {
                start: chrono::Utc::now() - chrono::Duration::minutes(1),
                end: chrono::Utc::now() + chrono::Duration::seconds(1),
                event_types: Some(vec!["repairs_issue_registry_updated".into()]),
            },
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.data
                == serde_json::Map::from_iter([
                    ("action".into(), json!("update")),
                    ("domain".into(), json!("tag")),
                    ("issue_id".into(), json!("sdk_process_fixture")),
                ])
        }));
        assert!(events.iter().all(|event| !event.data.contains_key("data")));
    })
    .await
    .unwrap();

    let mut restarted = Server::start(&config).await;
    let mut socket = authenticated_socket(&restarted).await;
    let listed = ws_command(&mut socket, json!({"id":1,"type":"repairs/list_issues"})).await;
    assert_eq!(
        listed["result"]["issues"][0]["dismissed_version"], "2026.12.0.dev0",
        "{listed}"
    );
    assert_eq!(listed["result"]["issues"][0]["ignored"], true, "{listed}");
    let base = "/api/repairs/issues/fix";
    let form: Value = restarted
        .post(
            base,
            json!({"handler":"tag","issue_id":"sdk_process_fixture"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(form["type"], "form", "{form}");
    assert_eq!(form["data_schema"], json!([]), "{form}");
    assert_eq!(form["description_placeholders"], json!({"name":"Fixture"}));
    let flow_path = format!("{base}/{}", form["flow_id"].as_str().unwrap());
    let finished: Value = restarted
        .post(&flow_path, json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(finished["type"], "create_entry", "{finished}");
    assert_eq!(
        restarted.get(&flow_path).await.status(),
        StatusCode::NOT_FOUND
    );
    let listed = ws_command(&mut socket, json!({"id":2,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["result"]["issues"], json!([]), "{listed}");
    socket.close(None).await.unwrap();
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();

    let mut final_start = Server::start(&config).await;
    let mut socket = authenticated_socket(&final_start).await;
    let listed = ws_command(&mut socket, json!({"id":1,"type":"repairs/list_issues"})).await;
    assert_eq!(
        listed["result"]["issues"],
        json!([]),
        "completed repair stays deleted: {listed}"
    );
    socket.close(None).await.unwrap();
    final_start.child.kill().await.unwrap();
    final_start.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_raw_log_stream_does_not_echo_its_own_frames() {
    use futures_util::{SinkExt, StreamExt};
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(
        &config,
        "recorder:\n  db_path: state.sqlite\nlogger:\n  default: notset\nsystem_log: {}\n",
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(
        ws_command(
            &mut socket,
            json!({"id":1,"type":"system_log/subscribe_raw"})
        )
        .await["success"],
        true
    );
    let initial = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial: Value = serde_json::from_str(initial.to_text().unwrap()).unwrap();
    assert!(
        initial["event"].is_array(),
        "initial live collection is an array"
    );
    socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":2,"type":"call_service","domain":"system_log","service":"write","service_data":{"level":"warning","message":"no-echo process fixture"}}).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut acknowledged = false;
        let mut captured = false;
        while !acknowledged || !captured {
            let frame = socket.next().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            if message["type"] == "result" && message["id"] == 2 {
                assert_eq!(message["success"], true, "{message}");
                acknowledged = true;
            } else {
                assert_eq!(message["type"], "event", "{message}");
                captured |= message["event"].as_array().unwrap().iter().any(|line| line.as_str().unwrap().contains("WARNING [homeassistant.components.system_log.external] no-echo process fixture"));
            }
        }
    }).await.expect("the actual emitted warning and service acknowledgement must arrive");
    // Allow a final real connection record, then require silence for more than
    // two debounce windows while TRACE output remains enabled in the child.
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            match tokio::time::timeout(Duration::from_millis(1_100), socket.next()).await {
                Err(_) => break,
                Ok(Some(Ok(frame))) => {
                    let event: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                    assert_eq!(event["type"], "event", "{event}");
                }
                other => panic!("raw stream closed unexpectedly: {other:?}"),
            }
        }
    })
    .await
    .expect("raw output must not continuously generate another raw output batch");
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_system_log_emits_filters_streams_and_restarts() {
    use futures_util::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(&config, "recorder:\n  db_path: state.sqlite\nsystem_log:\n  max_entries: 2\nlogger:\n  default: info\n  logs:\n    fixture.real: debug\n  filters:\n    fixture.real: ['^suppress$']\n").unwrap();
    let mut server = Server::start(&config).await;
    assert!(
        server
            .post("/api/services/system_log/clear", json!({}))
            .await
            .status()
            .is_success()
    );
    let mut socket = authenticated_socket(&server).await;
    let subscribed = ws_command(
        &mut socket,
        json!({"id":1,"type":"system_log/subscribe_raw"}),
    )
    .await;
    assert_eq!(subscribed["success"], true, "{subscribed}");
    let initial = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial: Value = serde_json::from_str(initial.to_text().unwrap()).unwrap();
    assert_eq!(initial["type"], "event");
    assert_eq!(initial["event"], json!([]));
    for (level, message) in [
        ("debug", "process debug fixture"),
        ("critical", "process critical fixture"),
        ("warning", "process warning fixture"),
        ("warning", "suppress"),
    ] {
        let response = server
            .post(
                "/api/services/system_log/write",
                json!({"logger":"fixture.real","level":level,"message":message}),
            )
            .await;
        assert!(response.status().is_success(), "{}", response.status());
    }
    let lines = tokio::time::timeout(Duration::from_secs(5), async {
        let mut lines = Vec::new();
        while lines.len() < 3 {
            let frame = socket.next().await.unwrap().unwrap();
            let event: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(event["type"], "event", "{event}");
            assert_eq!(event["id"], 1);
            lines.extend(
                event["event"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| line.as_str().unwrap().to_owned()),
            );
        }
        lines
    })
    .await
    .expect("actual emitted debug/warning/critical records must reach the raw stream");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("DEBUG [fixture.real] process debug fixture")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("CRITICAL [fixture.real] process critical fixture")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("suppress")),
        "{lines:?}"
    );
    let listed = ws_command(&mut socket, json!({"id":2,"type":"system_log/list"})).await;
    assert_eq!(listed["success"], true, "{listed}");
    let entries = listed["result"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{listed}");
    assert!(
        entries
            .iter()
            .all(|entry| entry["name"] == "fixture.real" && entry["count"] == 2),
        "{listed}"
    );
    assert!(
        entries.iter().any(|entry| entry["level"] == "CRITICAL"
            && entry["message"] == json!(["process critical fixture", "process warning fixture"])),
        "{listed}"
    );
    let unsubscribed = ws_command(
        &mut socket,
        json!({"id":3,"type":"unsubscribe_events","subscription":1}),
    )
    .await;
    assert_eq!(unsubscribed["success"], true, "{unsubscribed}");
    // Installed native integrations are valid filters even without a configured
    // entry. This performs no MQTT connection or provider activity.
    let subscribed = ws_command(
        &mut socket,
        json!({"id":4,"type":"system_log/subscribe_raw","integration":"mqtt"}),
    )
    .await;
    assert_eq!(subscribed["success"], true, "{subscribed}");
    let initial = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial: Value = serde_json::from_str(initial.to_text().unwrap()).unwrap();
    assert_eq!(initial["id"], 4);
    assert_eq!(initial["event"], json!([]));
    assert_eq!(
        ws_command(
            &mut socket,
            json!({"id":5,"type":"unsubscribe_events","subscription":4})
        )
        .await["success"],
        true
    );
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restarted = Server::start(&config).await;
    let mut socket = authenticated_socket(&restarted).await;
    let listed = ws_command(&mut socket, json!({"id":1,"type":"system_log/list"})).await;
    assert_eq!(listed["success"], true, "{listed}");
    assert!(
        !listed["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "fixture.real"),
        "retained errors are process-local: {listed}"
    );
    socket.close(None).await.unwrap();
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_sunrise_offset_runs_on_actual_clock() {
    // Move a calculated sunrise close to the actual UTC clock with the same
    // supported offset the production scheduler consumes. No injected event or
    // test clock can cause the action to run in the child process.
    let now = chrono::Utc::now();
    let sunrise = hass_sun::next_event(
        hass_sun::Observer {
            latitude: 42.0,
            longitude: -83.0,
            elevation_m: 0.0,
        },
        now,
        chrono_tz::America::Detroit,
        hass_sun::SolarEvent::Sunrise,
        chrono::Duration::zero(),
    )
    .unwrap();
    let offset = now.timestamp() + 12 - sunrise.timestamp();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(
        &config,
        format!(
            r#"homeassistant:
  latitude: 42
  longitude: -83
  elevation: 0
  time_zone: America/Detroit
sun:
recorder:
  db_path: state.sqlite
counter:
  sunrise_actions:
    initial: 0
input_text:
  sunrise_trigger:
    initial: waiting
automation:
  - id: real_sunrise_offset
    triggers:
      - platform: sun
        event: sunrise
        offset: {offset}
        id: observed_sunrise
        alias: actual UTC sunrise
    actions:
      - action: counter.increment
        target:
          entity_id: counter.sunrise_actions
      - action: input_text.set_value
        target:
          entity_id: input_text.sunrise_trigger
        data:
          value: "{{{{ trigger.platform }}}}|{{{{ trigger.event }}}}|{{{{ trigger.id }}}}|{{{{ trigger.idx }}}}|{{{{ trigger.alias }}}}"
"#
        ),
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    server.wait_state("counter.sunrise_actions", "0").await;
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let state: Value = server
                .get("/api/states/input_text.sunrise_trigger")
                .await
                .json()
                .await
                .unwrap();
            if state["state"] == "sun|sunrise|observed_sunrise|0|actual UTC sunrise" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("actual Sun timer must execute the service sequence");
    let count: Value = server
        .get("/api/states/counter.sunrise_actions")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(count["state"], "1");
    assert!(count["context"]["parent_id"].is_string(), "{count}");
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_sun_flow_enable_location_history_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(&config,"homeassistant:\n  latitude: 42\n  longitude: -83\n  elevation: 0\n  time_zone: America/Detroit\nrecorder:\n  db_path: state.sqlite\n").unwrap();
    let mut server = Server::start(&config).await;
    assert_eq!(
        server.get("/api/states/sun.sun").await.status(),
        StatusCode::NOT_FOUND
    );
    let flow_base = "/api/config/config_entries/flow";
    let form: Value = server
        .post(flow_base, json!({"handler":"sun"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(form["type"], "form", "{form}");
    let before_setup = chrono::Utc::now();
    let created: Value = server
        .post(
            &format!("{flow_base}/{}", form["flow_id"].as_str().unwrap()),
            json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(created["type"], "create_entry", "{created}");
    assert_eq!(created["result"]["state"], "loaded", "{created}");
    assert_eq!(created["result"]["title"], "Sun");
    let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
    let main: Value = server
        .get("/api/states/sun.sun")
        .await
        .json()
        .await
        .unwrap();
    assert!(matches!(
        main["state"].as_str(),
        Some("above_horizon" | "below_horizon")
    ));
    assert!((-90.0..=90.0).contains(&main["attributes"]["elevation"].as_f64().unwrap()));
    assert!((0.0..=360.0).contains(&main["attributes"]["azimuth"].as_f64().unwrap()));
    let old_noon =
        chrono::DateTime::parse_from_rfc3339(main["attributes"]["next_noon"].as_str().unwrap())
            .unwrap();
    for field in [
        "next_dawn",
        "next_dusk",
        "next_midnight",
        "next_noon",
        "next_rising",
        "next_setting",
    ] {
        let event =
            chrono::DateTime::parse_from_rfc3339(main["attributes"][field].as_str().unwrap())
                .unwrap();
        assert!(event > before_setup);
        assert!(event < before_setup + chrono::Duration::days(367));
    }
    let history: Value = server
        .get("/api/history/period?filter_entity_id=sun.sun")
        .await
        .json()
        .await
        .unwrap();
    let rows = history[0].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        for field in [
            "elevation",
            "azimuth",
            "rising",
            "next_dawn",
            "next_dusk",
            "next_midnight",
            "next_noon",
            "next_rising",
            "next_setting",
        ] {
            assert!(row["attributes"].get(field).is_none(), "{row}");
        }
    }
    let position_id = "sensor.sun_solar_elevation";
    assert_eq!(
        server
            .get(&format!("/api/states/{position_id}"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut socket = authenticated_socket(&server).await;
    let registry = ws_command(
        &mut socket,
        json!({"id":1,"type":"config/entity_registry/list"}),
    )
    .await;
    let entry = registry["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["entity_id"] == position_id)
        .unwrap()
        .clone();
    assert_eq!(entry["disabled_by"], "integration");
    assert_eq!(ws_command(&mut socket,json!({"id":2,"type":"config/entity_registry/update","entity_id":position_id,"disabled_by":null})).await["success"],true);
    let enabled = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = server.get(&format!("/api/states/{position_id}")).await;
            if response.status() == StatusCode::OK {
                break response.json::<Value>().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("enabling must publish without waiting for the phase poll");
    assert_eq!(enabled["attributes"]["state_class"], "measurement");
    assert_eq!(enabled["attributes"]["unit_of_measurement"], "°");
    assert_eq!(ws_command(&mut socket,json!({"id":3,"type":"config/core/update","latitude":-42,"longitude":97,"elevation":58,"time_zone":"Asia/Bangkok"})).await["success"],true);
    let updated = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state: Value = server
                .get("/api/states/sun.sun")
                .await
                .json()
                .await
                .unwrap();
            let noon = chrono::DateTime::parse_from_rfc3339(
                state["attributes"]["next_noon"].as_str().unwrap(),
            )
            .unwrap();
            if (noon - old_noon).num_hours().abs() >= 10 {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("saved location must recalculate solar events");
    assert_ne!(
        updated["attributes"]["next_noon"],
        main["attributes"]["next_noon"]
    );
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();

    let mut restarted = Server::start(&config).await;
    let core: Value = restarted.get("/api/config").await.json().await.unwrap();
    assert_eq!(core["latitude"], -42.0);
    assert_eq!(core["longitude"], 97.0);
    assert_eq!(core["time_zone"], "Asia/Bangkok");
    assert_eq!(
        restarted
            .get(&format!("/api/states/{position_id}"))
            .await
            .status(),
        StatusCode::OK
    );
    let mut socket = authenticated_socket(&restarted).await;
    let registry = ws_command(
        &mut socket,
        json!({"id":1,"type":"config/entity_registry/list"}),
    )
    .await;
    let restored = registry["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["entity_id"] == position_id)
        .unwrap();
    assert_eq!(restored["id"], entry["id"]);
    assert!(restored["disabled_by"].is_null());
    assert_eq!(
        restarted
            .client
            .delete(format!(
                "{}/api/config/config_entries/entry/{entry_id}",
                restarted.url
            ))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        restarted.get("/api/states/sun.sun").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        restarted
            .get(&format!("/api/states/{position_id}"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        restarted
            .get("/api/states/sensor.sun_next_noon")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    socket.close(None).await.unwrap();
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_logger_settings_obey_permanent_once_and_session_lifetimes() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(
        &config,
        "recorder:\n  db_path: state.sqlite\nlogger:\n  default: info\n",
    )
    .unwrap();
    async fn logger_level(socket: &mut Socket, id: u64) -> i64 {
        let info = ws_command(socket, json!({"id":id,"type":"logger/log_info"})).await;
        assert_eq!(info["success"], true, "{info}");
        info["result"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["domain"] == "logger")
            .unwrap()["level"]
            .as_i64()
            .unwrap()
    }
    async fn stop(server: &mut Server, socket: &mut Socket) {
        socket.close(None).await.unwrap();
        server.child.kill().await.unwrap();
        server.child.wait().await.unwrap();
    }
    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(logger_level(&mut socket, 1).await, 20);
    assert_eq!(ws_command(&mut socket,json!({"id":2,"type":"logger/integration_log_level","integration":"logger","level":"DEBUG","persistence":"permanent"})).await["success"],true);
    assert_eq!(logger_level(&mut socket, 3).await, 10);
    stop(&mut server, &mut socket).await;

    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(logger_level(&mut socket, 1).await, 10);
    assert_eq!(ws_command(&mut socket,json!({"id":2,"type":"logger/integration_log_level","integration":"logger","level":"WARNING","persistence":"once"})).await["success"],true);
    assert_eq!(logger_level(&mut socket, 3).await, 30);
    stop(&mut server, &mut socket).await;

    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(logger_level(&mut socket, 1).await, 30);
    stop(&mut server, &mut socket).await;

    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(logger_level(&mut socket, 1).await, 20);
    assert_eq!(
        server
            .post(
                "/api/services/logger/set_default_level",
                json!({"level":"error"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(logger_level(&mut socket, 2).await, 40);
    stop(&mut server, &mut socket).await;

    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    assert_eq!(logger_level(&mut socket, 1).await, 20);
    stop(&mut server, &mut socket).await;
}

#[tokio::test]
async fn real_process_tags_registry_and_trigger_survive_hard_restart() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("configuration.yaml");
    std::fs::write(
        &config,
        r#"
recorder:
  db_path: state.sqlite
tag: {}
input_text:
  tag_message:
    initial: waiting
automation:
  - id: tag_fixture
    triggers:
      - trigger: tag
        tag_id: process-tag
        device_id: scanner-fixture
    actions:
      - action: input_text.set_value
        target:
          entity_id: input_text.tag_message
        data:
          value: "{{ trigger.event.data.tag_id }}:{{ trigger.event.data.device_id }}"
"#,
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;
    let created = ws_command(&mut socket, json!({"id":1,"type":"tag/create","tag_id":"process-tag","name":"Kitchen","last_scanned":"2026-01-02T03:04:05"})).await;
    assert_eq!(created["success"], true, "{created}");
    let registry = ws_command(
        &mut socket,
        json!({"id":2,"type":"config/entity_registry/list"}),
    )
    .await;
    let entity = registry["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["platform"] == "tag" && entry["unique_id"] == "process-tag")
        .unwrap()
        .clone();
    let entity_id = entity["entity_id"].as_str().unwrap();
    server
        .wait_state(entity_id, "2026-01-02T03:04:05.000")
        .await;
    let current: Value = server
        .get(&format!("/api/states/{entity_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(current["attributes"]["tag_id"], "process-tag");
    let history: Value = server
        .get(&format!("/api/history/period?filter_entity_id={entity_id}"))
        .await
        .json()
        .await
        .unwrap();
    let rows = history[0].as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["attributes"].get("tag_id").is_none())
    );
    assert_eq!(
        ws_command(
            &mut socket,
            json!({"id":3,"type":"tag/update","tag_id":"process-tag","name":"Renamed kitchen"})
        )
        .await["success"],
        true
    );
    assert_eq!(
        server
            .post(
                "/api/events/tag_scanned",
                json!({"tag_id":"process-tag","device_id":"scanner-fixture"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    server
        .wait_state("input_text.tag_message", "process-tag:scanner-fixture")
        .await;
    // Publishing the public event drives automations; it does not impersonate
    // the native scanner's durable collection update.
    let listed = ws_command(&mut socket, json!({"id":4,"type":"tag/list"})).await;
    assert_eq!(listed["result"][0]["last_scanned"], "2026-01-02T03:04:05");
    assert_eq!(listed["result"][0]["name"], "Renamed kitchen");
    socket.close(None).await.unwrap();
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();

    let mut restarted = Server::start(&config).await;
    restarted
        .wait_state(entity_id, "2026-01-02T03:04:05.000")
        .await;
    let mut socket = authenticated_socket(&restarted).await;
    let registry = ws_command(
        &mut socket,
        json!({"id":1,"type":"config/entity_registry/list"}),
    )
    .await;
    let restored = registry["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["unique_id"] == "process-tag")
        .unwrap();
    assert_eq!(restored["id"], entity["id"]);
    assert_eq!(restored["entity_id"], entity["entity_id"]);
    assert_eq!(restored["name"], "Renamed kitchen");
    assert_eq!(
        ws_command(
            &mut socket,
            json!({"id":2,"type":"tag/delete","tag_id":"process-tag"})
        )
        .await["success"],
        true
    );
    assert_eq!(
        restarted
            .get(&format!("/api/states/{entity_id}"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    socket.close(None).await.unwrap();
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
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

#[tokio::test]
async fn real_process_webhook_executes_private_trigger_and_reregisters_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("configuration.yaml");
    std::fs::write(
        &config,
        r#"
recorder:
  db_path: state.sqlite
input_text:
  webhook_result:
    initial: waiting
counter:
  webhook_count:
    initial: 0
automation:
  - id: inbound_webhook
    triggers:
      - trigger: webhook
        webhook_id: compiled_process_webhook_fixture
        allowed_methods: [POST]
        local_only: true
    actions:
      - action: input_text.set_value
        target:
          entity_id: input_text.webhook_result
        data:
          value: "{{ trigger.json.message }}|{{ trigger.query.origin }}"
      - action: counter.increment
        target:
          entity_id: counter.webhook_count
"#,
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    let response = server
        .client
        .post(format!(
            "{}/api/webhook/compiled_process_webhook_fixture?origin=network",
            server.url
        ))
        .json(&json!({"message":"first"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    server
        .wait_state("input_text.webhook_result", "first|network")
        .await;
    server.wait_state("counter.webhook_count", "1").await;
    let response = server
        .client
        .get(format!(
            "{}/api/webhook/compiled_process_webhook_fixture",
            server.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let mut restarted = Server::start(&config).await;
    restarted.wait_state("counter.webhook_count", "1").await;
    let response = restarted
        .client
        .post(format!(
            "{}/api/webhook/compiled_process_webhook_fixture?origin=restart",
            restarted.url
        ))
        .json(&json!({"message":"second"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    restarted
        .wait_state("input_text.webhook_result", "second|restart")
        .await;
    restarted.wait_state("counter.webhook_count", "2").await;
    restarted.child.kill().await.unwrap();
    restarted.child.wait().await.unwrap();
}

#[tokio::test]
async fn real_process_clock_yaml_and_config_entries_use_actual_time_and_restore_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("configuration.yaml");
    std::fs::write(
        &config,
        r#"
homeassistant:
  time_zone: America/New_York
recorder:
  db_path: state.sqlite
sensor:
  - platform: time_date
    display_options: [date, time_utc]
  - platform: time_date
    entity_namespace: office
    display_options: [time]
    scan_interval: "00:02:00"
"#,
    )
    .unwrap();
    let mut server = Server::start(&config).await;
    let date: Value = server
        .get("/api/states/sensor.date")
        .await
        .json()
        .await
        .unwrap();
    assert!(chrono::NaiveDate::parse_from_str(date["state"].as_str().unwrap(), "%Y-%m-%d").is_ok());
    let time: Value = server
        .get("/api/states/sensor.time_utc")
        .await
        .json()
        .await
        .unwrap();
    assert!(chrono::NaiveTime::parse_from_str(time["state"].as_str().unwrap(), "%H:%M").is_ok());
    let flow_base = "/api/config/config_entries/flow";
    assert_eq!(
        server.get("/api/states/sensor.office_time").await.status(),
        StatusCode::OK
    );
    let mut entry_ids = Vec::new();
    let before_setup = chrono::Utc::now();
    for (domain, input) in [
        ("time_date", json!({"display_options":"date_time_utc"})),
        ("uptime", json!({})),
    ] {
        let form: Value = server
            .post(flow_base, json!({"handler":domain}))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(form["type"], "form", "{form}");
        let created: Value = server
            .post(
                &format!("{flow_base}/{}", form["flow_id"].as_str().unwrap()),
                input,
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(created["type"], "create_entry", "{created}");
        assert_eq!(created["result"]["state"], "loaded", "{created}");
        entry_ids.push(created["result"]["entry_id"].as_str().unwrap().to_owned());
    }
    let uptime: Value = server
        .get("/api/states/sensor.uptime")
        .await
        .json()
        .await
        .unwrap();
    let setup_time =
        chrono::DateTime::parse_from_rfc3339(uptime["state"].as_str().unwrap()).unwrap();
    assert!(setup_time >= before_setup && setup_time <= chrono::Utc::now());
    assert_eq!(uptime["attributes"]["device_class"], "uptime");
    let entry_sensor: Value = server
        .get("/api/states/sensor.date_time_utc")
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(entry_sensor["state"], "unknown");
    assert_ne!(entry_sensor["state"], "unavailable");
    let duplicate_form: Value = server
        .post(flow_base, json!({"handler":"uptime"}))
        .await
        .json()
        .await
        .unwrap();
    let duplicate: Value = server
        .post(
            &format!(
                "{flow_base}/{}",
                duplicate_form["flow_id"].as_str().unwrap()
            ),
            json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(duplicate["type"], "abort");
    assert_eq!(duplicate["reason"], "single_instance_allowed");
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let yaml = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, yaml.replace(
        "  - platform: time_date\n    entity_namespace: office\n    display_options: [time]\n    scan_interval: \"00:02:00\"\n", "",
    )).unwrap();
    let mut restarted = Server::start(&config).await;
    assert_eq!(
        restarted
            .get("/api/states/sensor.office_time")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let current: Value = restarted
        .get("/api/states/sensor.uptime")
        .await
        .json()
        .await
        .unwrap();
    let restarted_time =
        chrono::DateTime::parse_from_rfc3339(current["state"].as_str().unwrap()).unwrap();
    assert!(
        restarted_time > setup_time,
        "uptime is a fresh setup timestamp after restart"
    );
    let entries: Value = restarted
        .get("/api/config/config_entries/entry")
        .await
        .json()
        .await
        .unwrap();
    for entry_id in &entry_ids {
        assert!(
            entries
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["entry_id"] == *entry_id && entry["state"] == "loaded")
        );
    }
    let removed = restarted
        .client
        .delete(format!(
            "{}/api/config/config_entries/entry/{}",
            restarted.url, entry_ids[0]
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    assert_eq!(
        restarted
            .get("/api/states/sensor.date_time_utc")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        restarted.get("/api/states/sensor.date").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        restarted.get("/api/states/sensor.uptime").await.status(),
        StatusCode::OK
    );
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
