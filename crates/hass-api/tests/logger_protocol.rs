//! Actual authenticated logger control changes the same tracing output filter.
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Error, MemoryStore, Runtime};
use hass_logger::{IntegrationCatalog, LoggerConfig, LoggerIntegration};
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing_subscriber::prelude::*;

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

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn emit(subscriber: &tracing::Dispatch) {
    tracing::dispatcher::with_default(subscriber, || {
        tracing::debug!(target: "hass_mqtt", "visible fixture");
        tracing::debug!(target: "hass_mqtt", "suppress exact fixture");
        tracing::debug!(target: "hass_mqtt::wire", "suppress child remains visible");
    });
}

#[tokio::test]
async fn logger_commands_enforce_roles_and_change_real_output_and_restart_settings() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Owner", "logger-owner-fixture")
        .unwrap();
    let user = auth
        .create_user_with_username(&owner.id, "user", "User", "logger-user-fixture", false)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, "http://localhost/", None)
        .unwrap();
    let user_token = auth
        .issue_tokens(&user.id, "http://localhost/", None)
        .unwrap();
    let catalog = IntegrationCatalog::from([
        ("mqtt".into(), vec!["hass_mqtt".into()]),
        ("tag".into(), vec!["hass_tag".into()]),
    ]);
    let full_catalog = catalog.clone();
    let known = Arc::new(move || full_catalog.clone());
    let auth_owner = Arc::new(auth.clone());
    let weak_auth = Arc::downgrade(&auth_owner);
    let authorize = Arc::new(move |context: &hass_core::Context| {
        if let Some(id) = context.user_id.as_deref() {
            let user = weak_auth
                .upgrade()
                .ok_or_else(|| Error::Service("Auth stopped".into()))?
                .get_user(id)?;
            if !user.is_active || (!user.is_owner && !user.is_admin) {
                return Err(Error::NotAllowed("Administrator required".into()));
            }
        }
        Ok(())
    });
    let config = LoggerConfig {
        filters: std::collections::BTreeMap::from([(
            "homeassistant.components.mqtt".into(),
            vec!["^suppress".into()],
        )]),
        ..LoggerConfig::default()
    };
    let integration = LoggerIntegration::start(
        runtime.clone(),
        config.clone(),
        known.clone(),
        authorize.clone(),
    )
    .unwrap();
    let manager = integration.manager();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Capture(bytes.clone());
    let output = tracing::Dispatch::new(
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .with_filter(integration.filter()),
        ),
    );
    emit(&output);
    assert!(bytes.lock().unwrap().is_empty());
    let loaded = Arc::new(|| IntegrationCatalog::from([("tag".into(), vec!["hass_tag".into()])]));
    let app = router_with_features(
        ApiState {
            runtime: runtime.clone(),
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth),
            logger: Some(manager.clone()),
            logger_catalog: Some(loaded),
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
    let info = command(&mut ordinary, json!({"id":1,"type":"logger/log_info"})).await;
    assert_eq!(info["result"], json!([{"domain":"tag","level":20}]));
    let denied = command(&mut ordinary,json!({"id":2,"type":"logger/log_level","module":"hass_mqtt","level":"DEBUG","persistence":"permanent"})).await;
    assert_eq!(denied["error"]["code"], "unauthorized");
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("http://{address}/api/services/logger/set_level"))
            .bearer_auth(&user_token.access_token)
            .json(&json!({"hass_mqtt":"DEBUG"}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let mut public = runtime.subscribe();
    assert_eq!(command(&mut admin,json!({"id":1,"type":"logger/log_level","module":"hass_mqtt","level":"DEBUG","persistence":"permanent"})).await["success"],true);
    let changed = public.try_recv().unwrap();
    assert_eq!(changed.event_type, "logging_changed");
    assert!(changed.data.is_empty());
    emit(&output);
    let captured = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(captured.contains("visible fixture"));
    assert!(!captured.contains("suppress exact fixture"));
    assert!(captured.contains("suppress child remains visible"));
    for (id, extra) in [
        (2, json!({"level":"debug","persistence":"permanent"})),
        (3, json!({"level":"INFO","persistence":"Permanent"})),
        (
            4,
            json!({"level":"INFO","persistence":"none","unexpected":true}),
        ),
    ] {
        let mut value = json!({"id":id,"type":"logger/log_level","module":"hass_mqtt"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let invalid = command(&mut admin, value).await;
        assert_eq!(invalid["error"]["code"], "invalid_format");
    }
    assert!(public.try_recv().is_err());
    let missing = command(&mut admin,json!({"id":5,"type":"logger/integration_log_level","integration":"unported_fixture","level":"DEBUG","persistence":"none"})).await;
    assert_eq!(missing["error"]["code"], "not_found");
    assert_eq!(command(&mut admin,json!({"id":6,"type":"logger/integration_log_level","integration":"mqtt","level":"WARNING","persistence":"none"})).await["success"],true);
    let info = command(&mut ordinary, json!({"id":3,"type":"logger/log_info"})).await;
    assert_eq!(
        info["result"],
        json!([{"domain":"mqtt","level":30},{"domain":"tag","level":20}])
    );
    assert_eq!(command(&mut admin,json!({"id":7,"type":"logger/integration_log_level","integration":"mqtt","level":"NOTSET","persistence":"none"})).await["success"],true);
    let info = command(&mut ordinary, json!({"id":4,"type":"logger/log_info"})).await;
    assert_eq!(info["result"], json!([{"domain":"tag","level":20}]));
    let invalid = client
        .post(format!("http://{address}/api/services/logger/set_level"))
        .bearer_auth(&owner_token.access_token)
        .json(&json!({"hass_mqtt":"debug","another":"invalid"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        client
            .post(format!("http://{address}/api/services/logger/set_level"))
            .bearer_auth(&owner_token.access_token)
            .json(&json!({"hass_mqtt":"dEbUg"}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    ordinary.close(None).await.unwrap();
    admin.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    integration.shutdown().unwrap();
    let restarted = LoggerIntegration::start(runtime, config, known, authorize).unwrap();
    assert_eq!(
        restarted
            .manager()
            .effective_level("hass_mqtt")
            .unwrap()
            .number(),
        10
    );
    restarted.shutdown().unwrap();
}
