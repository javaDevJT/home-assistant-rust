//! Real HTTP/WebSocket coverage for native config-subentry adapters.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, network::NetworkPolicy, router_with_features};
use hass_auth::AuthManager;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    ConfigSubentryFlowRequest, ConfigSubentryFlowResult, ConfigSubentryTypeInfo,
    ConfigSubentryUpdate, NewConfigEntry, SetupOutcome,
};
use hass_core::{Error, MemoryStore, Result, Runtime};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "http://localhost:3000/";
const DOMAIN: &str = "fixture_subentries";
const SUBENTRY_TYPE: &str = "endpoint";

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct FlowDriver;

impl ConfigEntryDriver for FlowDriver {
    fn domain(&self) -> &str {
        DOMAIN
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

    fn supported_subentry_types(
        &self,
        _entry: &ConfigEntry,
    ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
        BTreeMap::from([(
            SUBENTRY_TYPE.to_owned(),
            ConfigSubentryTypeInfo {
                supports_reconfigure: true,
            },
        )])
    }

    fn options_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        Ok(ConfigFlowResult::form("init", Some(json!([]))))
    }

    fn subentry_flow(
        &self,
        request: &ConfigSubentryFlowRequest,
    ) -> Result<ConfigSubentryFlowResult> {
        let Some(input) = request.user_input.as_ref() else {
            return Ok(ConfigSubentryFlowResult::Form {
                step_id: request.source.clone(),
                data_schema: Some(json!([{"name":"title","required":true,"type":"string"}])),
                errors: None,
                description_placeholders: Some(Map::from_iter([(
                    "entry_id".into(),
                    json!(request.entry_id),
                )])),
            });
        };

        if request.source == "user"
            && input.get("validation_fixture").and_then(Value::as_bool) == Some(true)
        {
            if input
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| choices.iter().any(|choice| choice == "invalid"))
            {
                return Err(Error::InvalidData {
                    schema_errors: Map::from_iter([(
                        "choices".into(),
                        json!("invalid is not a valid option"),
                    )]),
                });
            }
            return Ok(ConfigSubentryFlowResult::Abort {
                reason: "validation_retry_succeeded".into(),
            });
        }

        match request.source.as_str() {
            "user" => Ok(ConfigSubentryFlowResult::CreateEntry {
                title: input
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled child")
                    .to_owned(),
                data: input.clone(),
                unique_id: input
                    .get("unique_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            "reconfigure" => {
                let title = input
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Renamed child")
                    .to_owned();
                let unique_id = input
                    .get("unique_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut data_updates = Map::new();
                if let Some(token) = input.get("token") {
                    data_updates.insert("token".into(), token.clone());
                }
                Ok(ConfigSubentryFlowResult::UpdateAndAbort {
                    update: ConfigSubentryUpdate {
                        title: Some(title),
                        unique_id: Some(unique_id),
                        data: None,
                    },
                    data_updates: Some(data_updates),
                    reason: None,
                    reload: false,
                })
            }
            source => Err(Error::InvalidInput(format!(
                "unsupported fixture subentry source {source}"
            ))),
        }
    }
}

async fn start_server(
    router: axum::Router,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_signal) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_signal.await;
        })
        .await
    });
    (address, shutdown, server)
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

async fn close_socket(socket: &mut Socket) {
    socket
        .send(Message::Close(None))
        .await
        .expect("send websocket close frame");
    loop {
        let frame = timeout(Duration::from_secs(4), socket.next())
            .await
            .expect("WebSocket close timed out");
        match frame {
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
            Some(Ok(Message::Ping(payload))) => socket
                .send(Message::Pong(payload))
                .await
                .expect("respond to websocket ping while closing"),
            Some(Ok(_)) => {}
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

fn auth_fixture(runtime: &Runtime) -> (AuthManager, String, String) {
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("owner", "Subentry Owner", "subentry-owner-password")
        .unwrap();
    let readonly_groups = vec!["system-read-only".to_owned()];
    let reader = auth
        .create_user_without_credentials(&owner.id, "Subentry Reader", Some(&readonly_groups), None)
        .unwrap();
    let owner_token = auth
        .issue_tokens(&owner.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    let reader_token = auth
        .issue_tokens(&reader.id, CLIENT_ID, None)
        .unwrap()
        .access_token;
    (auth, owner_token, reader_token)
}

async fn start_subentry_flow(
    client: &Client,
    base: &str,
    token: &str,
    _entry_id: &str,
    extra: Value,
) -> Value {
    client
        .post(base)
        .bearer_auth(token)
        .json(&extra)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn config_subentry_http_and_websocket_protocol_uses_native_flows_and_persisted_children() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let (auth, admin_token, reader_token) = auth_fixture(&runtime);
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let driver = Arc::new(FlowDriver);
    entries.register_driver(driver.clone()).unwrap();
    let parent = entries
        .create_entry(NewConfigEntry::new(DOMAIN, "Fixture parent", Map::new()))
        .unwrap();
    let entry_id = parent.entry_id.clone();

    let app = router_with_features(
        ApiState {
            runtime: runtime.clone(),
            access_token: String::new(),
            config: json!({"version": hass_core::VERSION}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
            config_entries: Some(entries.clone()),
            network: NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
            ..ApiFeatures::default()
        },
        None,
    );
    let (address, shutdown, server) = start_server(app).await;
    let client = Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let flow_url = format!("http://{address}/api/config/config_entries/subentries/flow");

    let malformed_handler = client
        .post(&flow_url)
        .bearer_auth(&admin_token)
        .json(&json!({"handler":[entry_id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed_handler.status(), StatusCode::BAD_REQUEST);

    let unsupported_handler = client
        .post(&flow_url)
        .bearer_auth(&admin_token)
        .json(&json!({"handler":[entry_id,"not-supported"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported_handler.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        unsupported_handler.json::<Value>().await.unwrap()["message"],
        "Invalid handler specified"
    );

    let denied_create = client
        .post(&flow_url)
        .bearer_auth(&reader_token)
        .json(&json!({"handler":[entry_id,SUBENTRY_TYPE]}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied_create.status(), StatusCode::UNAUTHORIZED);

    let create = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({
            "handler":[entry_id,SUBENTRY_TYPE],
            "ignored_source_field":"accepted by the pinned index schema"
        }),
    )
    .await;
    let create_flow_id = create["flow_id"].as_str().unwrap().to_owned();
    assert!(!create_flow_id.is_empty());
    assert_eq!(create["handler"], json!([entry_id, SUBENTRY_TYPE]));
    assert_eq!(create["type"], "form");
    assert_eq!(create["step_id"], "user");
    assert!(create.get("context").is_none());
    assert!(create["description_placeholders"]["entry_id"] == entry_id);

    let reader_get = client
        .get(format!("{flow_url}/{create_flow_id}"))
        .bearer_auth(&reader_token)
        .send()
        .await
        .unwrap();
    assert_eq!(reader_get.status(), StatusCode::UNAUTHORIZED);
    let reader_submit = client
        .post(format!("{flow_url}/{create_flow_id}"))
        .bearer_auth(&reader_token)
        .json(&json!({"title":"Denied"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reader_submit.status(), StatusCode::UNAUTHORIZED);

    let get_form = client
        .get(format!("{flow_url}/{create_flow_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(get_form.status(), StatusCode::OK);
    let get_form: Value = get_form.json().await.unwrap();
    assert_eq!(get_form["flow_id"], create_flow_id);
    assert!(get_form.get("context").is_none());

    let options_flow = entries.init_options_flow(&entry_id).unwrap();
    let options_flow_id = options_flow["flow_id"].as_str().unwrap();
    let wrong_kind_flow = client
        .get(format!("{flow_url}/{options_flow_id}"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_kind_flow.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        wrong_kind_flow.json::<Value>().await.unwrap()["message"],
        "Invalid flow specified"
    );
    entries.abort_flow(options_flow_id).unwrap();

    let invalid_step_body = client
        .post(format!("{flow_url}/{create_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!([]))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_step_body.status(), StatusCode::BAD_REQUEST);

    let validation_flow = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({"handler":[entry_id,SUBENTRY_TYPE]}),
    )
    .await;
    let validation_flow_id = validation_flow["flow_id"].as_str().unwrap();
    let schema_error = client
        .post(format!("{flow_url}/{validation_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({"validation_fixture":true,"choices":["valid","invalid"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(schema_error.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        schema_error.json::<Value>().await.unwrap(),
        json!({"errors":{"choices":"invalid is not a valid option"}})
    );
    let valid_retry = client
        .post(format!("{flow_url}/{validation_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({"validation_fixture":true,"choices":["valid"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(valid_retry.status(), StatusCode::OK);
    assert_eq!(
        valid_retry.json::<Value>().await.unwrap()["reason"],
        "validation_retry_succeeded"
    );

    let create_response = client
        .post(format!("{flow_url}/{create_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({
            "title":"Primary child",
            "unique_id":"endpoint-primary",
            "token":"private-primary",
            "private_marker":"keep-through-reconfigure"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::OK);
    let created: Value = create_response.json().await.unwrap();
    assert_eq!(created["type"], "create_entry");
    assert!(created.get("context").is_none());
    let primary_id = created["result"]["subentry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!primary_id.is_empty());
    assert_ne!(primary_id, create_flow_id);
    assert_eq!(created["result"]["unique_id"], "endpoint-primary");
    assert!(created["result"].get("data").is_none());

    let sibling_flow = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({"handler":[entry_id,SUBENTRY_TYPE]}),
    )
    .await;
    let sibling_flow_id = sibling_flow["flow_id"].as_str().unwrap();
    let sibling_response = client
        .post(format!("{flow_url}/{sibling_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({
            "title":"Sibling child",
            "unique_id":"endpoint-sibling",
            "token":"private-sibling",
            "private_marker":"sibling-private"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(sibling_response.status(), StatusCode::OK);
    let sibling: Value = sibling_response.json().await.unwrap();
    let sibling_id = sibling["result"]["subentry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(sibling_id, primary_id);

    let duplicate_flow = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({"handler":[entry_id,SUBENTRY_TYPE]}),
    )
    .await;
    let duplicate_flow_id = duplicate_flow["flow_id"].as_str().unwrap();
    let duplicate_response = client
        .post(format!("{flow_url}/{duplicate_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({
            "title":"Duplicate",
            "unique_id":"endpoint-primary",
            "token":"must-not-persist"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate_response.status(), StatusCode::OK);
    let duplicate: Value = duplicate_response.json().await.unwrap();
    assert_eq!(duplicate["type"], "abort");
    assert_eq!(duplicate["reason"], "already_configured");
    assert_eq!(duplicate["translation_domain"], "homeassistant");
    assert!(duplicate.get("context").is_none());
    assert_eq!(entries.list_subentries(&entry_id).unwrap().len(), 2);

    let reconfigure = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({
            "handler":[entry_id,SUBENTRY_TYPE],
            "subentry_id":primary_id
        }),
    )
    .await;
    let reconfigure_flow_id = reconfigure["flow_id"].as_str().unwrap();
    assert_eq!(reconfigure["type"], "form");
    assert_eq!(reconfigure["step_id"], "reconfigure");
    assert!(reconfigure.get("context").is_none());
    let reconfigure_response = client
        .post(format!("{flow_url}/{reconfigure_flow_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({
            "title":"Renamed primary",
            "unique_id":"endpoint-renamed",
            "token":"private-primary-updated"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reconfigure_response.status(), StatusCode::OK);
    let reconfigured: Value = reconfigure_response.json().await.unwrap();
    assert_eq!(reconfigured["type"], "abort");
    assert_eq!(reconfigured["reason"], "reconfigure_successful");
    assert_eq!(reconfigured["translation_domain"], "homeassistant");
    assert_eq!(reconfigured["description_placeholders"], Value::Null);
    assert!(reconfigured.get("context").is_none());

    let children = entries.list_subentries(&entry_id).unwrap();
    assert_eq!(children.len(), 2);
    let primary = children
        .iter()
        .find(|child| child.subentry_id == primary_id)
        .unwrap();
    let sibling_native = children
        .iter()
        .find(|child| child.subentry_id == sibling_id)
        .unwrap();
    assert_eq!(primary.title, "Renamed primary");
    assert_eq!(primary.unique_id.as_deref(), Some("endpoint-renamed"));
    assert_eq!(primary.data["private_marker"], "keep-through-reconfigure");
    assert_eq!(primary.data["token"], "private-primary-updated");
    assert_eq!(sibling_native.title, "Sibling child");
    assert_eq!(
        sibling_native.unique_id.as_deref(),
        Some("endpoint-sibling")
    );

    let mut admin_ws = connect(address, &admin_token).await;
    let mut reader_ws = connect(address, &reader_token).await;
    let denied_list = command(
        &mut reader_ws,
        json!({"id":1,"type":"config_entries/subentries/list","entry_id":entry_id}),
    )
    .await;
    assert_eq!(denied_list["success"], false);
    assert_eq!(denied_list["error"]["code"], "unauthorized");

    let listed = command(
        &mut admin_ws,
        json!({"id":1,"type":"config_entries/subentries/list","entry_id":entry_id}),
    )
    .await;
    assert_eq!(listed["success"], true);
    assert_eq!(listed["result"].as_array().unwrap().len(), 2);
    let listed_primary = listed["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["subentry_id"] == primary_id)
        .unwrap();
    assert_eq!(listed_primary["title"], "Renamed primary");
    assert_eq!(listed_primary["unique_id"], "endpoint-renamed");
    assert!(listed_primary.get("data").is_none());

    let malformed_update = command(
        &mut admin_ws,
        json!({
            "id":2,
            "type":"config_entries/subentries/update",
            "entry_id":entry_id,
            "subentry_id":sibling_id,
            "data":{"leak":"not an exposed update field"}
        }),
    )
    .await;
    assert_eq!(malformed_update["success"], false);
    assert_eq!(malformed_update["error"]["code"], "invalid_format");

    let empty_title = command(
        &mut admin_ws,
        json!({
            "id":3,
            "type":"config_entries/subentries/update",
            "entry_id":entry_id,
            "subentry_id":sibling_id,
            "title":""
        }),
    )
    .await;
    assert_eq!(empty_title["success"], true);
    assert_eq!(empty_title["result"], Value::Null);

    let missing_child = command(
        &mut admin_ws,
        json!({
            "id":4,
            "type":"config_entries/subentries/update",
            "entry_id":entry_id,
            "subentry_id":"missing-child",
            "title":"Ignored"
        }),
    )
    .await;
    assert_eq!(missing_child["error"]["code"], "not_found");
    assert_eq!(
        missing_child["error"]["message"],
        "Config subentry not found"
    );

    let deleted = command(
        &mut admin_ws,
        json!({
            "id":5,
            "type":"config_entries/subentries/delete",
            "entry_id":entry_id,
            "subentry_id":sibling_id
        }),
    )
    .await;
    assert_eq!(deleted["success"], true);
    assert_eq!(deleted["result"], Value::Null);
    let after_delete = command(
        &mut admin_ws,
        json!({"id":6,"type":"config_entries/subentries/list","entry_id":entry_id}),
    )
    .await;
    assert_eq!(after_delete["result"].as_array().unwrap().len(), 1);
    assert_eq!(after_delete["result"][0]["subentry_id"], primary_id);

    let missing_parent = command(
        &mut admin_ws,
        json!({
            "id":7,
            "type":"config_entries/subentries/delete",
            "entry_id":"missing-parent",
            "subentry_id":"missing-child"
        }),
    )
    .await;
    assert_eq!(missing_parent["error"]["code"], "not_found");
    assert_eq!(missing_parent["error"]["message"], "Config entry not found");

    let readonly_abort_flow = start_subentry_flow(
        &client,
        &flow_url,
        &admin_token,
        &entry_id,
        json!({"handler":[entry_id,SUBENTRY_TYPE]}),
    )
    .await;
    let readonly_abort_id = readonly_abort_flow["flow_id"].as_str().unwrap();
    let anonymous_abort = client
        .delete(format!("{flow_url}/{readonly_abort_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous_abort.status(), StatusCode::UNAUTHORIZED);
    let readonly_abort = client
        .delete(format!("{flow_url}/{readonly_abort_id}"))
        .bearer_auth(&reader_token)
        .send()
        .await
        .unwrap();
    assert_eq!(readonly_abort.status(), StatusCode::OK);
    assert_eq!(
        readonly_abort.json::<Value>().await.unwrap(),
        json!({"message":"Flow aborted"})
    );

    let invalid_flow = client
        .get(format!("{flow_url}/not-a-subentry-flow"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_flow.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        invalid_flow.json::<Value>().await.unwrap()["message"],
        "Invalid flow specified"
    );

    close_socket(&mut admin_ws).await;
    close_socket(&mut reader_ws).await;
    drop(admin_ws);
    drop(reader_ws);
    let _ = shutdown.send(());
    server.await.unwrap().unwrap();
    drop(entries);
    drop(driver);
    drop(auth);
    drop(runtime);

    let restarted_runtime = Runtime::new(store).unwrap();
    let restarted_entries = ConfigEntries::new(restarted_runtime).unwrap();
    let restored = restarted_entries
        .get_subentry(&entry_id, &primary_id)
        .expect("created subentry should persist through Runtime reopen");
    assert_eq!(restored.title, "Renamed primary");
    assert_eq!(restored.unique_id.as_deref(), Some("endpoint-renamed"));
    assert_eq!(restored.data["private_marker"], "keep-through-reconfigure");
    assert_eq!(restored.data["token"], "private-primary-updated");
    assert!(!restarted_entries.subentry_exists(&entry_id, &sibling_id));
}
