use hass_api::{ApiFeatures, ApiState, network::NetworkPolicy, router_with_features};
use hass_auth::AuthManager;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    SetupOutcome,
};
use hass_core::{MemoryStore, Result, Runtime};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, ThreadId},
    time::Duration,
};

const CLIENT_ID: &str = "http://localhost:3000/";

#[derive(Default)]
struct FixtureDriver {
    setup_calls: AtomicUsize,
    unload_calls: AtomicUsize,
    last_callback_thread: Mutex<Option<ThreadId>>,
}

impl FixtureDriver {
    fn mark_callback(&self) {
        *self.last_callback_thread.lock().unwrap() = Some(thread::current().id());
    }

    fn callback_thread(&self) -> Option<ThreadId> {
        *self.last_callback_thread.lock().unwrap()
    }
}

impl ConfigEntryDriver for FixtureDriver {
    fn domain(&self) -> &str {
        "fixture_http"
    }

    fn supports_options(&self) -> bool {
        true
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
        self.mark_callback();
        self.setup_calls.fetch_add(1, Ordering::SeqCst);
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
        self.mark_callback();
        self.unload_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        self.mark_callback();
        let Some(input) = request.user_input.as_ref() else {
            return Ok(ConfigFlowResult::form(
                "user",
                Some(json!([{"name":"token","type":"string"}])),
            ));
        };
        let mut data = Map::new();
        data.insert(
            "token".into(),
            input
                .get("token")
                .cloned()
                .unwrap_or_else(|| json!("flow-secret")),
        );
        Ok(ConfigFlowResult::create_entry(
            input
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Fixture account"),
            data,
            Some("fixture-http-unique-id".into()),
        ))
    }

    fn options_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        self.mark_callback();
        let Some(input) = request.user_input.as_ref() else {
            return Ok(ConfigFlowResult::form(
                "init",
                Some(json!([{"name":"polling","type":"integer"}])),
            ));
        };
        Ok(ConfigFlowResult::CreateEntry {
            title: "Fixture options".into(),
            data: Map::new(),
            options: input.clone(),
            unique_id: request.unique_id.clone(),
            version: 1,
            minor_version: 1,
        })
    }
}

#[tokio::test]
async fn config_entry_http_runs_native_flows_and_lifecycle_with_admin_gates() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Fixture Owner", "owner-password-123")
        .unwrap();
    let reader = auth
        .create_user_with_username(
            &owner.id,
            "reader",
            "Fixture Reader",
            "reader-password-123",
            false,
        )
        .unwrap();
    let admin_token = auth
        .issue_tokens(&owner.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let reader_token = auth
        .issue_tokens(&reader.id, CLIENT_ID, None)
        .unwrap()
        .access_token;

    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let driver = Arc::new(FixtureDriver::default());
    entries.register_driver(driver.clone()).unwrap();

    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({"version": hass_core::VERSION}),
        },
        ApiFeatures {
            auth: Some(auth),
            config_entries: Some(entries.clone()),
            network: NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
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
    let client = Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let base = format!("http://{address}/api/config/config_entries");
    let test_thread = thread::current().id();

    let forbidden = client
        .post(format!("{base}/flow"))
        .bearer_auth(&reader_token)
        .json(&json!({"handler":"fixture_http"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let invalid = client
        .post(format!("{base}/flow"))
        .bearer_auth(&admin_token)
        .json(&json!({"handler":"missing_driver"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let missing_handler = client
        .post(format!("{base}/flow"))
        .bearer_auth(&admin_token)
        .json(&json!({"entry_id":"unused"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_handler.status(), StatusCode::BAD_REQUEST);

    let handlers: Vec<String> = client
        .get(format!("{base}/flow_handlers?type=hub"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(handlers.contains(&"fixture_http".to_owned()));
    let no_handlers: Vec<String> = client
        .get(format!("{base}/flow_handlers?type=service"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(no_handlers.is_empty());

    let aborted: Value = client
        .post(format!("{base}/flow"))
        .bearer_auth(&admin_token)
        .json(&json!({"handler":["missing_driver","fixture_http"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(aborted["type"], "form");
    let aborted_id = aborted["flow_id"].as_str().unwrap();
    let abort_response = client
        .delete(format!("{base}/flow/{aborted_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(abort_response.status(), StatusCode::OK);
    assert_eq!(
        abort_response.json::<Value>().await.unwrap()["message"],
        "Flow aborted"
    );
    let missing_flow = client
        .get(format!("{base}/flow/{aborted_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_flow.status(), StatusCode::NOT_FOUND);

    let form: Value = client
        .post(format!("{base}/flow"))
        .bearer_auth(&admin_token)
        .json(&json!({"handler":"fixture_http"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(form["type"], "form");
    assert_eq!(form["step_id"], "user");
    let flow_id = form["flow_id"].as_str().unwrap();
    let form_again: Value = client
        .get(format!("{base}/flow/{flow_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(form_again["step_id"], "user");

    let created: Value = client
        .post(format!("{base}/flow/{flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({"title":"HTTP fixture account","token":"flow-secret"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["type"], "create_entry");
    let entry_id = created["result"]["entry_id"].as_str().unwrap().to_owned();
    assert_eq!(created["result"]["state"], "loaded");
    assert_eq!(driver.setup_calls.load(Ordering::SeqCst), 1);
    assert_ne!(driver.callback_thread(), Some(test_thread));

    let listed_response = client
        .get(format!("{base}/entry?domain=fixture_http&type=hub"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let list_body = listed_response.text().await.unwrap();
    assert!(list_body.contains(&entry_id));
    assert!(!list_body.contains("flow-secret"));
    assert!(!list_body.contains("option-secret"));
    let listed: Vec<Value> = serde_json::from_str(&list_body).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].get("data").is_none());
    assert!(listed[0].get("options").is_none());

    let filtered: Vec<Value> = client
        .get(format!("{base}/entry?domain=fixture_http&type=service"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(filtered.is_empty());

    let options_form: Value = client
        .post(format!("{base}/options/flow"))
        .bearer_auth(&admin_token)
        .json(&json!({"handler":entry_id}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(options_form["type"], "form");
    let options_flow_id = options_form["flow_id"].as_str().unwrap();
    let options_result: Value = client
        .post(format!("{base}/options/flow/{options_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({"polling":30,"credential":"option-secret"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(options_result["type"], "create_entry");
    assert_eq!(entries.get_entry(&entry_id).unwrap().options["polling"], 30);
    assert_eq!(
        entries.get_entry(&entry_id).unwrap().options["credential"],
        "option-secret"
    );
    let listed_after_options = client
        .get(format!("{base}/entry?domain=fixture_http"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!listed_after_options.contains("flow-secret"));
    assert!(!listed_after_options.contains("option-secret"));
    assert_eq!(driver.setup_calls.load(Ordering::SeqCst), 2);
    assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 1);

    let reloaded: Value = client
        .post(format!("{base}/entry/{entry_id}/reload"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reloaded["require_restart"], false);
    assert_eq!(driver.setup_calls.load(Ordering::SeqCst), 3);
    assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 2);

    let removed: bool = client
        .delete(format!("{base}/entry/{entry_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(removed);
    assert!(entries.get_entry(&entry_id).is_none());
    assert_eq!(driver.unload_calls.load(Ordering::SeqCst), 3);
    let empty: Vec<Value> = client
        .get(format!("{base}/entry?domain=fixture_http"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty.is_empty());

    server.abort();
    let _ = server.await;
}
