//! Loopback protocol fixtures for the Application Credentials WebSocket API.
//!
//! All storage is a temporary SQLite database and all provider behavior is
//! injected; these tests never call a real OAuth provider or exchange secrets.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_application_credentials::{
    ApplicationCredentialsIntegration, ApplicationCredentialsPlatform, AuthorizationServer,
    CreateCredential, PlatformRegistrationGuard, SUBSCRIPTION_CAPACITY,
};
use hass_auth::AuthManager;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    NewConfigEntry, SetupOutcome,
};
use hass_core::{Runtime, Store};
use hass_storage::SqliteStore;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::watch, task::JoinHandle, time::timeout};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "application-credentials-protocol-fixture";
type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Tokens {
    auth: AuthManager,
    owner_id: String,
    admin_id: String,
    owner: String,
    administrator: String,
    ordinary: String,
    reader: String,
}

struct Fixture {
    _directory: TempDir,
    runtime: Runtime,
    config_entries: ConfigEntries,
    tokens: Tokens,
    integration: ApplicationCredentialsIntegration,
    _platform: PlatformRegistrationGuard,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let store: Arc<dyn Store> = Arc::new(
            SqliteStore::open(directory.path().join("config.sqlite")).expect("SQLite store"),
        );
        let runtime = Runtime::new(store).expect("runtime");
        let config_entries = ConfigEntries::new(runtime.clone()).expect("config entries");
        let tokens = auth_tokens(&runtime);
        let integration =
            ApplicationCredentialsIntegration::new(runtime.clone(), config_entries.clone())
                .expect("application credentials integration");
        let platform = integration
            .platform_registry()
            .register("demo", demo_platform())
            .expect("loaded demo platform");

        Self {
            _directory: directory,
            runtime,
            config_entries,
            tokens,
            integration,
            _platform: platform,
        }
    }

    fn router(&self, shutdown: Option<watch::Receiver<bool>>) -> Router {
        router_with_features(
            ApiState {
                runtime: self.runtime.clone(),
                access_token: String::new(),
                config: json!({}),
            },
            ApiFeatures {
                auth: Some(self.tokens.auth.clone()),
                config_entries: Some(self.config_entries.clone()),
                application_credentials: Some(self.integration.manager()),
                ..ApiFeatures::default()
            },
            shutdown,
        )
    }

    fn add_config_entry(&self, auth_implementation: &str) -> String {
        if self.config_entries.driver("demo").is_none() {
            self.config_entries
                .register_driver(Arc::new(FixtureConfigEntryDriver))
                .expect("config-entry fixture driver");
        }
        let mut data = Map::new();
        data.insert("auth_implementation".to_owned(), json!(auth_implementation));
        self.config_entries
            .create_entry(NewConfigEntry::new("demo", "Credential fixture", data))
            .expect("config entry")
            .entry_id
    }
}

fn auth_tokens(runtime: &Runtime) -> Tokens {
    let auth = AuthManager::new(runtime.clone()).expect("create auth manager");
    let owner = auth
        .bootstrap_owner_with_username(
            "owner",
            "Application Credentials Owner",
            "app-credentials-owner-password",
        )
        .expect("create owner");
    let administrator = auth
        .create_user_with_username(
            &owner.id,
            "credential_admin",
            "Credential Administrator",
            "app-credentials-admin-password",
            true,
        )
        .expect("create secondary administrator");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "Ordinary User",
            "app-credentials-user-password",
            false,
        )
        .expect("create ordinary user");
    let reader = auth
        .create_user_without_credentials(
            &owner.id,
            "Read-only User",
            Some(&["system-read-only".to_owned()]),
            None,
        )
        .expect("create read-only user");
    Tokens {
        owner_id: owner.id.clone(),
        admin_id: administrator.id.clone(),
        owner: auth
            .issue_tokens(&owner.id, CLIENT_ID, None)
            .expect("issue owner token")
            .access_token,
        administrator: auth
            .issue_tokens(&administrator.id, CLIENT_ID, None)
            .expect("issue administrator token")
            .access_token,
        ordinary: auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .expect("issue ordinary token")
            .access_token,
        reader: auth
            .issue_tokens(&reader.id, CLIENT_ID, None)
            .expect("issue read-only token")
            .access_token,
        auth,
    }
}

fn demo_platform() -> ApplicationCredentialsPlatform {
    ApplicationCredentialsPlatform::new()
        .with_authorization_server_factory(|_| {
            Box::pin(async {
                Ok(AuthorizationServer {
                    authorize_url: "https://auth.example/authorize".to_owned(),
                    token_url: "https://auth.example/token".to_owned(),
                })
            })
        })
        .with_description_placeholders(|_| {
            Box::pin(async {
                Ok(BTreeMap::from([(
                    "docs_url".to_owned(),
                    "https://docs.example/demo".to_owned(),
                )]))
            })
        })
}

async fn start_server(router: Router) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback API server");
    let address = listener.local_addr().expect("API server address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    (address, server)
}

async fn receive(socket: &mut Socket) -> Value {
    loop {
        let frame = timeout(Duration::from_secs(4), socket.next())
            .await
            .expect("WebSocket response timed out")
            .expect("WebSocket closed before response")
            .expect("WebSocket receive failed");
        match frame {
            Message::Text(text) => return serde_json::from_str(&text).expect("JSON response"),
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .expect("reply to WebSocket ping"),
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send WebSocket command");
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .expect("connect WebSocket");
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    send(&mut socket, json!({"type": "auth", "access_token": token})).await;
    assert_eq!(receive(&mut socket).await["type"], "auth_ok");
    socket
}

fn assert_success(frame: &Value, id: u64) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], true, "unexpected response: {frame}");
}

fn assert_error(frame: &Value, id: u64, code: &str, message: Option<&str>) {
    assert_eq!(frame["id"], id);
    assert_eq!(frame["type"], "result");
    assert_eq!(frame["success"], false);
    assert_eq!(frame["error"]["code"], code);
    if let Some(message) = message {
        assert_eq!(frame["error"]["message"], message);
    }
}

#[tokio::test]
async fn schema_validation_precedes_admin_checks_and_invalid_unsubscribe_keeps_stream() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut ordinary = connect(address, &fixture.tokens.ordinary).await;
    for (offset, malformed) in [
        json!({"client_id": 42}),
        json!({"client_id": "client", "name": null}),
        json!({"client_id": "client", "unexpected": true}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = offset as u64 + 1;
        let mut command = json!({"id": id, "type": "application_credentials/create", "domain": "demo", "client_secret": "fixture-secret"});
        command
            .as_object_mut()
            .unwrap()
            .extend(malformed.as_object().unwrap().clone());
        send(&mut ordinary, command).await;
        assert_error(&receive(&mut ordinary).await, id, "invalid_format", None);
    }
    send(&mut ordinary, json!({"id": 4, "type": "application_credentials/create", "domain": "demo", "client_id": "client", "client_secret": "fixture-secret"})).await;
    assert_error(&receive(&mut ordinary).await, 4, "unauthorized", None);

    let mut owner = connect(address, &fixture.tokens.owner).await;
    send(
        &mut owner,
        json!({"id": 1, "type": "application_credentials/subscribe"}),
    )
    .await;
    assert_success(&receive(&mut owner).await, 1);
    assert_eq!(receive(&mut owner).await["event"], json!([]));
    send(
        &mut owner,
        json!({"id": 2, "type": "unsubscribe_events", "subscription": 1, "unexpected": true}),
    )
    .await;
    assert_error(&receive(&mut owner).await, 2, "invalid_format", None);
    let record = fixture
        .integration
        .manager()
        .create_admin(CreateCredential {
            domain: "demo".to_owned(),
            client_id: "still-subscribed".to_owned(),
            client_secret: "fixture-secret".to_owned(),
            auth_domain: None,
            name: None,
        })
        .unwrap();
    let event = receive(&mut owner).await;
    assert_eq!(event["id"], 1);
    assert_eq!(event["event"][0]["item"]["id"], record.id);
    send(
        &mut owner,
        json!({"id": 3, "type": "unsubscribe_events", "subscription": 1}),
    )
    .await;
    assert_success(&receive(&mut owner).await, 3);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn revoked_admin_session_closes_a_quiet_credential_subscription() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut socket = connect(address, &fixture.tokens.administrator).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "application_credentials/subscribe"}),
    )
    .await;
    assert_success(&receive(&mut socket).await, 1);
    assert_eq!(receive(&mut socket).await["event"], json!([]));
    fixture
        .tokens
        .auth
        .revoke_user_sessions(&fixture.tokens.owner_id, &fixture.tokens.admin_id)
        .unwrap();
    assert_connection_closed_without_frame(&mut socket).await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn owner_shutdown_emits_one_terminal_error_and_keeps_the_socket_usable() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut socket = connect(address, &fixture.tokens.owner).await;
    send(
        &mut socket,
        json!({"id": 1, "type": "application_credentials/subscribe"}),
    )
    .await;
    assert_success(&receive(&mut socket).await, 1);
    assert_eq!(receive(&mut socket).await["event"], json!([]));
    fixture.integration.shutdown().await;
    assert_error(
        &receive(&mut socket).await,
        1,
        "application_credentials_stream_error",
        None,
    );
    send(&mut socket, json!({"id": 2, "type": "ping"})).await;
    assert_eq!(receive(&mut socket).await, json!({"id": 2, "type": "pong"}));
    assert!(
        timeout(Duration::from_millis(200), socket.next())
            .await
            .is_err()
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn websocket_admin_boundary_and_public_catalog_match_source_contract() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut owner = connect(address, &fixture.tokens.owner).await;
    send(
        &mut owner,
        json!({
            "id": 1,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "\u{1c} Client.ID \u{1f}",
            "client_secret": "\u{1d} private-client-secret \u{1e}",
            "name": " Fixture client ",
        }),
    )
    .await;
    let created = receive(&mut owner).await;
    assert_success(&created, 1);
    assert_eq!(created["result"]["id"], "demo_client_id");
    assert_eq!(created["result"]["client_id"], "Client.ID");
    assert_eq!(created["result"]["client_secret"], "private-client-secret");
    assert_eq!(created["result"]["name"], " Fixture client ");

    send(
        &mut owner,
        json!({ "id": 2, "type": "application_credentials/list" }),
    )
    .await;
    let listed = receive(&mut owner).await;
    assert_success(&listed, 2);
    assert_eq!(listed["result"], json!([created["result"]]));
    assert_eq!(
        listed["result"][0]["client_secret"],
        "private-client-secret"
    );

    send(
        &mut owner,
        json!({
            "id": 3,
            "type": "application_credentials/update",
            "application_credentials_id": created["result"]["id"],
        }),
    )
    .await;
    assert_error(
        &receive(&mut owner).await,
        3,
        "invalid_format",
        Some("Updates not supported"),
    );

    send(
        &mut owner,
        json!({
            "id": 4,
            "type": "application_credentials/create",
            "domain": "not_loaded",
            "client_id": "client",
            "client_secret": "secret",
        }),
    )
    .await;
    assert_error(
        &receive(&mut owner).await,
        4,
        "invalid_format",
        Some("No application_credentials platform for not_loaded"),
    );

    send(
        &mut owner,
        json!({
            "id": 5,
            "type": "application_credentials/delete",
            "application_credentials_id": "missing",
        }),
    )
    .await;
    assert_error(
        &receive(&mut owner).await,
        5,
        "not_found",
        Some("Unable to find application_credentials_id missing"),
    );

    for (token, label) in [
        (&fixture.tokens.ordinary, "ordinary"),
        (&fixture.tokens.reader, "read-only"),
    ] {
        let mut socket = connect(address, token).await;
        let denied_commands = [
            json!({ "type": "application_credentials/list" }),
            json!({ "type": "application_credentials/subscribe" }),
            json!({
                "type": "application_credentials/create",
                "domain": "demo",
                "client_id": "denied",
                "client_secret": "denied-secret",
            }),
            json!({
                "type": "application_credentials/update",
                "application_credentials_id": "demo_client_id",
            }),
            json!({
                "type": "application_credentials/delete",
                "application_credentials_id": "demo_client_id",
            }),
            json!({
                "type": "application_credentials/config_entry",
                "config_entry_id": "missing-entry",
            }),
        ];
        for (offset, mut command) in denied_commands.into_iter().enumerate() {
            let id = 10 + offset as u64;
            command["id"] = json!(id);
            send(&mut socket, command).await;
            let denied = receive(&mut socket).await;
            assert_eq!(denied["id"], id, "{label} user");
            assert_eq!(denied["success"], false, "{label} user");
            assert_eq!(denied["error"]["code"], "unauthorized", "{label} user");
        }

        send(
            &mut socket,
            json!({ "id": 20, "type": "application_credentials/config" }),
        )
        .await;
        let catalog = receive(&mut socket).await;
        assert_success(&catalog, 20);
        assert_eq!(catalog["result"]["domains"], json!(["demo"]));
        assert_eq!(
            catalog["result"]["integrations"]["demo"]["description_placeholders"]["docs_url"],
            "https://docs.example/demo"
        );
        assert!(!catalog.to_string().contains("private-client-secret"));
        assert!(!catalog.to_string().contains("denied-secret"));
    }

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn config_entry_returns_stable_id_and_delete_guard_checks_compatibility_alias() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut owner = connect(address, &fixture.tokens.owner).await;
    send(
        &mut owner,
        json!({
            "id": 1,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "client",
            "client_secret": "entry-secret",
            "auth_domain": "legacy-auth",
        }),
    )
    .await;
    let created = receive(&mut owner).await;
    assert_success(&created, 1);
    let entry_id = fixture.add_config_entry("legacy-auth");

    send(
        &mut owner,
        json!({
            "id": 2,
            "type": "application_credentials/config_entry",
            "config_entry_id": entry_id,
        }),
    )
    .await;
    let mapping = receive(&mut owner).await;
    assert_success(&mapping, 2);
    assert_eq!(
        mapping["result"],
        json!({ "application_credentials_id": created["result"]["id"] })
    );
    assert!(!mapping.to_string().contains("entry-secret"));

    let unrelated_entry = fixture.add_config_entry("other-auth");
    send(
        &mut owner,
        json!({
            "id": 3,
            "type": "application_credentials/config_entry",
            "config_entry_id": unrelated_entry,
        }),
    )
    .await;
    let unmatched = receive(&mut owner).await;
    assert_success(&unmatched, 3);
    assert_eq!(unmatched["result"], json!({}));

    send(
        &mut owner,
        json!({
            "id": 4,
            "type": "application_credentials/config_entry",
            "config_entry_id": "missing-entry",
        }),
    )
    .await;
    assert_error(
        &receive(&mut owner).await,
        4,
        "invalid_config_entry_id",
        Some("Config entry not found: missing-entry"),
    );

    send(
        &mut owner,
        json!({
            "id": 5,
            "type": "application_credentials/delete",
            "application_credentials_id": created["result"]["id"],
        }),
    )
    .await;
    assert_error(
        &receive(&mut owner).await,
        5,
        "home_assistant_error",
        Some("Cannot delete credential in use by integration demo"),
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn subscription_ack_precedes_snapshot_and_unsubscribe_stops_live_changes() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut subscriber = connect(address, &fixture.tokens.owner).await;
    send(
        &mut subscriber,
        json!({
            "id": 1,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "initial",
            "client_secret": "initial-secret",
        }),
    )
    .await;
    let initial_record = receive(&mut subscriber).await;
    assert_success(&initial_record, 1);

    send(
        &mut subscriber,
        json!({ "id": 2, "type": "application_credentials/subscribe" }),
    )
    .await;
    let ack = receive(&mut subscriber).await;
    assert_eq!(ack["id"], 2);
    assert_eq!(ack["type"], "result");
    assert_eq!(ack["success"], true);
    assert_eq!(ack["result"], Value::Null);

    let snapshot = receive(&mut subscriber).await;
    assert_eq!(snapshot["id"], 2);
    assert_eq!(snapshot["type"], "event");
    assert_eq!(
        snapshot["event"],
        json!([{
            "change_type": "added",
            "application_credentials_id": initial_record["result"]["id"],
            "item": initial_record["result"],
        }])
    );
    assert_eq!(
        snapshot["event"][0]["item"]["client_secret"],
        "initial-secret"
    );

    // A second authenticated owner creates while the first socket receives
    // events, avoiding a command-result/event ordering race on one connection.
    let mut writer = connect(address, &fixture.tokens.owner).await;
    send(
        &mut writer,
        json!({
            "id": 10,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "live",
            "client_secret": "live-secret",
        }),
    )
    .await;
    let live_record = receive(&mut writer).await;
    assert_success(&live_record, 10);
    let added = receive(&mut subscriber).await;
    assert_eq!(added["id"], 2);
    assert_eq!(added["type"], "event");
    assert_eq!(
        added["event"],
        json!([{
            "change_type": "added",
            "application_credentials_id": live_record["result"]["id"],
            "item": live_record["result"],
        }])
    );

    send(&mut writer, json!({"id": 11, "type": "application_credentials/delete", "application_credentials_id": live_record["result"]["id"]})).await;
    assert_success(&receive(&mut writer).await, 11);
    let removed = receive(&mut subscriber).await;
    assert_eq!(removed["id"], 2);
    assert_eq!(
        removed["event"],
        json!([{"change_type": "removed", "application_credentials_id": live_record["result"]["id"], "item": live_record["result"]}])
    );
    send(
        &mut subscriber,
        json!({"id": 3, "type": "unsubscribe_events", "subscription": 2}),
    )
    .await;
    assert_success(&receive(&mut subscriber).await, 3);

    send(
        &mut writer,
        json!({
            "id": 12,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "after-unsubscribe",
            "client_secret": "third-secret",
        }),
    )
    .await;
    assert_success(&receive(&mut writer).await, 12);
    assert!(
        timeout(Duration::from_millis(200), subscriber.next())
            .await
            .is_err()
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn subscription_limit_is_shared_with_native_subscriptions_and_releases_slots() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut socket = connect(address, &fixture.tokens.owner).await;

    send(&mut socket, json!({ "id": 1, "type": "subscribe_events" })).await;
    let event_ack = receive(&mut socket).await;
    assert_success(&event_ack, 1);
    assert_eq!(event_ack["result"], Value::Null);

    for id in 2..=32 {
        send(
            &mut socket,
            json!({ "id": id, "type": "application_credentials/subscribe" }),
        )
        .await;
        let ack = receive(&mut socket).await;
        assert_success(&ack, id);
        assert_eq!(ack["result"], Value::Null);
        let snapshot = receive(&mut socket).await;
        assert_eq!(snapshot["id"], id);
        assert_eq!(snapshot["type"], "event");
        assert_eq!(snapshot["event"], json!([]));
    }

    send(
        &mut socket,
        json!({ "id": 33, "type": "application_credentials/subscribe" }),
    )
    .await;
    assert_error(
        &receive(&mut socket).await,
        33,
        "subscription_limit",
        Some("Subscription limit reached"),
    );

    send(
        &mut socket,
        json!({ "id": 34, "type": "unsubscribe_events", "subscription": 1 }),
    )
    .await;
    assert_success(&receive(&mut socket).await, 34);

    send(
        &mut socket,
        json!({ "id": 35, "type": "application_credentials/subscribe" }),
    )
    .await;
    let released_ack = receive(&mut socket).await;
    assert_success(&released_ack, 35);
    assert_eq!(released_ack["result"], Value::Null);
    let released_snapshot = receive(&mut socket).await;
    assert_eq!(released_snapshot["id"], 35);
    assert_eq!(released_snapshot["event"], json!([]));

    for (offset, subscription_id) in (2..=32).chain([35]).enumerate() {
        let command_id = 36 + offset as u64;
        send(
            &mut socket,
            json!({
                "id": command_id,
                "type": "unsubscribe_events",
                "subscription": subscription_id,
            }),
        )
        .await;
        assert_success(&receive(&mut socket).await, command_id);
    }

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn admin_subscription_closes_before_a_secret_event_after_role_demotion() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut socket = connect(address, &fixture.tokens.administrator).await;
    send(
        &mut socket,
        json!({
            "id": 1,
            "type": "application_credentials/create",
            "domain": "demo",
            "client_id": "before-demotion",
            "client_secret": "initial-admin-secret",
        }),
    )
    .await;
    let created = receive(&mut socket).await;
    assert_success(&created, 1);

    send(
        &mut socket,
        json!({ "id": 2, "type": "application_credentials/subscribe" }),
    )
    .await;
    assert_success(&receive(&mut socket).await, 2);
    let snapshot = receive(&mut socket).await;
    assert_eq!(snapshot["id"], 2);
    assert_eq!(
        snapshot["event"][0]["item"]["client_secret"],
        "initial-admin-secret"
    );

    fixture
        .tokens
        .auth
        .update_user(
            &fixture.tokens.owner_id,
            &fixture.tokens.admin_id,
            None,
            None,
            Some(&[]),
            None,
        )
        .expect("remove administrator group");
    fixture
        .integration
        .manager()
        .create_admin(CreateCredential {
            domain: "demo".to_owned(),
            client_id: "after-demotion".to_owned(),
            client_secret: "must-not-be-sent".to_owned(),
            auth_domain: None,
            name: None,
        })
        .expect("publish a credential change after demotion");

    assert_connection_closed_without_frame(&mut socket).await;

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn bounded_subscription_overflow_is_terminal_and_releases_the_subscription_id() {
    let fixture = Fixture::new();
    let (address, server) = start_server(fixture.router(None)).await;
    let mut socket = connect(address, &fixture.tokens.owner).await;
    send(
        &mut socket,
        json!({ "id": 1, "type": "application_credentials/subscribe" }),
    )
    .await;
    assert_success(&receive(&mut socket).await, 1);
    assert_eq!(receive(&mut socket).await["event"], json!([]));

    let manager = fixture.integration.manager();
    for index in 0..(SUBSCRIPTION_CAPACITY * 2 + 1) {
        manager
            .create_admin(CreateCredential {
                domain: "demo".to_owned(),
                client_id: format!("overflow-{index}"),
                client_secret: format!("overflow-secret-{index}"),
                auth_domain: None,
                name: None,
            })
            .expect("publish bounded credential event");
        if index + 1 == SUBSCRIPTION_CAPACITY {
            // Let the adapter forward one full batch into its bounded queue
            // while the WebSocket consumer remains between poll ticks.
            tokio::task::yield_now().await;
        }
    }

    let terminal = timeout(Duration::from_secs(5), async {
        loop {
            let frame = receive(&mut socket).await;
            if frame["error"]["code"] == "application_credentials_stream_error" {
                break frame;
            }
        }
    })
    .await
    .expect("terminal bounded-stream error");
    assert_error(&terminal, 1, "application_credentials_stream_error", None);
    assert!(
        terminal["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fell behind")
    );

    // The failed stream is removed, so its command ID is no longer consuming
    // a per-socket slot and another subscription can be established.
    send(
        &mut socket,
        json!({ "id": 2, "type": "application_credentials/subscribe" }),
    )
    .await;
    assert_success(&receive(&mut socket).await, 2);
    let snapshot = receive(&mut socket).await;
    assert_eq!(snapshot["id"], 2);
    assert_eq!(
        snapshot["event"].as_array().unwrap().len(),
        SUBSCRIPTION_CAPACITY * 2 + 1
    );

    server.abort();
    let _ = server.await;
}

struct FixtureConfigEntryDriver;

async fn assert_connection_closed_without_frame(socket: &mut Socket) {
    timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(Message::Ping(payload))) => socket
                    .send(Message::Pong(payload))
                    .await
                    .expect("reply to WebSocket ping"),
                Some(Ok(frame)) => panic!("unexpected frame after demotion: {frame:?}"),
            }
        }
    })
    .await
    .expect("WebSocket was not closed after administrator demotion");
}

impl ConfigEntryDriver for FixtureConfigEntryDriver {
    fn domain(&self) -> &str {
        "demo"
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
        Ok(())
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        Err(hass_core::Error::InvalidInput(
            "fixture flow is unused".to_owned(),
        ))
    }
}
