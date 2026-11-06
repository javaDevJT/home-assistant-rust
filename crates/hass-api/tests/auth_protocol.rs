//! Exercise native provider login, OAuth exchange, identity, roles and
//! revocation through actual HTTP/WebSocket transports.
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{MemoryStore, Runtime};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const PASSWORD: &str = "local-fixture-password-123456";
const CLIENT: &str = "http://localhost:3000/";

#[tokio::test]
async fn mfa_enrollment_challenge_exchange_and_disable_use_native_transports() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let owner = auth
        .bootstrap_owner_with_username("mfa-owner", "MFA Owner", PASSWORD)
        .unwrap();
    let initial = auth.issue_tokens(&owner.id, CLIENT, None).unwrap();
    let other = auth
        .create_user_with_username(&owner.id, "other", "Other", PASSWORD, false)
        .unwrap();
    let other_token = auth.issue_tokens(&other.id, CLIENT, None).unwrap();
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({}),
        },
        ApiFeatures {
            auth: Some(auth.clone()),
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
        .unwrap()
    });
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let (mut ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap();
    assert_eq!(
        command(
            &mut ws,
            json!({"type":"auth","access_token":initial.access_token})
        )
        .await["type"],
        "auth_ok"
    );
    let setup = command(
        &mut ws,
        json!({"id":1,"type":"auth/setup_mfa","mfa_module_id":"totp"}),
    )
    .await;
    assert_eq!(setup["success"], true);
    assert_eq!(setup["result"]["type"], "form");
    let flow = setup["result"]["flow_id"].as_str().unwrap();
    let secret = setup["result"]["description_placeholders"]["code"]
        .as_str()
        .unwrap();
    assert!(
        setup["result"]["description_placeholders"]["url"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );
    assert!(
        setup["result"]["description_placeholders"]["qr_code"]
            .as_str()
            .unwrap()
            .contains("<svg")
    );
    let (mut other_ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    other_ws.next().await.unwrap().unwrap();
    command(
        &mut other_ws,
        json!({"type":"auth","access_token":other_token.access_token}),
    )
    .await;
    let stolen = command(
        &mut other_ws,
        json!({"id":1,"type":"auth/setup_mfa","flow_id":flow}),
    )
    .await;
    assert_eq!(stolen["success"], false);
    assert!(!stolen.to_string().contains(secret));
    other_ws.close(None).await.unwrap();
    let wrong = command(
        &mut ws,
        json!({"id":2,"type":"auth/setup_mfa","flow_id":flow,"user_input":{"code":"invalid"}}),
    )
    .await;
    assert_eq!(wrong["result"]["errors"]["base"], "invalid_code");
    let totp = totp_rs::Builder::new()
        .with_secret(totp_rs::Secret::try_from_base32(secret).unwrap())
        .with_algorithm(totp_rs::Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .build()
        .unwrap();
    // Consume the previous accepted window at enrollment; the current window
    // can then prove a distinct, non-replayed login without a time-based sleep.
    let now = chrono::Utc::now().timestamp() as u64;
    let enrollment_code = totp.generate(now - 30).to_string();
    let enabled=command(&mut ws,json!({"id":3,"type":"auth/setup_mfa","flow_id":flow,"user_input":{"code":enrollment_code}})).await;
    assert_eq!(enabled["result"]["type"], "create_entry");
    assert!(!enabled.to_string().contains(secret));
    assert!(auth.issue_tokens(&owner.id, CLIENT, None).is_err());
    let start: Value = client
        .post(format!("{base}/auth/login_flow"))
        .json(&json!({"client_id":CLIENT,"handler":["homeassistant",null],"redirect_uri":CLIENT}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let login_flow = start["flow_id"].as_str().unwrap();
    let challenge: Value = client
        .post(format!("{base}/auth/login_flow/{login_flow}"))
        .json(&json!({"username":"mfa-owner","password":PASSWORD,"client_id":CLIENT}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(challenge["step_id"], "mfa");
    assert!(challenge.get("result").is_none());
    let replay: Value = client
        .post(format!("{base}/auth/login_flow/{login_flow}"))
        .json(&json!({"code":enrollment_code,"client_id":CLIENT}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["type"], "form");
    let login_code = totp
        .generate(chrono::Utc::now().timestamp() as u64)
        .to_string();
    let complete: Value = client
        .post(format!("{base}/auth/login_flow/{login_flow}"))
        .json(&json!({"code":login_code,"client_id":CLIENT}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(complete["type"], "create_entry");
    let code = complete["result"].as_str().unwrap();
    let tokens: Value = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT),
            ("code", code),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        auth.validate_access_token(tokens["access_token"].as_str().unwrap())
            .unwrap()
            .id,
        owner.id
    );
    let replay = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT),
            ("code", code),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let disabled = command(
        &mut ws,
        json!({"id":4,"type":"auth/depose_mfa","mfa_module_id":"totp"}),
    )
    .await;
    assert_eq!(disabled["result"], "done");
    assert!(auth.issue_tokens(&owner.id, CLIENT, None).is_ok());
    ws.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}

async fn command<S>(socket: &mut S, value: Value) -> Value
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

#[tokio::test]
async fn onboarding_login_roles_refresh_replay_and_revocation_over_real_transports() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let auth = AuthManager::new(runtime.clone()).unwrap();
    let registries = hass_registry::Registries::new(runtime.clone()).unwrap();
    let features = ApiFeatures {
        network: hass_api::network::NetworkPolicy::new(true, &["127.0.0.1".into()]).unwrap(),
        auth: Some(auth.clone()),
        registries: Some(registries),
        templates: Some(hass_template::TemplateEngine::new(runtime.clone())),
        ..ApiFeatures::default()
    };
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let app = router_with_features(
        ApiState {
            runtime,
            access_token: String::new(),
            config: json!({"version":hass_core::VERSION}),
        },
        features,
        Some(shutdown_rx),
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
    let base = format!("http://{address}");
    let first = client
        .get(format!("{base}/api/onboarding"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.json::<Value>().await.unwrap()[0]["done"], false);
    let onboarding=client.post(format!("{base}/api/onboarding/users")).json(&json!({"username":"admin","name":"Fixture Owner","password":PASSWORD,"client_id":CLIENT,"language":"en"})).send().await.unwrap();
    assert_eq!(onboarding.status(), StatusCode::OK);
    let code = onboarding.json::<Value>().await.unwrap()["auth_code"]
        .as_str()
        .unwrap()
        .to_owned();
    let issued = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", CLIENT),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(issued.status(), StatusCode::OK);
    assert_eq!(issued.headers()["cache-control"], "no-store");
    let tokens: Value = issued.json().await.unwrap();
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let user = auth.validate_access_token(access).unwrap();
    assert!(user.is_owner);
    assert_eq!(user.name, "Fixture Owner");
    let replay = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", CLIENT),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let state = client
        .post(format!("{base}/api/states/sensor.authenticated"))
        .bearer_auth(access)
        .json(&json!({"state":"42"}))
        .send()
        .await
        .unwrap();
    assert_eq!(state.status(), StatusCode::CREATED);
    assert_eq!(
        state.json::<Value>().await.unwrap()["context"]["user_id"],
        user.id
    );
    let (mut ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    let required = ws.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(required.to_text().unwrap()).unwrap()["type"],
        "auth_required"
    );
    assert_eq!(
        command(&mut ws, json!({"type":"auth","access_token":access})).await["type"],
        "auth_ok"
    );
    let current = command(&mut ws, json!({"id":1,"type":"auth/current_user"})).await;
    assert_eq!(current["success"], true);
    assert_eq!(current["result"]["id"], user.id);
    let sessions = command(&mut ws, json!({"id":2,"type":"auth/refresh_tokens"})).await;
    assert_eq!(sessions["success"], true);
    assert_eq!(
        sessions["result"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|session| session["is_current"] == true)
            .count(),
        1
    );
    let signed=command(&mut ws,json!({"id":3,"type":"auth/sign_path","path":"/api/states/sensor.authenticated?nonce=1","expires":60})).await;
    assert_eq!(signed["success"], true);
    let signed_path = signed["result"]["path"].as_str().unwrap().to_owned();
    assert_eq!(
        client
            .get(format!("{base}{signed_path}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{base}{signed_path}&width=64&height=32"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!(
                "{base}{}",
                signed_path.replace("nonce=1", "nonce=2")
            ))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{base}{signed_path}"))
            .json(&json!({"state":"must_not_write"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let area = command(
        &mut ws,
        json!({"id":4,"type":"config/area_registry/create","name":"Fixture room"}),
    )
    .await;
    assert_eq!(area["success"], true);
    assert_eq!(area["result"]["name"], "Fixture room");
    let viewer = auth
        .create_user_with_username(&user.id, "viewer", "Fixture Viewer", PASSWORD, false)
        .unwrap();
    let viewer_tokens = auth.issue_tokens(&viewer.id, CLIENT, None).unwrap();
    let (mut viewer_ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    viewer_ws.next().await.unwrap().unwrap();
    assert_eq!(
        command(
            &mut viewer_ws,
            json!({"type":"auth","access_token":viewer_tokens.access_token})
        )
        .await["type"],
        "auth_ok"
    );
    let denied = command(
        &mut viewer_ws,
        json!({"id":1,"type":"config/area_registry/create","name":"Forbidden room"}),
    )
    .await;
    assert_eq!(denied["success"], false);
    assert_eq!(denied["error"]["code"], "unauthorized");
    auth.handle_ws(
        &json!({"type":"config/auth/update","user_id":viewer.id,"local_only":true}),
        &user.id,
    )
    .unwrap();
    let local_tokens = auth.issue_tokens(&viewer.id, CLIENT, None).unwrap();
    let local = client
        .get(format!("{base}/api/states"))
        .bearer_auth(&local_tokens.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(local.status(), StatusCode::OK);
    let remote = client
        .get(format!("{base}/api/states"))
        .header("x-forwarded-for", "10.0.0.9, 203.0.113.10")
        .bearer_auth(&local_tokens.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(remote.status(), StatusCode::UNAUTHORIZED);
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut remote_request = format!("ws://{address}/api/websocket")
        .into_client_request()
        .unwrap();
    remote_request
        .headers_mut()
        .insert("x-forwarded-for", "10.0.0.9, 203.0.113.10".parse().unwrap());
    let (mut remote_ws, _) = connect_async(remote_request).await.unwrap();
    remote_ws.next().await.unwrap().unwrap();
    assert_eq!(
        command(
            &mut remote_ws,
            json!({"type":"auth","access_token":local_tokens.access_token})
        )
        .await["type"],
        "auth_invalid"
    );
    let wrong_client = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", "http://another-client.invalid/"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_client.status(), StatusCode::BAD_REQUEST);
    let rotated = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated: Value = rotated.json().await.unwrap();
    let next_refresh = rotated["refresh_token"].as_str().unwrap();
    assert_ne!(next_refresh, refresh);
    let replay = client
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let revoked = client
        .post(format!("{base}/auth/token"))
        .form(&[("action", "revoke"), ("refresh_token", next_refresh)])
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    let denied = client
        .get(format!("{base}/api/states"))
        .bearer_auth(rotated["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        client
            .get(format!("{base}{signed_path}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let invalid_redirect=client.post(format!("{base}/auth/login_flow")).json(&json!({"handler":["homeassistant",null],"client_id":CLIENT,"redirect_uri":"https://unrelated.invalid/callback"})).send().await.unwrap();
    assert_eq!(invalid_redirect.status(), StatusCode::BAD_REQUEST);
    let flow=client.post(format!("{base}/auth/login_flow")).json(&json!({"handler":["homeassistant",null],"client_id":CLIENT,"redirect_uri":"http://localhost:3000/callback"})).send().await.unwrap();
    let flow: Value = flow.json().await.unwrap();
    let flow_id = flow["flow_id"].as_str().unwrap();
    let invalid = client
        .post(format!("{base}/auth/login_flow/{flow_id}"))
        .json(&json!({"username":"admin","password":"wrong","client_id":CLIENT}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        invalid.json::<Value>().await.unwrap()["errors"]["base"],
        "invalid_auth"
    );
    let valid = client
        .post(format!("{base}/auth/login_flow/{flow_id}"))
        .json(&json!({"username":"admin","password":PASSWORD,"client_id":CLIENT}))
        .send()
        .await
        .unwrap();
    let valid: Value = valid.json().await.unwrap();
    assert_eq!(valid["type"], "create_entry");
    assert!(valid["result"].as_str().is_some());
    let _ = shutdown.send(true);
    server.abort();
}
