//! Draft loopback HTTP fixtures for the native OAuth config-flow bridge.
//!
//! These fixtures use temporary SQLite storage and synthetic OAuth values.
//! The only HTTP listeners are bound to 127.0.0.1; no provider, browser, or
//! device traffic is performed.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures_util::future::BoxFuture;
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_application_credentials::{
    ApplicationCredentialsIntegration, ApplicationCredentialsPlatform, AuthorizationServer,
    CreateCredential,
};
use hass_auth::AuthManager;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    SetupOutcome,
};
use hass_core::{Runtime, Store};
use hass_oauth2_client::{Clock, OAuthClientFactory};
use hass_oauth2_flow::{
    ConfigEntryTokenStore, OAuthEntryDetails, OAuthFlowHost, OAuthFlowHostOptions,
    OAuthFlowProvider, OAuthFlowRegistrationGuard, OAuthPostAuthorizationContext,
};
use hass_storage::SqliteStore;
use reqwest::{Client, Response as ClientResponse};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{Notify, watch},
    task::JoinHandle,
    time::timeout,
};
use url::{Url, form_urlencoded};

const CLIENT_ID: &str = "oauth-flow-api-fixture";
const DOMAIN: &str = "oauth_api_fixture";
const CLIENT_SECRET: &str = "synthetic-client-secret-sentinel";
const ACCESS_TOKEN: &str = "synthetic-provider-access-sentinel";
const REFRESH_TOKEN: &str = "synthetic-provider-refresh-sentinel";
const CALLBACK_CODE: &str = "synthetic-authorization-code-sentinel";
const CALLBACK_ERROR: &str = "synthetic-provider-error-sentinel";
const TOKEN_BODY_SENTINEL: &str = "synthetic-token-endpoint-body-sentinel";
const CALLBACK_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorization complete</title></head><body><p>You can close this window.</p><script>window.close();</script></body></html>";

#[derive(Clone, Copy)]
enum TokenMode {
    Success,
    Block,
    Reject,
}

struct TokenEndpoint {
    mode: TokenMode,
    requests: AtomicUsize,
    forms: Mutex<Vec<BTreeMap<String, String>>>,
    arrived: Notify,
    release: Notify,
    finished: Notify,
}

impl TokenEndpoint {
    fn new(mode: TokenMode) -> Self {
        Self {
            mode,
            requests: AtomicUsize::new(0),
            forms: Mutex::new(Vec::new()),
            arrived: Notify::new(),
            release: Notify::new(),
            finished: Notify::new(),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn wait_for_request(&self) {
        timeout(Duration::from_secs(4), self.arrived.notified())
            .await
            .expect("token endpoint was not called");
    }
}

async fn handle_token_post(State(state): State<Arc<TokenEndpoint>>, body: Bytes) -> Response<Body> {
    struct Finished(Arc<TokenEndpoint>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.finished.notify_one();
        }
    }
    let _finished = Finished(state.clone());
    state.requests.fetch_add(1, Ordering::SeqCst);
    let form = form_urlencoded::parse(&body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<BTreeMap<_, _>>();
    state.forms.lock().expect("token forms mutex").push(form);
    state.arrived.notify_one();

    match state.mode {
        TokenMode::Success | TokenMode::Block => {
            if matches!(state.mode, TokenMode::Block) {
                state.release.notified().await;
            }
            (
                StatusCode::OK,
                Json(json!({
                    "access_token": ACCESS_TOKEN,
                    "refresh_token": REFRESH_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 3600
                })),
            )
                .into_response()
        }
        TokenMode::Reject => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "text/plain")
            .body(Body::from(TOKEN_BODY_SENTINEL))
            .expect("token error response"),
    }
}

#[derive(Default)]
struct ManualClock(AtomicI64);

impl ManualClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }

    fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct FixtureProvider {
    token_url: String,
    authorization_calls: AtomicUsize,
}

impl ConfigEntryDriver for FixtureProvider {
    fn domain(&self) -> &str {
        DOMAIN
    }

    fn version(&self) -> (u32, u32) {
        (1, 1)
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
        Ok(())
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        Err(hass_core::Error::InvalidInput(
            "OAuth fixture flow is owned by the native host".to_owned(),
        ))
    }
}

impl OAuthFlowProvider for FixtureProvider {
    fn application_credentials_platform(&self) -> ApplicationCredentialsPlatform {
        let token_url = self.token_url.clone();
        ApplicationCredentialsPlatform::new().with_authorization_server_factory(move |_| {
            let token_url = token_url.clone();
            Box::pin(async move {
                Ok(AuthorizationServer {
                    authorize_url: "https://auth.example.test/authorize".to_owned(),
                    token_url,
                })
            })
        })
    }

    fn oauth_scopes(&self, _request: &ConfigFlowRequest) -> Vec<String> {
        vec!["profile".to_owned(), "email".to_owned()]
    }

    fn post_authorize<'a>(
        &'a self,
        context: OAuthPostAuthorizationContext,
    ) -> BoxFuture<'a, Result<OAuthEntryDetails, hass_oauth2_flow::OAuthFlowError>> {
        self.authorization_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut data = context
                .existing_entry
                .as_ref()
                .map(|entry| entry.data.clone())
                .unwrap_or_default();
            data.insert(
                "provider_account_marker".to_owned(),
                json!("private-account-sentinel"),
            );
            if !data.contains_key("unrelated_data") {
                data.insert("unrelated_data".to_owned(), json!("preserve-this-data"));
            }
            let mut options = context
                .existing_entry
                .as_ref()
                .map(|entry| entry.options.clone())
                .unwrap_or_default();
            if !options.contains_key("unrelated_option") {
                options.insert("unrelated_option".to_owned(), json!("preserve-this-option"));
            }
            Ok(OAuthEntryDetails::new(
                "OAuth API fixture account",
                data,
                options,
                Some("oauth-api-fixture-account".to_owned()),
                (1, 1),
            ))
        })
    }
}

struct AuthTokens {
    auth: AuthManager,
    admin: String,
    ordinary: String,
}

struct Fixture {
    _directory: TempDir,
    config_entries: ConfigEntries,
    app_credentials: ApplicationCredentialsIntegration,
    oauth_host: OAuthFlowHost,
    _provider_registration: OAuthFlowRegistrationGuard,
    provider: Arc<FixtureProvider>,
    clock: Arc<ManualClock>,
    tokens: AuthTokens,
    token_endpoint: Arc<TokenEndpoint>,
    client: Client,
    api_base: String,
    callback_url: String,
    shutdown_tx: watch::Sender<bool>,
    api_server: JoinHandle<std::io::Result<()>>,
    token_server: JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn new(token_mode: TokenMode) -> Self {
        let directory = tempfile::tempdir().expect("temporary storage directory");
        let store: Arc<dyn Store> = Arc::new(
            SqliteStore::open(directory.path().join("oauth.sqlite")).expect("SQLite store"),
        );
        let runtime = Runtime::new(store).expect("runtime");
        let config_entries = ConfigEntries::new(runtime.clone()).expect("config entries");
        let tokens = auth_tokens(&runtime);
        let app_credentials =
            ApplicationCredentialsIntegration::new(runtime.clone(), config_entries.clone())
                .expect("application credentials integration");

        let token_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind token loopback listener");
        let token_address = token_listener.local_addr().expect("token listener address");
        let token_url =
            Url::parse(&format!("http://{token_address}/token")).expect("loopback token URL");
        let token_endpoint = Arc::new(TokenEndpoint::new(token_mode));
        let token_router = Router::new()
            .route("/token", post(handle_token_post))
            .with_state(Arc::clone(&token_endpoint));
        let token_server =
            tokio::spawn(async move { axum::serve(token_listener, token_router).await });

        let api_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind API loopback listener");
        let api_address = api_listener.local_addr().expect("API listener address");
        let callback_url = format!("http://{api_address}/auth/external/callback");
        let callback_origin = Url::parse(&callback_url).expect("fixed callback URL");
        let clock = Arc::new(ManualClock::new(1_800_000_000));
        let client_factory = OAuthClientFactory::with_clock_without_proxy(
            Arc::new(ConfigEntryTokenStore::new(config_entries.clone())),
            clock.clone(),
        )
        .expect("loopback-only OAuth client factory");
        let oauth_host = OAuthFlowHost::new(
            config_entries.clone(),
            app_credentials.manager(),
            app_credentials.platform_registry(),
            client_factory,
            OAuthFlowHostOptions::new(callback_origin)
                .with_max_active_flows(8)
                .with_resolver_timeout(Duration::from_secs(8)),
        )
        .expect("OAuth flow host");
        let provider = Arc::new(FixtureProvider {
            token_url: token_url.to_string(),
            authorization_calls: AtomicUsize::new(0),
        });
        let provider_registration = oauth_host
            .register_provider(Arc::clone(&provider) as Arc<dyn OAuthFlowProvider>)
            .expect("register loaded OAuth provider");
        app_credentials
            .manager()
            .create_admin(CreateCredential {
                domain: DOMAIN.to_owned(),
                client_id: "synthetic-client-id-sentinel".to_owned(),
                client_secret: CLIENT_SECRET.to_owned(),
                auth_domain: Some(DOMAIN.to_owned()),
                name: Some("Synthetic OAuth API fixture".to_owned()),
            })
            .expect("create synthetic application credential");

        let shutdown = watch::channel(false);
        let router = router_with_features(
            ApiState {
                runtime: runtime.clone(),
                access_token: String::new(),
                config: json!({}),
            },
            ApiFeatures {
                auth: Some(tokens.auth.clone()),
                config_entries: Some(config_entries.clone()),
                application_credentials: Some(app_credentials.manager()),
                oauth2_flows: Some(oauth_host.manager()),
                ..ApiFeatures::default()
            },
            Some(shutdown.1.clone()),
        );
        let mut server_shutdown = shutdown.1.clone();
        let api_server = tokio::spawn(async move {
            axum::serve(
                api_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = server_shutdown.changed().await;
            })
            .await
        });
        let client = Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client without proxy settings");

        Self {
            _directory: directory,
            config_entries,
            app_credentials,
            oauth_host,
            _provider_registration: provider_registration,
            provider,
            clock,
            tokens,
            token_endpoint,
            client,
            api_base: format!("http://{api_address}"),
            callback_url,
            shutdown_tx: shutdown.0,
            api_server,
            token_server,
        }
    }

    async fn start_flow(&self, token: &str) -> Value {
        let response = self
            .client
            .post(format!("{}/api/config/config_entries/flow", self.api_base))
            .bearer_auth(token)
            .header("HA-Frontend-Base", "https://attacker.example.invalid/")
            .json(&json!({"handler": DOMAIN}))
            .send()
            .await
            .expect("start OAuth flow over loopback HTTP");
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("flow start JSON")
    }

    async fn get_flow(&self, token: &str, flow_id: &str) -> ClientResponse {
        self.client
            .get(format!(
                "{}/api/config/config_entries/flow/{flow_id}",
                self.api_base
            ))
            .bearer_auth(token)
            .send()
            .await
            .expect("get OAuth flow over loopback HTTP")
    }

    async fn abort_flow(&self, token: &str, flow_id: &str) -> ClientResponse {
        self.client
            .delete(format!(
                "{}/api/config/config_entries/flow/{flow_id}",
                self.api_base
            ))
            .bearer_auth(token)
            .send()
            .await
            .expect("abort OAuth flow over loopback HTTP")
    }

    async fn callback_query(&self, query: &str) -> (StatusCode, String) {
        let url = if query.is_empty() {
            self.callback_url.clone()
        } else {
            format!("{}?{query}", self.callback_url)
        };
        let response = self
            .client
            .get(url)
            .send()
            .await
            .expect("public OAuth callback over loopback HTTP");
        let status = response.status();
        let body = response.text().await.expect("callback response body");
        (status, body)
    }

    fn authorization_state(&self, start: &Value) -> (String, String) {
        assert_eq!(start["type"], "external");
        assert_eq!(start["step_id"], "auth");
        assert!(start.get("state").is_none());
        assert!(start.get("context").is_none());
        let authorization_url = Url::parse(start["url"].as_str().expect("external URL"))
            .expect("valid authorization URL");
        let state = authorization_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("authorization URL carries signed state");
        let redirect_uri = authorization_url
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .expect("authorization URL carries redirect URI");
        assert_eq!(redirect_uri, self.callback_url);
        assert!(!start.to_string().contains(CLIENT_SECRET));
        (state, redirect_uri)
    }

    fn callback_query_for(state: &str, code: Option<&str>, error: Option<&str>) -> String {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("state", state);
        if let Some(code) = code {
            query.append_pair("code", code);
        }
        if let Some(error) = error {
            query.append_pair("error", error);
        }
        query.finish()
    }

    async fn shutdown(&mut self) {
        self.shutdown_tx.send_replace(true);
        if timeout(Duration::from_secs(3), &mut self.api_server)
            .await
            .is_err()
        {
            self.api_server.abort();
        }
        self.oauth_host.shutdown().await;
        self.app_credentials.shutdown().await;
        self.token_server.abort();
        let _ = (&mut self.token_server).await;
    }
}

fn auth_tokens(runtime: &Runtime) -> AuthTokens {
    let auth = AuthManager::new(runtime.clone()).expect("auth manager");
    let owner = auth
        .bootstrap_owner_with_username("owner", "OAuth Fixture Owner", "owner-password")
        .expect("owner user");
    let ordinary = auth
        .create_user_with_username(
            &owner.id,
            "ordinary",
            "OAuth Fixture Ordinary User",
            "ordinary-password",
            false,
        )
        .expect("ordinary user");
    let admin_token = auth
        .issue_tokens(&owner.id, CLIENT_ID, None)
        .expect("admin token")
        .access_token;
    let ordinary_token = auth
        .issue_tokens(&ordinary.id, CLIENT_ID, None)
        .expect("ordinary token")
        .access_token;
    AuthTokens {
        auth,
        admin: admin_token,
        ordinary: ordinary_token,
    }
}

#[tokio::test]
async fn callback_is_public_and_defers_one_exchange_until_admin_refresh() {
    let mut fixture = Fixture::new(TokenMode::Success).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);

    let forbidden_get = fixture
        .get_flow(&fixture.tokens.ordinary, start["flow_id"].as_str().unwrap())
        .await;
    assert_eq!(forbidden_get.status(), StatusCode::FORBIDDEN);

    let query = Fixture::callback_query_for(&state, Some(CALLBACK_CODE), Some(CALLBACK_ERROR));
    let (callback_status, callback_body) = fixture.callback_query(&query).await;
    assert_eq!(callback_status, StatusCode::OK);
    assert_eq!(callback_body, CALLBACK_HTML);
    assert!(!callback_body.contains(&state));
    assert!(!callback_body.contains(CALLBACK_CODE));
    assert!(!callback_body.contains(CALLBACK_ERROR));
    assert_eq!(fixture.token_endpoint.request_count(), 0);

    let flow_id = start["flow_id"].as_str().unwrap();
    let external_done = fixture
        .config_entries
        .flow_result(flow_id)
        .expect("external done");
    assert_eq!(external_done["type"], "external_done");
    assert_eq!(external_done["step_id"], "creation");
    assert!(!external_done.to_string().contains(CALLBACK_CODE));
    assert!(!external_done.to_string().contains(CLIENT_SECRET));

    let resolved_response = fixture.get_flow(&fixture.tokens.admin, flow_id).await;
    assert_eq!(resolved_response.status(), StatusCode::OK);
    let resolved_body = resolved_response.text().await.expect("resolved flow body");
    assert!(!resolved_body.contains(CLIENT_SECRET));
    assert!(!resolved_body.contains(ACCESS_TOKEN));
    assert!(!resolved_body.contains(REFRESH_TOKEN));
    assert!(!resolved_body.contains(CALLBACK_CODE));
    assert_eq!(fixture.token_endpoint.request_count(), 1);
    assert_eq!(
        fixture.provider.authorization_calls.load(Ordering::SeqCst),
        1
    );

    let forms = fixture
        .token_endpoint
        .forms
        .lock()
        .expect("captured token forms")
        .clone();
    assert_eq!(forms.len(), 1);
    assert_eq!(
        forms[0].get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        forms[0].get("code").map(String::as_str),
        Some(CALLBACK_CODE)
    );
    let entries = fixture.config_entries.list_entries(Some(DOMAIN));
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.data["token"]["access_token"], ACCESS_TOKEN);
    assert_eq!(entry.data["token"]["refresh_token"], REFRESH_TOKEN);
    assert_eq!(entry.data["unrelated_data"], "preserve-this-data");
    assert_eq!(entry.options["unrelated_option"], "preserve-this-option");
    assert!(entry.data.contains_key("auth_implementation"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn malformed_expired_wrong_duplicate_and_replayed_callbacks_never_exchange_tokens() {
    let mut fixture = Fixture::new(TokenMode::Success).await;
    let malformed = fixture.start_flow(&fixture.tokens.admin).await;
    let (malformed_state, _) = fixture.authorization_state(&malformed);
    let missing_result = fixture
        .callback_query(&format!("state={malformed_state}"))
        .await;
    assert_eq!(missing_result.0, StatusCode::BAD_REQUEST);

    let wrong_state = format!("{malformed_state}x");
    let wrong_query = Fixture::callback_query_for(&wrong_state, Some(CALLBACK_CODE), None);
    assert_eq!(
        fixture.callback_query(&wrong_query).await.0,
        StatusCode::BAD_REQUEST
    );

    let duplicate_query =
        format!("state={malformed_state}&state={malformed_state}&code={CALLBACK_CODE}");
    assert_eq!(
        fixture.callback_query(&duplicate_query).await.0,
        StatusCode::BAD_REQUEST
    );

    let duplicate_code_query =
        format!("state={malformed_state}&code={CALLBACK_CODE}&code=another-code");
    assert_eq!(
        fixture.callback_query(&duplicate_code_query).await.0,
        StatusCode::BAD_REQUEST
    );

    let replay = fixture.start_flow(&fixture.tokens.admin).await;
    let (replay_state, _) = fixture.authorization_state(&replay);
    let replay_query = Fixture::callback_query_for(&replay_state, Some(CALLBACK_CODE), None);
    let first = fixture.callback_query(&replay_query).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1, CALLBACK_HTML);
    let second = fixture.callback_query(&replay_query).await;
    assert_eq!(second.0, StatusCode::BAD_REQUEST);
    assert!(!second.1.contains(CALLBACK_CODE));

    let expired = fixture.start_flow(&fixture.tokens.admin).await;
    let (expired_state, _) = fixture.authorization_state(&expired);
    fixture.clock.advance(601);
    let expired_query = Fixture::callback_query_for(&expired_state, Some(CALLBACK_CODE), None);
    assert_eq!(
        fixture.callback_query(&expired_query).await.0,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(fixture.token_endpoint.request_count(), 0);
    assert!(fixture.config_entries.list_entries(Some(DOMAIN)).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn provider_denial_is_acknowledged_without_token_post_or_public_error_details() {
    let mut fixture = Fixture::new(TokenMode::Success).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("state", &state);
    query.append_pair("error", "access_denied");
    query.append_pair("error_description", CALLBACK_ERROR);

    let (status, body) = fixture.callback_query(&query.finish()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, CALLBACK_HTML);
    assert!(!body.contains(CALLBACK_ERROR));
    assert_eq!(fixture.token_endpoint.request_count(), 0);

    let external_done = fixture
        .config_entries
        .flow_result(start["flow_id"].as_str().unwrap())
        .expect("denial callback is staged");
    assert_eq!(external_done["type"], "external_done");
    assert_eq!(external_done["step_id"], "authorize_rejected");
    let response = fixture
        .get_flow(&fixture.tokens.admin, start["flow_id"].as_str().unwrap())
        .await;
    let public_body = response.text().await.expect("denied flow body");
    assert!(!public_body.contains(CALLBACK_ERROR));
    assert!(!public_body.contains("access_denied"));
    assert_eq!(fixture.token_endpoint.request_count(), 0);
    assert!(fixture.config_entries.list_entries(Some(DOMAIN)).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn concurrent_admin_refreshes_share_one_loopback_token_exchange() {
    let mut fixture = Fixture::new(TokenMode::Block).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);
    let query = Fixture::callback_query_for(&state, Some(CALLBACK_CODE), None);
    assert_eq!(fixture.callback_query(&query).await.0, StatusCode::OK);
    let flow_id = start["flow_id"].as_str().unwrap().to_owned();

    let client_a = fixture.client.clone();
    let client_b = fixture.client.clone();
    let url = format!(
        "{}/api/config/config_entries/flow/{flow_id}",
        fixture.api_base
    );
    let admin_a = fixture.tokens.admin.clone();
    let admin_b = fixture.tokens.admin.clone();
    let first = tokio::spawn(async move {
        client_a
            .get(&url)
            .bearer_auth(admin_a)
            .send()
            .await
            .unwrap()
    });
    fixture.token_endpoint.wait_for_request().await;
    let client_b_url = format!(
        "{}/api/config/config_entries/flow/{flow_id}",
        fixture.api_base
    );
    let mut second = tokio::spawn(async move {
        client_b
            .get(client_b_url)
            .bearer_auth(admin_b)
            .send()
            .await
            .unwrap()
    });
    assert!(
        timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err(),
        "parallel refresh shares the pending result"
    );
    assert_eq!(fixture.token_endpoint.request_count(), 1);
    fixture.token_endpoint.release.notify_one();

    let first = timeout(Duration::from_secs(4), first)
        .await
        .expect("first refresh response")
        .expect("first refresh task");
    let second = timeout(Duration::from_secs(4), second)
        .await
        .expect("parallel refresh response")
        .expect("parallel refresh task");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(fixture.token_endpoint.request_count(), 1);
    assert_eq!(fixture.config_entries.list_entries(Some(DOMAIN)).len(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn abort_during_blocked_exchange_prevents_late_entry_creation() {
    let mut fixture = Fixture::new(TokenMode::Block).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);
    let query = Fixture::callback_query_for(&state, Some(CALLBACK_CODE), None);
    assert_eq!(fixture.callback_query(&query).await.0, StatusCode::OK);
    let flow_id = start["flow_id"].as_str().unwrap().to_owned();

    let client = fixture.client.clone();
    let url = format!(
        "{}/api/config/config_entries/flow/{flow_id}",
        fixture.api_base
    );
    let admin = fixture.tokens.admin.clone();
    let getter = tokio::spawn(async move { client.get(url).bearer_auth(admin).send().await });
    fixture.token_endpoint.wait_for_request().await;
    assert_eq!(fixture.token_endpoint.request_count(), 1);

    let abort = fixture.abort_flow(&fixture.tokens.admin, &flow_id).await;
    assert_eq!(abort.status(), StatusCode::OK);
    fixture.token_endpoint.release.notify_one();
    timeout(
        Duration::from_secs(4),
        fixture.token_endpoint.finished.notified(),
    )
    .await
    .expect("blocked loopback token handler released");
    let response = timeout(Duration::from_secs(4), getter)
        .await
        .expect("aborted GET waiter exits")
        .expect("GET task completes")
        .expect("server answers the aborted waiter");
    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::CONFLICT | StatusCode::GONE
    ));
    assert!(fixture.config_entries.list_entries(Some(DOMAIN)).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn reauthentication_updates_the_same_entry_and_preserves_unrelated_data_and_options() {
    let mut fixture = Fixture::new(TokenMode::Success).await;
    let first_start = fixture.start_flow(&fixture.tokens.admin).await;
    let (first_state, _) = fixture.authorization_state(&first_start);
    let first_query = Fixture::callback_query_for(&first_state, Some(CALLBACK_CODE), None);
    assert_eq!(fixture.callback_query(&first_query).await.0, StatusCode::OK);
    let first_get = fixture
        .get_flow(
            &fixture.tokens.admin,
            first_start["flow_id"].as_str().unwrap(),
        )
        .await;
    assert_eq!(first_get.status(), StatusCode::OK);
    let initial = fixture
        .config_entries
        .list_entries(Some(DOMAIN))
        .into_iter()
        .next()
        .expect("initial OAuth entry");
    assert!(!initial.needs_reauth);
    let entry_id = initial.entry_id.clone();

    let reauth = fixture
        .oauth_host
        .manager()
        .begin_reauth(&entry_id)
        .await
        .expect("start backend-triggered reauth flow");
    let (reauth_state, _) = fixture.authorization_state(&reauth);
    let reauth_query = Fixture::callback_query_for(&reauth_state, Some(CALLBACK_CODE), None);
    assert_eq!(
        fixture.callback_query(&reauth_query).await.0,
        StatusCode::OK
    );
    let reauth_get = fixture
        .get_flow(&fixture.tokens.admin, reauth["flow_id"].as_str().unwrap())
        .await;
    assert_eq!(reauth_get.status(), StatusCode::OK);

    let updated = fixture
        .config_entries
        .get_entry(&entry_id)
        .expect("updated entry");
    assert_eq!(updated.entry_id, initial.entry_id);
    assert!(!updated.needs_reauth);
    assert_eq!(updated.data["unrelated_data"], "preserve-this-data");
    assert_eq!(updated.options["unrelated_option"], "preserve-this-option");
    assert_eq!(
        updated.data["provider_account_marker"],
        "private-account-sentinel"
    );
    assert_eq!(updated.data["token"]["access_token"], ACCESS_TOKEN);
    assert_eq!(fixture.token_endpoint.request_count(), 2);
    fixture.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_http_waiter_then_joins_blocked_host_resolution() {
    let mut fixture = Fixture::new(TokenMode::Block).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);
    let query = Fixture::callback_query_for(&state, Some(CALLBACK_CODE), None);
    assert_eq!(fixture.callback_query(&query).await.0, StatusCode::OK);
    let flow_id = start["flow_id"].as_str().unwrap().to_owned();

    let client = fixture.client.clone();
    let url = format!(
        "{}/api/config/config_entries/flow/{flow_id}",
        fixture.api_base
    );
    let admin = fixture.tokens.admin.clone();
    let getter = tokio::spawn(async move { client.get(url).bearer_auth(admin).send().await });
    fixture.token_endpoint.wait_for_request().await;

    fixture.shutdown_tx.send_replace(true);
    let response = timeout(Duration::from_secs(2), getter)
        .await
        .expect("shutdown releases the HTTP waiter")
        .expect("GET task completes")
        .expect("GET response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    timeout(Duration::from_secs(3), fixture.oauth_host.shutdown())
        .await
        .expect("OAuth host cancels and joins its resolver jobs");
    fixture.token_endpoint.release.notify_one();
    timeout(
        Duration::from_secs(4),
        fixture.token_endpoint.finished.notified(),
    )
    .await
    .expect("loopback token endpoint is released");
    assert!(fixture.config_entries.list_entries(Some(DOMAIN)).is_empty());

    if timeout(Duration::from_secs(2), &mut fixture.api_server)
        .await
        .is_err()
    {
        fixture.api_server.abort();
    }
    fixture.app_credentials.shutdown().await;
    fixture.token_server.abort();
    let _ = (&mut fixture.token_server).await;
}

#[tokio::test]
async fn token_endpoint_failure_is_sanitized_at_the_http_boundary() {
    let mut fixture = Fixture::new(TokenMode::Reject).await;
    let start = fixture.start_flow(&fixture.tokens.admin).await;
    let (state, _) = fixture.authorization_state(&start);
    let query = Fixture::callback_query_for(&state, Some(CALLBACK_CODE), None);
    assert_eq!(fixture.callback_query(&query).await.0, StatusCode::OK);

    let response = fixture
        .get_flow(&fixture.tokens.admin, start["flow_id"].as_str().unwrap())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("sanitized provider failure");
    let aborted: Value = serde_json::from_str(&body).expect("source-style abort result");
    assert_eq!(aborted["type"], "abort");
    assert_eq!(aborted["reason"], "oauth_failed");
    assert!(!body.contains(TOKEN_BODY_SENTINEL));
    assert!(!body.contains(CLIENT_SECRET));
    assert!(!body.contains(CALLBACK_CODE));
    assert_eq!(fixture.token_endpoint.request_count(), 1);
    assert!(fixture.config_entries.list_entries(Some(DOMAIN)).is_empty());
    fixture.shutdown().await;
}
