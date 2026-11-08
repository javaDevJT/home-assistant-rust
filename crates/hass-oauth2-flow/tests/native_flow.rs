use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use hass_application_credentials::{
    ApplicationCredentialsIntegration, ApplicationCredentialsPlatform, AuthorizationServer,
    CreateCredential,
};
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest,
    ConfigFlowResult, NewConfigEntry, SetupOutcome,
};
use hass_core::{
    Event, EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, Runtime,
    State as CoreState, Store,
};
use hass_oauth2_client::{Clock, OAuthClientFactory};
use hass_oauth2_flow::{
    ConfigEntryTokenStore, OAuthCallbackParams, OAuthEntryDetails, OAuthFlowError, OAuthFlowHost,
    OAuthFlowHostOptions, OAuthFlowProvider, OAuthPostAuthorizationContext,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use url::Url;

const DOMAIN: &str = "oauth_fixture";
const AUTH_DOMAIN: &str = "fixture-auth";
const CLIENT_SECRET: &str = "fixture-client-secret";
const TOKEN_MARKER: &str = "oauth-access-1";

#[derive(Clone, Default)]
struct TokenServer {
    forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
    gate_next_code: Arc<AtomicBool>,
    next_status: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl TokenServer {
    fn request_count(&self) -> usize {
        self.forms.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<HashMap<String, String>> {
        self.forms.lock().unwrap().clone()
    }

    fn fail_next_with_status(&self, status: u16) {
        self.next_status.store(status.into(), Ordering::Release);
    }
}

async fn token_endpoint(
    State(server): State<TokenServer>,
    body: String,
) -> (StatusCode, Json<Value>) {
    let form = url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<HashMap<_, _>>();
    server.forms.lock().unwrap().push(form.clone());
    if form.get("grant_type").map(String::as_str) == Some("authorization_code")
        && server.gate_next_code.swap(false, Ordering::AcqRel)
    {
        server.entered.notify_one();
        server.release.notified().await;
    }
    let failure_status = server.next_status.swap(0, Ordering::AcqRel);
    if failure_status != 0 {
        let status =
            StatusCode::from_u16(failure_status as u16).expect("test token status should be valid");
        return (
            status,
            Json(json!({
                "error": "server_error",
                "error_description": "synthetic-token-response-secret",
            })),
        );
    }
    let response = if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
        json!({
            "access_token": "oauth-access-refreshed",
            "expires_in": 3600,
            "provider_refresh_marker": "refresh-response",
        })
    } else {
        json!({
            "access_token": TOKEN_MARKER,
            "refresh_token": "oauth-refresh-1",
            "expires_in": 3600,
            "provider_extra": "preserve-me",
        })
    };
    (StatusCode::OK, Json(response))
}

#[derive(Default)]
struct ProviderHooks {
    block_next_post_authorize: AtomicBool,
    post_entered: Notify,
    post_release: Notify,
    post_authorize_calls: AtomicUsize,
    setup_calls: AtomicUsize,
    unload_calls: AtomicUsize,
    require_blocking_setup: AtomicBool,
    abort_reason: Mutex<Option<&'static str>>,
}

struct TestProvider {
    platform: ApplicationCredentialsPlatform,
    hooks: Arc<ProviderHooks>,
}

impl ConfigEntryDriver for TestProvider {
    fn domain(&self) -> &str {
        DOMAIN
    }

    fn version(&self) -> (u32, u32) {
        (2, 3)
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        if self.hooks.require_blocking_setup.load(Ordering::Acquire) {
            // Real synchronous integration drivers drive their async setup
            // through the runtime handle. This panics on a Tokio worker and
            // succeeds on its blocking pool (which also has a current handle).
            assert!(tokio::runtime::Handle::current().block_on(async {
                tokio::task::yield_now().await;
                true
            }));
        }
        self.hooks.setup_calls.fetch_add(1, Ordering::Relaxed);
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
        self.hooks.unload_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        Ok(ConfigFlowResult::form("auth", None))
    }
}

impl OAuthFlowProvider for TestProvider {
    fn application_credentials_platform(&self) -> ApplicationCredentialsPlatform {
        self.platform.clone()
    }

    fn oauth_scopes(&self, _request: &ConfigFlowRequest) -> Vec<String> {
        vec!["profile read".to_owned()]
    }

    fn post_authorize<'a>(
        &'a self,
        context: OAuthPostAuthorizationContext,
    ) -> BoxFuture<'a, Result<OAuthEntryDetails, OAuthFlowError>> {
        Box::pin(async move {
            self.hooks
                .post_authorize_calls
                .fetch_add(1, Ordering::Relaxed);
            if let Some(reason) = *self.hooks.abort_reason.lock().unwrap() {
                return Err(OAuthFlowError::ProviderAbort(reason));
            }
            if self
                .hooks
                .block_next_post_authorize
                .swap(false, Ordering::AcqRel)
            {
                self.hooks.post_entered.notify_one();
                self.hooks.post_release.notified().await;
            }
            let provider_token = context.token.to_value();
            let identity = provider_token
                .get("account_id")
                .cloned()
                .unwrap_or_else(|| json!("fixture-person"));
            let mut data = Map::new();
            data.insert("provider_profile".to_owned(), identity);
            data.insert("provider_unrelated".to_owned(), json!("updated"));
            let mut options = Map::new();
            options.insert("provider_option".to_owned(), json!("new"));
            Ok(OAuthEntryDetails::new(
                "OAuth Fixture",
                data,
                options,
                Some("fixture-person".to_owned()),
                (2, 3),
            ))
        })
    }
}

struct TestClock(AtomicI64);

impl TestClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }

    fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct CommitGate {
    armed: AtomicBool,
    entered: Notify,
    released: (Mutex<bool>, Condvar),
}

impl CommitGate {
    fn arm(&self) {
        *self.released.0.lock().unwrap() = false;
        self.armed.store(true, Ordering::Release);
    }

    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }

    fn block_if_token_commit(&self, key: &str, value: &Value) {
        if key != "hass.config_entries.v1"
            || !value.to_string().contains(TOKEN_MARKER)
            || !self.armed.swap(false, Ordering::AcqRel)
        {
            return;
        }
        self.entered.notify_one();
        let mut released = self.released.0.lock().unwrap();
        while !*released {
            released = self.released.1.wait(released).unwrap();
        }
    }
}

struct GateStore {
    inner: MemoryStore,
    gate: Arc<CommitGate>,
}

impl Store for GateStore {
    fn load_states(&self) -> hass_core::Result<Vec<CoreState>> {
        self.inner.load_states()
    }

    fn put_state(&self, state: &CoreState, event: Option<&Event>) -> hass_core::Result<()> {
        self.inner.put_state(state, event)
    }

    fn put_state_with_history(
        &self,
        state: &CoreState,
        history_state: &CoreState,
        event: &Event,
    ) -> hass_core::Result<()> {
        self.inner
            .put_state_with_history(state, history_state, event)
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> hass_core::Result<()> {
        self.inner.remove_state(entity_id, event)
    }

    fn append_event(&self, event: &Event) -> hass_core::Result<()> {
        self.inner.append_event(event)
    }

    fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
        self.inner.get_data(key)
    }

    fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
        self.gate.block_if_token_commit(key, value);
        self.inner.set_data(key, value)
    }

    fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
        self.inner.delete_data(key)
    }

    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> hass_core::Result<Vec<CoreState>> {
        self.inner.history(entity_id, start, end)
    }

    fn history_query(&self, query: &HistoryQuery) -> hass_core::Result<Vec<HistorySeries>> {
        self.inner.history_query(query)
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> hass_core::Result<Vec<Event>> {
        self.inner.event_history_query(query)
    }

    fn purge_before(&self, cutoff: DateTime<Utc>) -> hass_core::Result<PurgeResult> {
        self.inner.purge_before(cutoff)
    }

    fn repack(&self) -> hass_core::Result<()> {
        self.inner.repack()
    }
}

struct Fixture {
    host: Option<Arc<OAuthFlowHost>>,
    manager: hass_oauth2_flow::OAuthFlowManager,
    entries: ConfigEntries,
    app_credentials: ApplicationCredentialsIntegration,
    factory: OAuthClientFactory,
    provider: Arc<TestProvider>,
    provider_guard: Option<hass_oauth2_flow::OAuthFlowRegistrationGuard>,
    hooks: Arc<ProviderHooks>,
    clock: Arc<TestClock>,
    token_server: TokenServer,
    server_task: JoinHandle<()>,
    callback_url: Url,
}

impl Fixture {
    async fn new(flow_ttl: Duration) -> Self {
        Self::with_store(flow_ttl, Arc::new(MemoryStore::default())).await
    }

    async fn with_store(flow_ttl: Duration, store: Arc<dyn Store>) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let token_server = TokenServer::default();
        let app = Router::new()
            .route("/token", post(token_endpoint))
            .with_state(token_server.clone());
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let authorization_url = format!("http://{address}/authorize");
        let token_url = format!("http://{address}/token");
        let platform =
            ApplicationCredentialsPlatform::new().with_authorization_server_factory(move |_| {
                let authorization_url = authorization_url.clone();
                let token_url = token_url.clone();
                Box::pin(async move {
                    Ok(AuthorizationServer {
                        authorize_url: authorization_url,
                        token_url,
                    })
                })
            });
        let hooks = Arc::new(ProviderHooks::default());
        let provider = Arc::new(TestProvider {
            platform,
            hooks: hooks.clone(),
        });
        let runtime = Runtime::new(store).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let app_credentials =
            ApplicationCredentialsIntegration::new(runtime.clone(), entries.clone()).unwrap();
        let credentials = app_credentials.manager();
        let clock = Arc::new(TestClock::new(1_700_000_000));
        let factory = OAuthClientFactory::with_clock_without_proxy(
            Arc::new(ConfigEntryTokenStore::new(entries.clone())),
            clock.clone(),
        )
        .unwrap();
        let callback_url = Url::parse("http://127.0.0.1:8123/auth/external/callback").unwrap();
        let options = OAuthFlowHostOptions::new(callback_url.clone()).with_flow_ttl(flow_ttl);
        let host = Arc::new(
            OAuthFlowHost::new(
                entries.clone(),
                credentials.clone(),
                app_credentials.platform_registry(),
                factory.clone(),
                options,
            )
            .unwrap(),
        );
        let provider_guard = host.register_provider(provider.clone()).unwrap();
        credentials
            .create_admin(CreateCredential {
                domain: DOMAIN.to_owned(),
                client_id: "fixture-client-id".to_owned(),
                client_secret: CLIENT_SECRET.to_owned(),
                auth_domain: Some(AUTH_DOMAIN.to_owned()),
                name: Some("Loopback Fixture".to_owned()),
            })
            .unwrap();
        Self {
            manager: host.manager(),
            host: Some(host),
            entries,
            app_credentials,
            factory,
            provider,
            provider_guard: Some(provider_guard),
            hooks,
            clock,
            token_server,
            server_task,
            callback_url,
        }
    }

    async fn shutdown(mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown().await;
        }
        let _ = self.factory.shutdown().await;
        self.app_credentials.shutdown().await;
        self.server_task.abort();
    }
}

async fn start_flow(fixture: &Fixture) -> (String, String, Url) {
    let result = fixture
        .manager
        .begin(DOMAIN, "user", Map::new())
        .await
        .unwrap();
    assert_eq!(result["type"], "external");
    assert_eq!(result["step_id"], "auth");
    let flow_id = result["flow_id"].as_str().unwrap().to_owned();
    let authorization_url = Url::parse(result["url"].as_str().unwrap()).unwrap();
    let state = authorization_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    (flow_id, state, authorization_url)
}

async fn wait_for_provider_reregister(
    fixture: &Fixture,
) -> hass_oauth2_flow::OAuthFlowRegistrationGuard {
    let host = fixture.host.as_ref().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(2), async move {
        loop {
            match host.register_provider(fixture.provider.clone()) {
                Ok(guard) => return guard,
                Err(OAuthFlowError::ShuttingDown) => {
                    panic!("provider host unexpectedly stopped accepting registrations")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("provider and platform registration cleanup should complete")
}

async fn wait_until_provider_unloaded(manager: &hass_oauth2_flow::OAuthFlowManager) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.handles_domain(DOMAIN) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider removal should become visible before cleanup waits");
}

async fn finish_code_flow(fixture: &Fixture, flow_id: &str, state: &str) -> Value {
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state.to_owned(),
            Some("fixture-authorization-code".to_owned()),
            None,
        ))
        .await
        .unwrap();
    fixture.manager.get(flow_id).await.unwrap_or_else(|error| {
        panic!(
            "OAuth flow GET failed: {error:?}; entries={:?}; setup_calls={}",
            fixture.entries.list_entries(Some(DOMAIN)),
            fixture.hooks.setup_calls.load(Ordering::Relaxed)
        )
    })
}

fn entry_from_result(fixture: &Fixture, result: &Value) -> ConfigEntry {
    let entry_id = result["result"]["entry_id"].as_str().unwrap();
    fixture.entries.get_entry(entry_id).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_activation_runs_on_an_owned_blocking_thread() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    fixture
        .hooks
        .require_blocking_setup
        .store(true, Ordering::Release);
    let (flow_id, state, _) = start_flow(&fixture).await;
    let result = finish_code_flow(&fixture, &flow_id, &state).await;
    assert_eq!(result["type"], "create_entry");
    assert_eq!(result["result"]["state"], "loaded");
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Acquire), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_identity_abort_finishes_the_flow_without_persisting_a_token() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    *fixture.hooks.abort_reason.lock().unwrap() = Some("reauth_wrong_account");
    let (flow_id, state, _) = start_flow(&fixture).await;
    let result = finish_code_flow(&fixture, &flow_id, &state).await;
    assert_eq!(result["type"], "abort");
    assert_eq!(result["reason"], "reauth_wrong_account");
    assert!(fixture.entries.list_entries(Some(DOMAIN)).is_empty());
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Acquire), 0);
    assert_eq!(fixture.token_server.request_count(), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchanged_and_refreshed_private_tokens_survive_sqlite_runtime_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oauth.sqlite");
    let store = Arc::new(hass_storage::SqliteStore::open(&path).unwrap());
    let fixture = Fixture::with_store(Duration::from_secs(600), store).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    let result = finish_code_flow(&fixture, &flow_id, &state).await;
    let entry = entry_from_result(&fixture, &result);
    fixture.clock.advance(4_000);
    let session = fixture
        .manager
        .session_for_entry(DOMAIN, &entry.entry_id)
        .await
        .unwrap();
    // Build authorization only; no resource GET is sent to this callback URL.
    let request = reqwest::Client::new().get(fixture.callback_url.clone());
    let authorized = session.authorized_request(request).await.unwrap();
    assert_eq!(
        authorized.headers()[reqwest::header::AUTHORIZATION],
        "Bearer oauth-access-refreshed"
    );
    assert_eq!(fixture.token_server.request_count(), 2);
    let expected = fixture.entries.get_entry(&entry.entry_id).unwrap();
    drop(session);
    fixture.shutdown().await;
    let runtime = Runtime::new(Arc::new(hass_storage::SqliteStore::open(&path).unwrap())).unwrap();
    let reopened = ConfigEntries::new(runtime).unwrap();
    let actual = reopened.get_entry(&entry.entry_id).unwrap();
    assert_eq!(actual.data, expected.data);
    assert_eq!(actual.unique_id, expected.unique_id);
    assert!(
        !actual
            .public_json()
            .to_string()
            .contains("oauth-access-refreshed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callback_stages_external_done_then_get_commits_private_token_and_refreshes() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    let (flow_id, state, authorization_url) = start_flow(&fixture).await;
    let query = authorization_url.query_pairs().collect::<HashMap<_, _>>();
    assert_eq!(query["redirect_uri"], fixture.callback_url.as_str());
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "fixture-client-id");
    assert_eq!(query["scope"], "profile read");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query.contains_key("code_verifier"));

    let callback = fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state.clone(),
            Some("fixture-authorization-code".to_owned()),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(format!("{callback:?}"), "OAuthCallbackResponse");
    assert!(fixture.manager.flow_is_active(&flow_id).await);
    assert_eq!(fixture.token_server.request_count(), 0);
    let external_done = fixture.entries.flow_result(&flow_id).unwrap();
    assert_eq!(external_done["type"], "external_done");
    assert_eq!(external_done["step_id"], "creation");
    assert!(
        !external_done
            .to_string()
            .contains("fixture-authorization-code")
    );
    assert!(!external_done.to_string().contains(&state));

    let result = fixture.manager.get(&flow_id).await.unwrap();
    assert_eq!(result["type"], "create_entry");
    assert_eq!(fixture.token_server.request_count(), 1);
    let token_request = fixture.token_server.requests().pop().unwrap();
    assert_eq!(token_request["grant_type"], "authorization_code");
    assert_eq!(token_request["code"], "fixture-authorization-code");
    assert_eq!(token_request["client_id"], "fixture-client-id");
    assert_eq!(token_request["client_secret"], CLIENT_SECRET);
    assert_eq!(token_request["redirect_uri"], fixture.callback_url.as_str());
    assert!(token_request.contains_key("code_verifier"));

    let entry = entry_from_result(&fixture, &result);
    assert_eq!(entry.data["auth_implementation"], AUTH_DOMAIN);
    assert_eq!(entry.data["token"]["access_token"], TOKEN_MARKER);
    assert_eq!(entry.data["token"]["refresh_token"], "oauth-refresh-1");
    assert_eq!(entry.data["token"]["provider_extra"], "preserve-me");
    assert!(!result.to_string().contains(TOKEN_MARKER));
    assert!(!entry.public_json().to_string().contains(TOKEN_MARKER));

    fixture.clock.advance(4_000);
    let session = fixture
        .manager
        .session_for_entry(DOMAIN, &entry.entry_id)
        .await
        .unwrap();
    let request = reqwest::Client::new().get("http://127.0.0.1:9/protected-resource");
    let authorized = session.authorized_request(request).await.unwrap();
    assert_eq!(
        authorized.headers()[reqwest::header::AUTHORIZATION]
            .to_str()
            .unwrap(),
        "Bearer oauth-access-refreshed"
    );
    assert_eq!(fixture.token_server.request_count(), 2);
    let refreshed = fixture.entries.get_entry(&entry.entry_id).unwrap();
    assert_eq!(
        refreshed.data["token"]["access_token"],
        "oauth-access-refreshed"
    );
    assert_eq!(refreshed.data["token"]["refresh_token"], "oauth-refresh-1");
    assert_eq!(refreshed.data["token"]["provider_extra"], "preserve-me");
    assert_eq!(refreshed.data["provider_profile"], "fixture-person");
    assert_eq!(refreshed.data["auth_implementation"], AUTH_DOMAIN);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_endpoint_http_502_uses_source_oauth_failed_abort_reason() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    fixture.token_server.fail_next_with_status(502);
    let (flow_id, state, _) = start_flow(&fixture).await;

    let result = finish_code_flow(&fixture, &flow_id, &state).await;

    assert_eq!(result["type"], "abort");
    assert_eq!(result["reason"], "oauth_failed");
    assert!(
        !result
            .to_string()
            .contains("synthetic-token-response-secret")
    );
    assert!(!result.to_string().contains(CLIENT_SECRET));
    assert_eq!(fixture.token_server.request_count(), 1);
    assert!(fixture.entries.list_entries(Some(DOMAIN)).is_empty());
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reauth_merges_private_data_and_keeps_entry_identity_options_and_preferences() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    let mut data = Map::new();
    data.insert("auth_implementation".to_owned(), json!(AUTH_DOMAIN));
    data.insert(
        "token".to_owned(),
        json!({
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": 1_800_000_000,
            "provider_extra": "old-extra",
        }),
    );
    data.insert("unrelated_entry_data".to_owned(), json!("keep"));
    let mut seed = NewConfigEntry::new(DOMAIN, "Existing Account", data);
    seed.unique_id = Some("fixture-person".to_owned());
    seed.version = 2;
    seed.minor_version = 3;
    seed.options
        .insert("unrelated_option".to_owned(), json!("keep"));
    let original = fixture.entries.create_entry(seed).unwrap();
    fixture
        .entries
        .update_entry_preferences(&original.entry_id, None, Some(false), Some(true))
        .unwrap();

    let started = fixture
        .manager
        .begin_reauth(&original.entry_id)
        .await
        .unwrap();
    assert_eq!(started["type"], "external");
    let flow_id = started["flow_id"].as_str().unwrap().to_owned();
    let url = Url::parse(started["url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let result = finish_code_flow(&fixture, &flow_id, &state).await;
    assert_eq!(result["type"], "create_entry");
    let updated = fixture.entries.get_entry(&original.entry_id).unwrap();
    assert_eq!(updated.entry_id, original.entry_id);
    assert_eq!(updated.unique_id, original.unique_id);
    assert_eq!(updated.options["unrelated_option"], "keep");
    assert_eq!(updated.data["unrelated_entry_data"], "keep");
    assert_eq!(updated.data["provider_unrelated"], "updated");
    assert_eq!(updated.data["token"]["access_token"], TOKEN_MARKER);
    assert_eq!(updated.data["auth_implementation"], AUTH_DOMAIN);
    assert!(!updated.needs_reauth);
    assert!(!updated.pref_disable_new_entities);
    assert!(updated.pref_disable_polling);
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reauth_rejects_code_commit_after_a_newer_refresh_revision() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    let mut data = Map::new();
    data.insert("auth_implementation".to_owned(), json!(AUTH_DOMAIN));
    data.insert(
        "token".to_owned(),
        json!({
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": 1_700_003_600_i64,
            "provider_extra": "old-extra",
        }),
    );
    data.insert("unrelated_entry_data".to_owned(), json!("keep"));
    let mut seed = NewConfigEntry::new(DOMAIN, "Existing Account", data);
    seed.unique_id = Some("fixture-person".to_owned());
    seed.version = 2;
    seed.minor_version = 3;
    let original = fixture.entries.create_entry(seed).unwrap();

    let started = fixture
        .manager
        .begin_reauth(&original.entry_id)
        .await
        .unwrap();
    let flow_id = started["flow_id"].as_str().unwrap().to_owned();
    let url = Url::parse(started["url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("stale-code".to_owned()),
            None,
        ))
        .await
        .unwrap();

    fixture
        .hooks
        .block_next_post_authorize
        .store(true, Ordering::Release);
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.hooks.post_entered.notified(),
    )
    .await
    .expect("code exchange should finish before provider identity work");

    fixture.clock.advance(4_000);
    let current = fixture
        .manager
        .session_for_entry(DOMAIN, &original.entry_id)
        .await
        .unwrap();
    let refresh_request = reqwest::Client::new().get("http://127.0.0.1:9/refresh-check");
    current.authorized_request(refresh_request).await.unwrap();
    assert_eq!(fixture.token_server.request_count(), 2);

    fixture.hooks.post_release.notify_one();
    let result = get.await.unwrap().unwrap();
    assert_eq!(result["type"], "abort");
    assert_eq!(result["reason"], "oauth_error");
    let updated = fixture.entries.get_entry(&original.entry_id).unwrap();
    assert_eq!(updated.data["unrelated_entry_data"], "keep");
    assert_eq!(
        updated.data["token"]["access_token"],
        "oauth-access-refreshed"
    );
    assert_eq!(updated.data["token"]["refresh_token"], "old-refresh");
    assert_eq!(updated.data["token"]["provider_extra"], "old-extra");
    assert!(updated.needs_reauth);
    assert!(updated.data.get("provider_profile").is_none());
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_wrong_replayed_and_denied_callbacks_do_not_exchange_codes() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    let callback_debug = format!(
        "{:?}",
        OAuthCallbackParams::new(state.clone(), Some("sensitive-debug-code".to_owned()), None,)
    );
    assert!(!callback_debug.contains(&state));
    assert!(!callback_debug.contains("sensitive-debug-code"));
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(state.clone(), None, None))
            .await,
        Err(OAuthFlowError::InvalidCallback)
    ));
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                format!("{state}x"),
                Some("wrong-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::UnknownFlow)
    ));
    assert_eq!(fixture.token_server.request_count(), 0);

    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state.clone(),
            Some("valid-code".to_owned()),
            Some("access_denied".to_owned()),
        ))
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                state.clone(),
                Some("replayed-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::ReplayedCallback)
    ));
    assert_eq!(fixture.token_server.request_count(), 0);
    let first = fixture.manager.clone();
    let second = fixture.manager.clone();
    let (left, right) = tokio::join!(first.get(&flow_id), second.get(&flow_id));
    assert_eq!(left.unwrap()["type"], "create_entry");
    assert_eq!(right.unwrap()["type"], "create_entry");
    assert_eq!(fixture.token_server.request_count(), 1);

    let (denied_flow, denied_state, _) = start_flow(&fixture).await;
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            denied_state,
            None,
            Some("access_denied".to_owned()),
        ))
        .await
        .unwrap();
    let denied = fixture.manager.get(&denied_flow).await.unwrap();
    assert_eq!(denied["type"], "abort");
    assert_eq!(denied["reason"], "user_rejected_authorize");
    assert_eq!(fixture.token_server.request_count(), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_cancels_a_gated_code_exchange_without_creating_an_entry() {
    let fixture = Fixture::new(Duration::from_secs(600)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .token_server
        .gate_next_code
        .store(true, Ordering::Release);
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("gated-code".to_owned()),
            None,
        ))
        .await
        .unwrap();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    fixture.token_server.entered.notified().await;
    tokio::time::timeout(Duration::from_secs(2), fixture.manager.abort(&flow_id))
        .await
        .expect("abort should cancel and join the gated resolver")
        .unwrap();
    assert!(matches!(
        get.await.unwrap(),
        Err(OAuthFlowError::FlowAborted)
    ));
    fixture.token_server.release.notify_one();
    tokio::task::yield_now().await;
    assert!(fixture.entries.list_entries(Some(DOMAIN)).is_empty());
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_waits_for_admitted_private_commit_and_deferred_activation() {
    let gate = Arc::new(CommitGate::default());
    let store = Arc::new(GateStore {
        inner: MemoryStore::default(),
        gate: gate.clone(),
    });
    let fixture = Fixture::with_store(Duration::from_secs(600), store).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("committing-code".to_owned()),
            None,
        ))
        .await
        .unwrap();

    gate.arm();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("external flow should reach the private entry transaction");
    let manager = fixture.manager.clone();
    let abort_flow_id = flow_id.clone();
    let abort = tokio::spawn(async move { manager.abort(&abort_flow_id).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !abort.is_finished(),
        "abort must wait once the private commit is admitted"
    );

    gate.release();
    assert_eq!(get.await.unwrap().unwrap()["type"], "create_entry");
    abort.await.unwrap().unwrap();
    let entries = fixture.entries.list_entries(Some(DOMAIN));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].data["token"]["access_token"], TOKEN_MARKER);
    assert_eq!(entries[0].state, ConfigEntryState::Loaded);
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_waits_for_admitted_private_commit_and_deferred_activation() {
    let gate = Arc::new(CommitGate::default());
    let store = Arc::new(GateStore {
        inner: MemoryStore::default(),
        gate: gate.clone(),
    });
    let fixture = Fixture::with_store(Duration::from_secs(600), store).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("committing-code".to_owned()),
            None,
        ))
        .await
        .unwrap();

    gate.arm();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("external flow should reach the private entry transaction");
    let host = fixture.host.as_ref().unwrap().clone();
    let shutdown = tokio::spawn(async move { host.shutdown().await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for deferred activation after durable commit"
    );

    gate.release();
    assert_eq!(get.await.unwrap().unwrap()["type"], "create_entry");
    shutdown.await.unwrap();
    let entries = fixture.entries.list_entries(Some(DOMAIN));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].data["token"]["access_token"], TOKEN_MARKER);
    assert_eq!(entries[0].state, ConfigEntryState::Loaded);
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregister_cancels_gated_resolver_and_releases_provider() {
    let mut fixture = Fixture::new(Duration::from_secs(600)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .token_server
        .gate_next_code
        .store(true, Ordering::Release);
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("unregister-gated-code".to_owned()),
            None,
        ))
        .await
        .unwrap();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.token_server.entered.notified(),
    )
    .await
    .expect("resolver should enter the controlled token endpoint");

    let guard = fixture.provider_guard.take().unwrap();
    let unregister = tokio::spawn(async move { guard.unregister().await });
    wait_until_provider_unloaded(&fixture.manager).await;
    unregister
        .await
        .expect("unregister should finish after canceling the pre-commit resolver");

    fixture.token_server.release.notify_one();
    assert!(matches!(
        get.await.unwrap(),
        Err(OAuthFlowError::FlowAborted)
    ));
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    fixture.provider_guard = Some(wait_for_provider_reregister(&fixture).await);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_unregister_waiter_joins_private_commit_before_driver_cleanup() {
    let gate = Arc::new(CommitGate::default());
    let store = Arc::new(GateStore {
        inner: MemoryStore::default(),
        gate: gate.clone(),
    });
    let mut fixture = Fixture::with_store(Duration::from_secs(600), store).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("unregister-committing-code".to_owned()),
            None,
        ))
        .await
        .unwrap();

    gate.arm();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("resolver should enter the durable private commit");

    let guard = fixture.provider_guard.take().unwrap();
    let unregister = tokio::spawn(async move { guard.unregister().await });
    wait_until_provider_unloaded(&fixture.manager).await;
    assert!(
        fixture
            .host
            .as_ref()
            .unwrap()
            .register_provider(fixture.provider.clone())
            .is_err(),
        "platform guard and Core driver must remain until the commit finishes"
    );
    unregister.abort();
    assert!(unregister.await.unwrap_err().is_cancelled());

    gate.release();
    let result = get.await.unwrap().unwrap();
    assert_eq!(result["type"], "create_entry");
    let entries = fixture.entries.list_entries(Some(DOMAIN));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, ConfigEntryState::Loaded);
    assert_eq!(fixture.hooks.setup_calls.load(Ordering::Relaxed), 1);
    fixture.provider_guard = Some(wait_for_provider_reregister(&fixture).await);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_shutdown_waiter_preserves_cleanup_and_concurrent_callers_join() {
    let gate = Arc::new(CommitGate::default());
    let store = Arc::new(GateStore {
        inner: MemoryStore::default(),
        gate: gate.clone(),
    });
    let fixture = Fixture::with_store(Duration::from_secs(600), store).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    fixture
        .manager
        .callback(OAuthCallbackParams::new(
            state,
            Some("shutdown-committing-code".to_owned()),
            None,
        ))
        .await
        .unwrap();

    gate.arm();
    let manager = fixture.manager.clone();
    let get_flow_id = flow_id.clone();
    let get = tokio::spawn(async move { manager.get(&get_flow_id).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("resolver should enter the durable private commit");

    let host = fixture.host.as_ref().unwrap().clone();
    let canceled_shutdown_host = host.clone();
    let canceled_shutdown = tokio::spawn(async move { canceled_shutdown_host.shutdown().await });
    wait_until_provider_unloaded(&fixture.manager).await;
    let joined_shutdown_host = host.clone();
    let joined_shutdown = tokio::spawn(async move { joined_shutdown_host.shutdown().await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!canceled_shutdown.is_finished());
    assert!(!joined_shutdown.is_finished());
    canceled_shutdown.abort();
    assert!(canceled_shutdown.await.unwrap_err().is_cancelled());

    gate.release();
    assert_eq!(get.await.unwrap().unwrap()["type"], "create_entry");
    joined_shutdown.await.unwrap();
    host.shutdown().await;
    assert!(!fixture.manager.handles_domain(DOMAIN));
    assert!(!fixture.manager.owns_flow(&flow_id));

    let replacement_host = Arc::new(
        OAuthFlowHost::new(
            fixture.entries.clone(),
            fixture.app_credentials.manager(),
            fixture.app_credentials.platform_registry(),
            fixture.factory.clone(),
            OAuthFlowHostOptions::new(fixture.callback_url.clone()),
        )
        .unwrap(),
    );
    let replacement_guard = replacement_host
        .register_provider(fixture.provider.clone())
        .expect("shutdown must release the exact driver and platform guard");
    replacement_guard.unregister().await;
    replacement_host.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_prunes_flow_and_releases_sdk_verifier_and_provider_can_reregister() {
    let mut fixture = Fixture::new(Duration::from_secs(1)).await;
    let (expired_flow, expired_state, _) = start_flow(&fixture).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!fixture.manager.flow_is_active(&expired_flow).await);
    assert!(!fixture.manager.owns_flow(&expired_flow));
    assert!(fixture.entries.flow_result(&expired_flow).is_err());
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                expired_state,
                Some("expired-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::UnknownFlow)
    ));
    assert_eq!(fixture.token_server.request_count(), 0);

    let (old_flow, old_state, _) = start_flow(&fixture).await;
    fixture.provider_guard.take().unwrap().unregister().await;
    assert!(!fixture.manager.handles_domain(DOMAIN));
    assert!(!fixture.manager.owns_flow(&old_flow));
    assert!(fixture.entries.flow_result(&old_flow).is_err());
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                old_state,
                Some("unregistered-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::UnknownFlow)
    ));

    fixture.provider_guard = Some(
        fixture
            .host
            .as_ref()
            .unwrap()
            .register_provider(fixture.provider.clone())
            .unwrap(),
    );
    assert!(fixture.manager.handles_domain(DOMAIN));
    let (flow_id, state, _) = start_flow(&fixture).await;
    let result = finish_code_flow(&fixture, &flow_id, &state).await;
    assert_eq!(result["type"], "create_entry");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_get_expires_flow_without_a_later_begin() {
    let fixture = Fixture::new(Duration::from_millis(50)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    tokio::time::sleep(Duration::from_millis(75)).await;

    assert!(matches!(
        fixture.manager.get(&flow_id).await,
        Err(OAuthFlowError::ExpiredFlow)
    ));
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                state,
                Some("idle-get-expired-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::UnknownFlow)
    ));
    assert_eq!(fixture.token_server.request_count(), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_picker_submit_does_not_start_an_authorization_url() {
    let fixture = Fixture::new(Duration::from_millis(50)).await;
    fixture
        .app_credentials
        .manager()
        .create_admin(CreateCredential {
            domain: DOMAIN.to_owned(),
            client_id: "fixture-client-id-alt".to_owned(),
            client_secret: "fixture-client-secret-alt".to_owned(),
            auth_domain: Some("fixture-auth-alt".to_owned()),
            name: Some("Loopback Fixture Alternate".to_owned()),
        })
        .unwrap();
    let picker = fixture
        .manager
        .begin(DOMAIN, "user", Map::new())
        .await
        .unwrap();
    assert_eq!(picker["type"], "form");
    assert_eq!(picker["step_id"], "auth_implementation");
    let flow_id = picker["flow_id"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(75)).await;
    let user_input = json!({"auth_implementation": "fixture-auth-alt"})
        .as_object()
        .unwrap()
        .clone();

    assert!(matches!(
        fixture.manager.submit(&flow_id, user_input).await,
        Err(OAuthFlowError::ExpiredFlow)
    ));
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    assert_eq!(fixture.token_server.request_count(), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_picker_config_admission_aborts_when_core_submits_directly() {
    let fixture = Fixture::new(Duration::from_millis(50)).await;
    fixture
        .app_credentials
        .manager()
        .create_admin(CreateCredential {
            domain: DOMAIN.to_owned(),
            client_id: "fixture-client-id-alt".to_owned(),
            client_secret: "fixture-client-secret-alt".to_owned(),
            auth_domain: Some("fixture-auth-alt".to_owned()),
            name: Some("Loopback Fixture Alternate".to_owned()),
        })
        .unwrap();
    let picker = fixture
        .manager
        .begin(DOMAIN, "user", Map::new())
        .await
        .unwrap();
    assert_eq!(picker["type"], "form");
    let flow_id = picker["flow_id"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(75)).await;
    let user_input = json!({"auth_implementation": "fixture-auth-alt"})
        .as_object()
        .unwrap()
        .clone();

    let result = fixture
        .entries
        .submit_flow_async(&flow_id, user_input)
        .await
        .unwrap();
    assert_eq!(result["type"], "abort");
    assert_eq!(result["reason"], "oauth_timeout");
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert_eq!(fixture.token_server.request_count(), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_callback_cancels_idle_authorization_without_a_later_begin() {
    let fixture = Fixture::new(Duration::from_millis(50)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    tokio::time::sleep(Duration::from_millis(75)).await;

    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                state,
                Some("expired-callback-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::ExpiredFlow)
    ));
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    assert_eq!(fixture.token_server.request_count(), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_expiry_cycles_keep_same_provider_usable() {
    let fixture = Fixture::new(Duration::from_millis(60)).await;
    for _ in 0..12 {
        let (flow_id, _, _) = start_flow(&fixture).await;
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(matches!(
            fixture.manager.get(&flow_id).await,
            Err(OAuthFlowError::ExpiredFlow)
        ));
        assert!(!fixture.manager.owns_flow(&flow_id));
    }

    let (flow_id, state, _) = start_flow(&fixture).await;
    assert_eq!(
        finish_code_flow(&fixture, &flow_id, &state).await["type"],
        "create_entry"
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_host_invalidates_weak_manager_and_aborts_pending_sdk_flow() {
    let mut fixture = Fixture::new(Duration::from_secs(600)).await;
    let (flow_id, state, _) = start_flow(&fixture).await;
    let host = fixture.host.take().unwrap();
    drop(host);
    assert!(!fixture.manager.owns_flow(&flow_id));
    assert!(!fixture.manager.handles_domain(DOMAIN));
    assert!(matches!(
        fixture.manager.get(&flow_id).await,
        Err(OAuthFlowError::ShuttingDown)
    ));
    assert!(matches!(
        fixture
            .manager
            .callback(OAuthCallbackParams::new(
                state,
                Some("after-drop-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthFlowError::ShuttingDown)
    ));
    assert!(fixture.entries.flow_result(&flow_id).is_err());
    assert_eq!(fixture.token_server.request_count(), 0);
    fixture.shutdown().await;
}
