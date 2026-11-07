//! Acceptance coverage for the compiled process' real Network integration.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    task::JoinHandle,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const INTERNAL_URL: &str = "http://127.0.0.1:8123";
const EXTERNAL_URL: &str = "https://127.0.0.1:8443";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    child: Child,
    url: String,
    stdout: JoinHandle<()>,
}

impl Server {
    async fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hass"))
            .arg("--config")
            .arg(config)
            .arg("--port")
            .arg("0")
            .env("HASS_TOKEN", TOKEN)
            .env_remove("HASS_TOKEN_FILE")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("compiled hass process should start");

        // Read the child's actual ephemeral bind rather than reserving a port
        // in this process and racing the child when that reservation closes.
        let mut lines = BufReader::new(
            child
                .stdout
                .take()
                .expect("child stdout is captured for readiness"),
        )
        .lines();
        let port = tokio::time::timeout(STARTUP_TIMEOUT, async {
            while let Some(line) = lines
                .next_line()
                .await
                .expect("readiness output should be readable")
            {
                if line.contains("Rust runtime listening")
                    && let Some((_, address)) = line.split_once("127.0.0.1:")
                {
                    let port: String = address.chars().take_while(char::is_ascii_digit).collect();
                    return port
                        .parse::<u16>()
                        .expect("runtime should announce its ephemeral port");
                }
            }
            panic!(
                "compiled hass exited before announcing its listener: {:?}",
                child.wait().await
            );
        })
        .await
        .expect("compiled hass should announce its listener");
        let stdout =
            tokio::spawn(async move { while matches!(lines.next_line().await, Ok(Some(_))) {} });

        Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            stdout,
        }
    }

    async fn stop(self) {
        let Server {
            mut child, stdout, ..
        } = self;
        let pid = child.id().expect("running child has a process id");

        #[cfg(unix)]
        {
            // SIGTERM drives the runtime's graceful shutdown path; the
            // WebSocket is closed by the caller before this signal is sent.
            let signal = std::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .expect("send SIGTERM to the child process");
            assert!(signal.success(), "SIGTERM should be delivered to child");
        }

        #[cfg(not(unix))]
        {
            // Tokio does not expose a portable graceful signal sender here.
            // This fallback is an intentional hard stop for non-Unix hosts.
            child.start_kill().expect("stop child process");
        }

        let status = match tokio::time::timeout(STARTUP_TIMEOUT, child.wait()).await {
            Ok(status) => status.expect("reap child process"),
            Err(_) => {
                child.kill().await.expect("kill hung child process");
                let _ = child.wait().await;
                panic!("compiled hass child did not stop in time");
            }
        };
        assert!(
            status.success(),
            "compiled hass should stop cleanly: {status}"
        );
        stdout
            .await
            .expect("stdout drain should finish after child shutdown");
    }
}

async fn ws_command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send WebSocket command");
    let frame = tokio::time::timeout(COMMAND_TIMEOUT, socket.next())
        .await
        .expect("WebSocket command should receive a response")
        .expect("WebSocket should remain open")
        .expect("WebSocket response should be valid");
    serde_json::from_str(frame.to_text().expect("response should be text"))
        .expect("response should be JSON")
}

async fn authenticated_socket(server: &Server) -> Socket {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "{}/api/websocket",
        server.url.replacen("http://", "ws://", 1)
    ))
    .await
    .expect("connect over loopback WebSocket");
    let required = tokio::time::timeout(COMMAND_TIMEOUT, socket.next())
        .await
        .expect("server should request WebSocket authentication")
        .expect("WebSocket should stay open")
        .expect("auth_required frame should be valid");
    let required: Value = serde_json::from_str(
        required
            .to_text()
            .expect("auth_required frame should be text"),
    )
    .expect("auth_required frame should be JSON");
    assert_eq!(required["type"], "auth_required", "{required}");

    let accepted = ws_command(&mut socket, json!({"type":"auth", "access_token":TOKEN})).await;
    assert_eq!(accepted["type"], "auth_ok", "{accepted}");
    socket
}

fn assert_success(response: &Value, id: u64) -> &Value {
    assert_eq!(response["id"], id, "response correlation id: {response}");
    assert_eq!(
        response["success"], true,
        "WebSocket command failed: {response}"
    );
    &response["result"]
}

fn assert_adapter_snapshot(adapters: &Value) {
    let adapters = adapters
        .as_array()
        .expect("network result adapters should be an array");
    assert!(
        !adapters.is_empty(),
        "the real system adapter provider returned no OS adapters; loopback should be available"
    );

    let mut has_address = false;
    for adapter in adapters {
        assert!(
            adapter["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty()),
            "adapter name should be a nonempty system interface name: {adapter}"
        );
        assert!(
            adapter["index"].is_null() || adapter["index"].as_u64().is_some(),
            "adapter index should be null or an unsigned integer: {adapter}"
        );
        for field in ["enabled", "auto", "default"] {
            assert!(
                adapter[field].is_boolean(),
                "adapter field {field} should be boolean: {adapter}"
            );
        }

        let ipv4 = adapter["ipv4"]
            .as_array()
            .expect("adapter ipv4 should be an array");
        for record in ipv4 {
            has_address = true;
            assert!(
                record["address"]
                    .as_str()
                    .and_then(|address| address.parse::<Ipv4Addr>().ok())
                    .is_some(),
                "IPv4 record should contain a numeric IPv4 address: {record}"
            );
            assert!(
                record["network_prefix"]
                    .as_u64()
                    .is_some_and(|prefix| prefix <= 32),
                "IPv4 prefix should be an integer from 0 through 32: {record}"
            );
        }

        let ipv6 = adapter["ipv6"]
            .as_array()
            .expect("adapter ipv6 should be an array");
        for record in ipv6 {
            has_address = true;
            assert!(
                record["address"]
                    .as_str()
                    .and_then(|address| address.parse::<Ipv6Addr>().ok())
                    .is_some(),
                "IPv6 record should contain a numeric IPv6 address: {record}"
            );
            assert!(record["flowinfo"].as_u64().is_some(), "{record}");
            assert!(record["scope_id"].as_u64().is_some(), "{record}");
            assert!(
                record["network_prefix"]
                    .as_u64()
                    .is_some_and(|prefix| prefix <= 128),
                "IPv6 prefix should be an integer from 0 through 128: {record}"
            );
        }
    }
    assert!(
        has_address,
        "real system adapter snapshot had no address records; loopback should be available"
    );
}

fn assert_only_adapter_selected(network: &Value, selected_name: &str) {
    assert_eq!(
        network["configured_adapters"],
        json!([selected_name]),
        "configured selection should store the exact adapter name"
    );
    assert_adapter_snapshot(&network["adapters"]);
    let adapters = network["adapters"].as_array().unwrap();
    assert!(
        adapters
            .iter()
            .any(|adapter| adapter["name"] == selected_name),
        "selected system adapter should still appear in the snapshot: {selected_name}"
    );
    for adapter in adapters {
        assert_eq!(
            adapter["enabled"],
            adapter["name"] == selected_name,
            "only the configured adapter name should be enabled: {adapter}"
        );
    }
}

#[tokio::test]
async fn compiled_process_network_admin_selection_and_urls_survive_restart() {
    let directory = tempfile::tempdir().expect("create temporary process directory");
    let config = directory.path().join("configuration.yaml");
    std::fs::write(&config, "recorder:\n  db_path: state.sqlite\n")
        .expect("write minimal temporary configuration");
    let database = directory.path().join("state.sqlite");

    let server = Server::start(&config).await;
    let mut socket = authenticated_socket(&server).await;

    let initial = ws_command(&mut socket, json!({"id":1,"type":"network"})).await;
    let initial = assert_success(&initial, 1).clone();
    assert_eq!(initial["configured_adapters"], json!([]));
    assert_adapter_snapshot(&initial["adapters"]);
    let selected_name = initial["adapters"][0]["name"]
        .as_str()
        .expect("first real adapter has a name")
        .to_owned();

    let configured = ws_command(
        &mut socket,
        json!({
            "id":2,
            "type":"network/configure",
            "config":{"configured_adapters":[selected_name]},
        }),
    )
    .await;
    assert_eq!(
        assert_success(&configured, 2)["configured_adapters"],
        json!([selected_name]),
        "configure should persist the exact selected system adapter name"
    );

    let selected = ws_command(&mut socket, json!({"id":3,"type":"network"})).await;
    let selected = assert_success(&selected, 3).clone();
    assert_only_adapter_selected(&selected, &selected_name);

    let update = ws_command(
        &mut socket,
        json!({
            "id":4,
            "type":"config/core/update",
            "internal_url":INTERNAL_URL,
            "external_url":EXTERNAL_URL,
        }),
    )
    .await;
    assert_success(&update, 4);
    let urls = ws_command(&mut socket, json!({"id":5,"type":"network/url"})).await;
    let urls = assert_success(&urls, 5);
    assert_eq!(urls["internal"], INTERNAL_URL);
    assert_eq!(urls["external"], EXTERNAL_URL);
    assert_eq!(urls["cloud"], Value::Null);
    assert!(
        database.exists(),
        "child should use the temporary SQLite file"
    );

    socket
        .close(None)
        .await
        .expect("close WebSocket before graceful shutdown");
    server.stop().await;

    let restarted = Server::start(&config).await;
    let mut socket = authenticated_socket(&restarted).await;
    let network = ws_command(&mut socket, json!({"id":6,"type":"network"})).await;
    let network = assert_success(&network, 6).clone();
    assert_only_adapter_selected(&network, &selected_name);

    let urls = ws_command(&mut socket, json!({"id":7,"type":"network/url"})).await;
    let urls = assert_success(&urls, 7);
    assert_eq!(
        urls["internal"], INTERNAL_URL,
        "internal URL should persist"
    );
    assert_eq!(
        urls["external"], EXTERNAL_URL,
        "external URL should persist"
    );
    assert_eq!(urls["cloud"], Value::Null);

    socket
        .close(None)
        .await
        .expect("close restarted WebSocket before graceful shutdown");
    restarted.stop().await;
}
