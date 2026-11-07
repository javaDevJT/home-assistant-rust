//! Real HTTP/WebSocket contract coverage for the native Repairs adapters.

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, network::NetworkPolicy, router_with_features};
use hass_auth::AuthManager;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    NewConfigEntry, SetupOutcome,
};
use hass_core::{Error, MemoryStore, Result, Runtime};
use hass_repairs::{
    FixFlowRequest, IssueDraft, NextFlow, NextFlowType, RepairsFlow, RepairsFlowContext,
    RepairsFlowStep, RepairsIntegration, RepairsPlatform, RepairsResult,
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "http://localhost:3000/";
const HOME_ASSISTANT_VERSION: &str = "2026.12.0.dev0";

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn start_server(router: axum::Router) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
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
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}

async fn command(socket: &mut Socket, value: Value) -> Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    receive(socket).await
}

async fn connect(address: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["type"], "auth_required");
    assert_eq!(
        command(&mut socket, json!({"type":"auth","access_token":token}),).await["type"],
        "auth_ok"
    );
    socket
}

fn persistent_issue(domain: &str, issue_id: &str) -> IssueDraft {
    let mut issue = IssueDraft::new(domain, issue_id, true, "error", "repair_me");
    issue.is_persistent = true;
    issue.breaks_in_ha_version = Some("2026.12.0".into());
    issue.issue_domain = Some("fixture_component".into());
    issue.learn_more_url = Some("https://example.invalid/repair".into());
    issue.translation_placeholders =
        Some(Map::from_iter([("device_name".into(), json!("Boiler"))]));
    issue.data = Some(Map::from_iter([(
        "detail".into(),
        json!("user-visible detail"),
    )]));
    issue
}

fn auth_fixture(runtime: &Runtime) -> (AuthManager, String, String, String) {
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Repairs Owner", "repairs-owner-password")
        .unwrap();
    let user = auth
        .create_user_with_username(
            &owner.id,
            "user",
            "Repairs User",
            "repairs-user-password",
            false,
        )
        .unwrap();
    let readonly_groups = vec!["system-read-only".to_owned()];
    let reader = auth
        .create_user_without_credentials(
            &owner.id,
            "Read only Repairs user",
            Some(&readonly_groups),
            None,
        )
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let user_token = auth
        .issue_tokens(&user.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let reader_token = auth
        .issue_tokens(&reader.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    (auth, owner_token, user_token, reader_token)
}

fn app_state(runtime: Runtime) -> ApiState {
    ApiState {
        runtime,
        access_token: String::new(),
        config: json!({"version": hass_core::VERSION}),
    }
}

#[tokio::test]
async fn repairs_websocket_commands_are_authenticated_and_events_are_minimal() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let (auth, owner_token, _user_token, reader_token) = auth_fixture(&runtime);
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let hidden_entry = entries
        .create_entry(NewConfigEntry::new("fixture", "Disabled entry", Map::new()))
        .unwrap();
    entries
        .disable_entry(&hidden_entry.entry_id, Some("user"))
        .unwrap();

    let integration =
        RepairsIntegration::start(runtime.clone(), entries.clone(), HOME_ASSISTANT_VERSION)
            .unwrap();
    let repairs = integration.manager();
    repairs
        .create_issue(persistent_issue("fixture", "visible"))
        .unwrap();
    let mut hidden = persistent_issue("fixture", "disabled-entry");
    hidden.config_entry_id = Some(hidden_entry.entry_id.clone());
    repairs.create_issue(hidden).unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = router_with_features(
        app_state(runtime.clone()),
        ApiFeatures {
            auth: Some(auth.clone()),
            config_entries: Some(entries.clone()),
            repairs: Some(repairs.clone()),
            network: NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
            ..ApiFeatures::default()
        },
        Some(shutdown_rx),
    );
    let (address, server) = start_server(app).await;
    let mut reader = connect(address, &reader_token).await;

    let listed = command(&mut reader, json!({"id":1,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["success"], true);
    let issues = listed["result"]["issues"].as_array().unwrap();
    assert_eq!(
        issues.len(),
        1,
        "issues linked to disabled entries are hidden"
    );
    let summary = &issues[0];
    assert_eq!(summary["issue_id"], "visible");
    assert_eq!(summary["severity"], "error");
    assert_eq!(summary["translation_key"], "repair_me");
    assert_eq!(summary["translation_placeholders"]["device_name"], "Boiler");
    assert_eq!(summary["breaks_in_ha_version"], "2026.12.0");
    assert!(summary["created"].as_str().is_some());
    assert!(summary.get("data").is_none());
    assert!(summary.get("config_entry_id").is_none());

    let data = command(
        &mut reader,
        json!({"id":2,"type":"repairs/get_issue_data","domain":"fixture","issue_id":"visible"}),
    )
    .await;
    assert_eq!(data["success"], true);
    assert_eq!(
        data["result"]["issue_data"]["detail"],
        "user-visible detail"
    );

    let ignored = command(
        &mut reader,
        json!({"id":3,"type":"repairs/ignore_issue","domain":"fixture","issue_id":"visible","ignore":true}),
    )
    .await;
    assert_eq!(ignored["success"], true);
    assert_eq!(ignored["result"], Value::Null);
    let listed = command(&mut reader, json!({"id":4,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["result"]["issues"][0]["ignored"], true);
    assert_eq!(
        listed["result"]["issues"][0]["dismissed_version"],
        HOME_ASSISTANT_VERSION
    );

    let unignored = command(
        &mut reader,
        json!({"id":5,"type":"repairs/ignore_issue","domain":"fixture","issue_id":"visible","ignore":false}),
    )
    .await;
    assert_eq!(unignored["success"], true);
    let listed = command(&mut reader, json!({"id":6,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["result"]["issues"][0]["ignored"], false);
    assert_eq!(
        listed["result"]["issues"][0]["dismissed_version"],
        Value::Null
    );

    let reignored = command(
        &mut reader,
        json!({"id":7,"type":"repairs/ignore_issue","domain":"fixture","issue_id":"visible","ignore":true}),
    )
    .await;
    assert_eq!(reignored["success"], true);
    let listed = command(&mut reader, json!({"id":8,"type":"repairs/list_issues"})).await;
    assert_eq!(listed["result"]["issues"][0]["ignored"], true);

    let missing = command(
        &mut reader,
        json!({"id":9,"type":"repairs/get_issue_data","domain":"fixture","issue_id":"missing"}),
    )
    .await;
    assert_eq!(missing["error"]["code"], "unknown_issue");
    assert_eq!(missing["error"]["message"], "Issue 'missing' not found");
    let invalid = command(
        &mut reader,
        json!({"id":10,"type":"repairs/ignore_issue","domain":"fixture","issue_id":"visible","ignore":"true"}),
    )
    .await;
    assert_eq!(invalid["error"]["code"], "invalid_format");

    let subscribed = command(
        &mut reader,
        json!({"id":11,"type":"subscribe_events","event_type":"repairs_issue_registry_updated"}),
    )
    .await;
    assert_eq!(subscribed["success"], true);
    repairs
        .create_issue(persistent_issue("fixture", "fresh"))
        .unwrap();
    let event = receive(&mut reader).await;
    assert_eq!(event["id"], 11);
    assert_eq!(event["type"], "event");
    assert_eq!(
        event["event"]["event_type"],
        "repairs_issue_registry_updated"
    );
    assert_eq!(event["event"]["data"]["action"], "create");
    assert_eq!(event["event"]["data"]["domain"], "fixture");
    assert_eq!(event["event"]["data"]["issue_id"], "fresh");
    assert_eq!(
        event["event"]["data"].as_object().unwrap().len(),
        3,
        "issue data must not be broadcast"
    );

    let stopped = command(
        &mut reader,
        json!({"id":12,"type":"unsubscribe_events","subscription":11}),
    )
    .await;
    assert_eq!(stopped["success"], true);

    shutdown_tx.send(true).unwrap();
    let closed = timeout(Duration::from_secs(2), reader.next())
        .await
        .expect("API shutdown did not close the authenticated Repairs socket")
        .expect("Repairs socket ended without a close frame")
        .expect("Repairs socket close failed");
    assert!(matches!(closed, Message::Close(_)));
    server.abort();
    let _ = server.await;

    drop(integration);
    assert!(
        repairs.list_issues().is_err(),
        "uninstalled weak manager must close"
    );

    let app = router_with_features(
        app_state(runtime.clone()),
        ApiFeatures {
            auth: Some(auth),
            repairs: Some(repairs.clone()),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let unavailable = Client::new()
        .get(format!("http://{address}/api/repairs/issues/fix/flow"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::INTERNAL_SERVER_ERROR);
    server.abort();
    let _ = server.await;

    let restarted = RepairsIntegration::start(runtime, entries, HOME_ASSISTANT_VERSION).unwrap();
    let restored = restarted.manager().list_issues().unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].issue_id, "visible");
    assert!(restored[0].ignored);
    assert_eq!(restored[1].issue_id, "fresh");
}

#[derive(Default)]
struct FixtureConfigDriver;

impl ConfigEntryDriver for FixtureConfigDriver {
    fn domain(&self) -> &str {
        "fixture"
    }

    fn supports_options(&self) -> bool {
        true
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
        Ok(())
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        Err(Error::InvalidInput(
            "config flow is not used by this fixture".into(),
        ))
    }

    fn options_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        if request.user_input.is_none() {
            return Ok(ConfigFlowResult::Form {
                step_id: "options".into(),
                data_schema: Some(json!([{"name":"enabled","type":"boolean"}])),
                errors: None,
                description_placeholders: Some(Map::from_iter([(
                    "account".into(),
                    json!("Public account"),
                )])),
            });
        }
        let options = request.user_input.clone().unwrap_or_default();
        Ok(ConfigFlowResult::CreateEntry {
            title: "Fixture options".into(),
            data: Map::new(),
            options,
            unique_id: request.unique_id.clone(),
            version: 1,
            minor_version: 1,
        })
    }
}

#[derive(Clone)]
struct FixtureRepairPlatform {
    options_flow_id: String,
    requests: Arc<Mutex<Vec<FixFlowRequest>>>,
    contexts: Arc<Mutex<Vec<RepairsFlowContext>>>,
}

impl RepairsPlatform for FixtureRepairPlatform {
    fn create_fix_flow(&self, request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
        self.requests.lock().unwrap().push(request);
        Ok(Box::new(FixtureRepairFlow {
            options_flow_id: self.options_flow_id.clone(),
            contexts: self.contexts.clone(),
        }))
    }
}

struct FixtureRepairFlow {
    options_flow_id: String,
    contexts: Arc<Mutex<Vec<RepairsFlowContext>>>,
}

impl RepairsFlow for FixtureRepairFlow {
    fn step(
        &mut self,
        context: &RepairsFlowContext,
        user_input: Option<Value>,
    ) -> RepairsResult<RepairsFlowStep> {
        self.contexts.lock().unwrap().push(context.clone());
        if user_input.is_none() {
            return Ok(RepairsFlowStep::Form {
                step_id: "confirm".into(),
                data_schema: Some(json!([{"name":"begin_options","type":"boolean"}])),
                errors: None,
                description_placeholders: Some(Map::from_iter([(
                    "device_name".into(),
                    json!("Boiler"),
                )])),
            });
        }
        if user_input
            .as_ref()
            .and_then(|input| input.get("begin_options"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            Ok(RepairsFlowStep::CreateEntry {
                title: Some("Repair complete".into()),
                data: Map::from_iter([("fixed".into(), json!(true))]),
                description: None,
                description_placeholders: None,
                next_flow: Some(NextFlow::new(
                    NextFlowType::OptionsFlow,
                    self.options_flow_id.clone(),
                )),
            })
        } else {
            Ok(RepairsFlowStep::Abort {
                reason: "dismissed".into(),
                description_placeholders: None,
                translation_domain: None,
                next_flow: None,
            })
        }
    }
}

#[tokio::test]
async fn repairs_http_flow_routes_enforce_policy_and_return_terminal_next_flow_metadata() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let (auth, owner_token, user_token, reader_token) = auth_fixture(&runtime);
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    entries
        .register_driver(Arc::new(FixtureConfigDriver))
        .unwrap();
    let mut seed = NewConfigEntry::new(
        "fixture",
        "Fixture account",
        Map::from_iter([("token".into(), json!("ENTRY_PRIVATE_SENTINEL"))]),
    );
    seed.options = Map::from_iter([("password".into(), json!("OPTION_PRIVATE_SENTINEL"))]);
    let entry = entries.create_entry(seed).unwrap();
    let options_form = entries.init_options_flow(&entry.entry_id).unwrap();
    let options_flow_id = options_form["flow_id"].as_str().unwrap().to_owned();

    let integration =
        RepairsIntegration::start(runtime.clone(), entries.clone(), HOME_ASSISTANT_VERSION)
            .unwrap();
    let repairs = integration.manager();
    let mut issue = persistent_issue("fixture", "flow-issue");
    issue.config_entry_id = Some(entry.entry_id.clone());
    issue.data = Some(Map::from_iter([
        ("detail".into(), json!("user-visible detail")),
        // Legacy handler data may carry the same key. The source of truth for
        // flow context remains the request issue_id, which overrides it.
        ("issue_id".into(), json!("legacy-data-issue")),
    ]));
    repairs.create_issue(issue).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    repairs
        .register_platform(
            "fixture",
            Arc::new(FixtureRepairPlatform {
                options_flow_id: options_flow_id.clone(),
                requests: requests.clone(),
                contexts: contexts.clone(),
            }),
        )
        .unwrap();

    let app = router_with_features(
        app_state(runtime),
        ApiFeatures {
            auth: Some(auth),
            config_entries: Some(entries.clone()),
            repairs: Some(repairs.clone()),
            network: NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, server) = start_server(app).await;
    let client = Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let base = format!("http://{address}/api/repairs/issues/fix");

    let forbidden_start = client
        .post(&base)
        .bearer_auth(&user_token)
        .json(&json!({"handler":"fixture","issue_id":"flow-issue"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_start.status(), StatusCode::UNAUTHORIZED);

    let started = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({
            "handler":"fixture",
            "issue_id":"flow-issue",
            "ignored_extra":"source permits extra fields"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let started: Value = started.json().await.unwrap();
    assert_eq!(started["handler"], "fixture");
    assert_eq!(started["type"], "form");
    assert_eq!(started["step_id"], "confirm");
    assert_eq!(started["description_placeholders"]["device_name"], "Boiler");
    assert_eq!(started["context"]["issue_id"], "flow-issue");
    assert!(started["data_schema"].as_array().is_some());
    assert!(!started.to_string().contains("user-visible detail"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(requests.lock().unwrap()[0].issue_id, "flow-issue");
    assert_eq!(
        requests.lock().unwrap()[0].issue_data.as_ref().unwrap()["detail"],
        "user-visible detail"
    );
    assert_eq!(
        requests.lock().unwrap()[0].issue_data.as_ref().unwrap()["issue_id"],
        "legacy-data-issue"
    );
    let flow_id = started["flow_id"].as_str().unwrap();
    let context = contexts.lock().unwrap().last().unwrap().clone();
    assert_eq!(context.issue_id, "flow-issue");
    assert_eq!(
        context.metadata,
        Map::from_iter([("issue_id".into(), json!("flow-issue"))])
    );
    assert_eq!(
        context.initial_data,
        Map::from_iter([("issue_id".into(), json!("flow-issue"))]),
        "the HTTP adapter sets canonical context and does not pass accepted extras as flow data"
    );
    assert_eq!(contexts.lock().unwrap()[0].issue_id, "flow-issue");

    let forbidden_get = client
        .get(format!("{base}/{flow_id}"))
        .bearer_auth(&reader_token)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_get.status(), StatusCode::UNAUTHORIZED);
    let forbidden_post = client
        .post(format!("{base}/{flow_id}"))
        .bearer_auth(&user_token)
        .json(&json!({"begin_options":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_post.status(), StatusCode::UNAUTHORIZED);

    let anonymous_abort = client
        .delete(format!("{base}/{flow_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous_abort.status(), StatusCode::UNAUTHORIZED);

    // The pinned resource DELETE has no Repairs-specific admin decorator.
    let aborted = client
        .delete(format!("{base}/{flow_id}"))
        .bearer_auth(&user_token)
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), StatusCode::OK);
    assert_eq!(
        aborted.json::<Value>().await.unwrap()["message"],
        "Flow aborted"
    );
    assert!(
        repairs
            .get_issue("fixture", "flow-issue")
            .unwrap()
            .is_some()
    );

    let readonly_flow = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"fixture","issue_id":"flow-issue"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let readonly_flow_id = readonly_flow["flow_id"].as_str().unwrap();
    let readonly_abort = client
        .delete(format!("{base}/{readonly_flow_id}"))
        .bearer_auth(&reader_token)
        .send()
        .await
        .unwrap();
    assert_eq!(readonly_abort.status(), StatusCode::OK);

    let bad_body = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":7,"issue_id":"flow-issue"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_body.status(), StatusCode::BAD_REQUEST);
    let missing_issue = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"fixture","issue_id":"missing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_issue.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        missing_issue.json::<Value>().await.unwrap()["message"],
        "issue id missing is not found"
    );
    let mut non_fixable = IssueDraft::new("fixture", "not-fixable", false, "error", "not_fixable");
    non_fixable.is_persistent = true;
    repairs.create_issue(non_fixable).unwrap();
    let non_fixable = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"fixture","issue_id":"not-fixable"}))
        .send()
        .await
        .unwrap();
    assert_eq!(non_fixable.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        non_fixable.json::<Value>().await.unwrap()["message"],
        "issue id not-fixable is not fixable"
    );
    let missing_flow = client
        .get(format!("{base}/missing-flow"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_flow.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing_flow.json::<Value>().await.unwrap()["message"],
        "Invalid flow specified"
    );
    let missing_flow_post = client
        .post(format!("{base}/missing-flow"))
        .bearer_auth(&owner_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_flow_post.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing_flow_post.json::<Value>().await.unwrap()["message"],
        "Invalid flow specified"
    );

    let abort_started = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"fixture","issue_id":"flow-issue"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let abort_flow_id = abort_started["flow_id"].as_str().unwrap();
    let abort_result = client
        .post(format!("{base}/{abort_flow_id}"))
        .bearer_auth(&owner_token)
        .json(&json!({"begin_options":false}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(abort_result["type"], "abort");
    assert_eq!(abort_result["reason"], "dismissed");
    assert_eq!(abort_result["context"]["issue_id"], "flow-issue");
    assert!(
        repairs
            .get_issue("fixture", "flow-issue")
            .unwrap()
            .is_some()
    );
    let aborted_flow_get = client
        .get(format!("{base}/{abort_flow_id}"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(aborted_flow_get.status(), StatusCode::NOT_FOUND);

    let started = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"fixture","issue_id":"flow-issue"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let flow_id = started["flow_id"].as_str().unwrap();
    let current = client
        .get(format!("{base}/{flow_id}"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(current["type"], "form");
    assert_eq!(current["description_placeholders"]["device_name"], "Boiler");

    let completed = client
        .post(format!("{base}/{flow_id}"))
        .bearer_auth(&owner_token)
        .json(&json!({"begin_options":true}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(completed["type"], "create_entry");
    assert_eq!(completed["context"]["issue_id"], "flow-issue");
    assert_eq!(completed["data"]["fixed"], true);
    assert_eq!(
        completed["next_flow"],
        json!(["options_flow", options_flow_id.as_str()])
    );
    assert_eq!(completed["result"]["entry_id"], entry.entry_id);
    assert!(completed["result"].get("data").is_none());
    assert!(completed["result"].get("options").is_none());
    assert!(!completed.to_string().contains("ENTRY_PRIVATE_SENTINEL"));
    assert!(!completed.to_string().contains("OPTION_PRIVATE_SENTINEL"));
    assert!(
        repairs
            .get_issue("fixture", "flow-issue")
            .unwrap()
            .is_none()
    );

    let terminal_post = client
        .post(format!("{base}/{flow_id}"))
        .bearer_auth(&owner_token)
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(terminal_post.status(), StatusCode::NOT_FOUND);
    let terminal_get = client
        .get(format!("{base}/{flow_id}"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(terminal_get.status(), StatusCode::NOT_FOUND);

    let options_url =
        format!("http://{address}/api/config/config_entries/options/flow/{options_flow_id}");
    let child_form = client
        .get(&options_url)
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(child_form.status(), StatusCode::OK);
    let child_form = child_form.json::<Value>().await.unwrap();
    assert_eq!(child_form["type"], "form");
    assert_eq!(child_form["step_id"], "options");
    let child_complete = client
        .post(&options_url)
        .bearer_auth(&owner_token)
        .json(&json!({"enabled":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(child_complete.status(), StatusCode::OK);
    let child_complete = child_complete.json::<Value>().await.unwrap();
    assert_eq!(child_complete["type"], "create_entry");
    assert!(
        !child_complete
            .to_string()
            .contains("ENTRY_PRIVATE_SENTINEL")
    );
    assert!(
        !child_complete
            .to_string()
            .contains("OPTION_PRIVATE_SENTINEL")
    );

    let mut fallback = persistent_issue("no_native_platform", "confirm-issue");
    fallback.translation_placeholders = Some(Map::from_iter([(
        "device_name".into(),
        json!("Fallback device"),
    )]));
    repairs.create_issue(fallback).unwrap();
    let fallback = client
        .post(&base)
        .bearer_auth(&owner_token)
        .json(&json!({"handler":"no_native_platform","issue_id":"confirm-issue"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(fallback["type"], "form");
    assert_eq!(fallback["step_id"], "confirm");
    assert_eq!(
        fallback["description_placeholders"]["device_name"],
        "Fallback device"
    );
    assert_eq!(fallback["data_schema"], json!([]));
    let fallback_id = fallback["flow_id"].as_str().unwrap();
    let fixed = client
        .post(format!("{base}/{fallback_id}"))
        .bearer_auth(&owner_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(fixed["type"], "create_entry");
    assert!(
        repairs
            .get_issue("no_native_platform", "confirm-issue")
            .unwrap()
            .is_none()
    );

    server.abort();
    let _ = server.await;
}
