#![cfg(unix)]

//! Acceptance coverage for `homeassistant.restart` in the compiled process.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    task::JoinHandle,
    time::Instant,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::CloseFrame},
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ENTITY_ID: &str = "sensor.restart_process_fixture";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type CapturedOutput = Arc<Mutex<Vec<String>>>;

struct Server {
    child: Child,
    address: SocketAddr,
    output: CapturedOutput,
    readers: Vec<JoinHandle<()>>,
    reaped: bool,
}

impl Server {
    async fn start(config: &Path, database: &Path, address: SocketAddr) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hass"))
            .arg("--config")
            .arg(config)
            .arg("--host")
            .arg(address.ip().to_string())
            .arg("--port")
            .arg(address.port().to_string())
            .arg("--database")
            .arg(database)
            .env("HASS_TOKEN", TOKEN)
            .env_remove("HASS_TOKEN_FILE")
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("compiled hass process should start");

        let output = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().expect("capture child stdout");
        let stderr = child.stderr.take().expect("capture child stderr");
        let readers = vec![
            capture_lines(stdout, output.clone()),
            capture_lines(stderr, output.clone()),
        ];
        let mut server = Self {
            child,
            address,
            output,
            readers,
            reaped: false,
        };

        let mut socket = server.wait_for_boot(1).await;
        socket
            .close(None)
            .await
            .expect("close initial readiness WebSocket");
        server
    }

    fn websocket_url(&self) -> String {
        format!("ws://{}/api/websocket", self.address)
    }

    fn lines(&self) -> Vec<String> {
        self.output
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }

    fn diagnostics(&self) -> String {
        let lines = self.lines();
        if lines.is_empty() {
            "child stdout/stderr is empty".to_owned()
        } else {
            lines.join("\n")
        }
    }

    fn boot_count(&self) -> usize {
        self.lines()
            .iter()
            .filter(|line| line.contains("Rust runtime listening"))
            .count()
    }

    async fn wait_for_boot(&mut self, boot_count: usize) -> Socket {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut last_error = "listener has not accepted an authenticated WebSocket".to_owned();

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => panic!(
                    "compiled hass exited before boot {boot_count} was ready ({status})\n{}",
                    self.diagnostics()
                ),
                Ok(None) => {}
                Err(error) => panic!(
                    "cannot check compiled hass process before boot {boot_count}: {error}\n{}",
                    self.diagnostics()
                ),
            }

            if self.boot_count() >= boot_count {
                match tokio::time::timeout(CONNECT_TIMEOUT, self.authenticated_socket()).await {
                    Ok(Ok(socket)) => return socket,
                    Ok(Err(error)) => last_error = error,
                    Err(_) => last_error = "WebSocket authentication attempt timed out".into(),
                }
            }

            if Instant::now() >= deadline {
                panic!(
                    "compiled hass did not expose loopback WebSocket boot {boot_count} before deadline; last error: {last_error}\n{}",
                    self.diagnostics()
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn authenticated_socket(&self) -> Result<Socket, String> {
        let (mut socket, _) = tokio_tungstenite::connect_async(self.websocket_url())
            .await
            .map_err(|error| format!("WebSocket connect failed: {error}"))?;
        let required = read_json(&mut socket).await?;
        if required["type"] != "auth_required" {
            return Err(format!("expected auth_required, received {required}"));
        }

        socket
            .send(Message::Text(
                json!({"type":"auth", "access_token":TOKEN})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| format!("send WebSocket auth failed: {error}"))?;
        let accepted = read_json(&mut socket).await?;
        if accepted["type"] != "auth_ok" {
            return Err(format!("expected auth_ok, received {accepted}"));
        }
        Ok(socket)
    }

    async fn stop(&mut self) {
        let pid = self.child.id().expect("running child has a process id");
        let signal = std::process::Command::new("/bin/kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .expect("send SIGTERM to the compiled child");
        assert!(
            signal.success(),
            "SIGTERM should be delivered to child {pid}\n{}",
            self.diagnostics()
        );

        let status = match tokio::time::timeout(STARTUP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => panic!("reap compiled hass child: {error}\n{}", self.diagnostics()),
            Err(_) => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                self.reaped = true;
                panic!(
                    "compiled hass child did not stop in time\n{}",
                    self.diagnostics()
                );
            }
        };
        self.reaped = true;
        for reader in self.readers.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
        }
        assert!(
            status.success(),
            "compiled hass should stop cleanly: {status}\n{}",
            self.diagnostics()
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if !self.reaped {
            let pid = self.child.id();
            let _ = self.child.start_kill();
            eprintln!(
                "cleaning up compiled hass child {pid:?} after test exit:\n{}",
                self.diagnostics()
            );
        }
    }
}

fn capture_lines<R>(reader: R, output: CapturedOutput) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut captured) = output.lock() {
                captured.push(line);
            }
        }
    })
}

async fn read_json(socket: &mut Socket) -> Result<Value, String> {
    let frame = tokio::time::timeout(COMMAND_TIMEOUT, socket.next())
        .await
        .map_err(|_| "WebSocket response timed out".to_owned())?
        .ok_or_else(|| "WebSocket closed before its response".to_owned())?
        .map_err(|error| format!("WebSocket read failed: {error}"))?;
    let text = frame
        .to_text()
        .map_err(|error| format!("expected WebSocket text response: {error}"))?;
    serde_json::from_str(text).map_err(|error| format!("invalid JSON WebSocket response: {error}"))
}

async fn ws_command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send WebSocket command");
    read_json(socket)
        .await
        .expect("read WebSocket command response")
}

fn assert_success(response: &Value, id: u64) -> &Value {
    assert_eq!(response["id"], id, "response correlation id: {response}");
    assert_eq!(response["type"], "result", "response type: {response}");
    assert_eq!(
        response["success"], true,
        "command should succeed: {response}"
    );
    &response["result"]
}

async fn set_fixture_state(server: &Server) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/api/states/{ENTITY_ID}", server.address))
        .bearer_auth(TOKEN)
        .json(&json!({
            "state": "persisted-before-restart",
            "attributes": {"fixture": "restart_process"}
        }))
        .send()
        .await
        .expect("write fixture state through the compiled process");
    let status = response.status();
    let body = response
        .text()
        .await
        .expect("read fixture state write response");
    assert!(
        status.is_success(),
        "fixture state write failed ({status}): {body}\n{}",
        server.diagnostics()
    );
    let state: Value = serde_json::from_str(&body).expect("fixture state response should be JSON");
    assert_eq!(
        state["entity_id"], ENTITY_ID,
        "created fixture state: {state}"
    );
    assert_eq!(
        state["state"], "persisted-before-restart",
        "created fixture state: {state}"
    );
}

async fn await_clean_close(socket: &mut Socket) -> Result<Option<CloseFrame>, String> {
    tokio::time::timeout(CLOSE_TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => return Ok(frame),
                Some(Ok(Message::Ping(payload))) => socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| format!("reply to WebSocket ping: {error}"))?,
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(message)) => {
                    return Err(format!(
                        "unexpected frame before clean WebSocket close: {message:?}"
                    ));
                }
                Some(Err(error)) => return Err(format!("unclean WebSocket close: {error}")),
                None => return Err("WebSocket ended without a close frame".into()),
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for the old WebSocket close handshake".to_owned())?
}

fn available_loopback_address() -> SocketAddr {
    let reservation =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve a free loopback port");
    reservation
        .local_addr()
        .expect("read reserved loopback address")
}

#[tokio::test]
async fn compiled_process_restart_reexecs_in_place_and_preserves_state_and_config() {
    let directory = tempfile::tempdir().expect("create temporary restart fixture directory");
    let config = directory.path().join("configuration.yaml");
    let database = directory.path().join("state.sqlite");
    std::fs::write(
        &config,
        "homeassistant:\n  name: Compiled Restart Fixture\n  time_zone: UTC\n  unit_system: metric\nrecorder:\n  db_path: state.sqlite\n",
    )
    .expect("write temporary configuration");
    let address = available_loopback_address();
    let mut server = Server::start(&config, &database, address).await;
    let pid = server.child.id().expect("running child has a process id");
    assert_eq!(
        server.boot_count(),
        1,
        "first process boot marker\n{}",
        server.diagnostics()
    );

    set_fixture_state(&server).await;

    let mut socket = server
        .authenticated_socket()
        .await
        .expect("authenticate initial WebSocket");
    let initial_config = ws_command(&mut socket, json!({"id": 1, "type": "get_config"})).await;
    let initial_config = assert_success(&initial_config, 1);
    assert_eq!(initial_config["location_name"], "Compiled Restart Fixture");
    assert_eq!(initial_config["time_zone"], "UTC");

    let boot_count = server.boot_count();
    let restart = ws_command(
        &mut socket,
        json!({
            "id": 2,
            "type": "call_service",
            "domain": "homeassistant",
            "service": "restart",
            "service_data": {},
            "target": {}
        }),
    )
    .await;
    let result = assert_success(&restart, 2);
    assert_eq!(
        result["changed_states"],
        json!([]),
        "restart result: {result}"
    );
    assert_eq!(result["response"], Value::Null, "restart result: {result}");
    assert!(
        result["context"]["id"]
            .as_str()
            .is_some_and(|context_id| !context_id.is_empty()),
        "restart response should preserve a generated service context: {result}"
    );

    await_clean_close(&mut socket)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "restart should cleanly close the old WebSocket: {error}\n{}",
                server.diagnostics()
            )
        });
    assert_eq!(
        server.child.id(),
        Some(pid),
        "Unix exec should preserve the child PID before the next boot\n{}",
        server.diagnostics()
    );

    let mut restarted_socket = server.wait_for_boot(boot_count + 1).await;
    assert_eq!(
        server.child.id(),
        Some(pid),
        "Unix exec should keep one child process owning the listener\n{}",
        server.diagnostics()
    );
    assert_eq!(
        server.address, address,
        "restart should reuse the fixed loopback address"
    );
    assert!(
        TcpListener::bind(address).is_err(),
        "the restarted process should be the sole listener owner at {address}"
    );

    let restarted_config = ws_command(
        &mut restarted_socket,
        json!({"id": 3, "type": "get_config"}),
    )
    .await;
    let restarted_config = assert_success(&restarted_config, 3);
    assert_eq!(
        restarted_config["location_name"],
        "Compiled Restart Fixture"
    );
    assert_eq!(restarted_config["time_zone"], "UTC");

    let states = ws_command(
        &mut restarted_socket,
        json!({"id": 4, "type": "get_states"}),
    )
    .await;
    let states = assert_success(&states, 4)
        .as_array()
        .expect("get_states should return a list");
    let fixture_state = states
        .iter()
        .find(|state| state["entity_id"] == ENTITY_ID)
        .expect("fixture state should survive exec and SQLite reopen");
    assert_eq!(fixture_state["state"], "persisted-before-restart");
    assert_eq!(fixture_state["attributes"]["fixture"], "restart_process");

    restarted_socket
        .close(None)
        .await
        .expect("close restarted WebSocket before SIGTERM");

    let boot_count = server.boot_count();
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(COMMAND_TIMEOUT)
        .build()
        .unwrap()
        .post(format!(
            "http://{address}/api/services/homeassistant/restart"
        ))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .expect("HTTP restart must acknowledge its own shutdown request");
    assert!(response.status().is_success(), "{}", server.diagnostics());
    assert_eq!(response.json::<Value>().await.unwrap(), json!([]));
    let mut third_socket = server.wait_for_boot(boot_count + 1).await;
    assert_eq!(
        server.child.id(),
        Some(pid),
        "HTTP restart also execs in place"
    );
    third_socket.close(None).await.unwrap();
    server.stop().await;
}
