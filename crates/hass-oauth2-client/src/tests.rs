use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const NOW: i64 = 1_700_000_000;

#[derive(Clone)]
struct FakeClock(Arc<AtomicI64>);

impl FakeClock {
    fn new(now: i64) -> Self {
        Self(Arc::new(AtomicI64::new(now)))
    }

    fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct MemoryStore {
    values: tokio::sync::Mutex<HashMap<String, Value>>,
    fail_load: AtomicBool,
    fail_save: AtomicBool,
    saves: AtomicUsize,
}

impl MemoryStore {
    async fn value(&self, account_id: &str) -> Option<Value> {
        self.values.lock().await.get(account_id).cloned()
    }

    async fn put(&self, account_id: &str, value: Value) {
        self.values
            .lock()
            .await
            .insert(account_id.to_owned(), value);
    }
}

impl OAuthTokenStore for MemoryStore {
    fn load<'a>(
        &'a self,
        account_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, TokenStoreError>> {
        Box::pin(async move {
            if self.fail_load.load(Ordering::SeqCst) {
                return Err(TokenStoreError::Load);
            }
            Ok(self.values.lock().await.get(account_id).cloned())
        })
    }

    fn store<'a>(
        &'a self,
        account_id: &'a str,
        provider_token: Value,
    ) -> BoxFuture<'a, Result<(), TokenStoreError>> {
        Box::pin(async move {
            if self.fail_save.load(Ordering::SeqCst) {
                return Err(TokenStoreError::Save);
            }
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.values
                .lock()
                .await
                .insert(account_id.to_owned(), provider_token);
            Ok(())
        })
    }
}

#[derive(Default)]
struct CommitGateStore {
    inner: MemoryStore,
    block_next_store: AtomicBool,
    store_entered: Notify,
    release_store: Notify,
}

impl CommitGateStore {
    async fn wait_for_blocked_store(&self) {
        self.store_entered.notified().await;
    }

    fn release_blocked_store(&self) {
        self.release_store.notify_one();
    }
}

impl OAuthTokenStore for CommitGateStore {
    fn load<'a>(
        &'a self,
        account_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, TokenStoreError>> {
        Box::pin(async move { self.inner.load(account_id).await })
    }

    fn store<'a>(
        &'a self,
        account_id: &'a str,
        provider_token: Value,
    ) -> BoxFuture<'a, Result<(), TokenStoreError>> {
        Box::pin(async move {
            if self.block_next_store.swap(false, Ordering::SeqCst) {
                self.store_entered.notify_one();
                self.release_store.notified().await;
            }
            self.inner.store(account_id, provider_token).await
        })
    }
}

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    headers: HashMap<String, String>,
    body: String,
}

async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(
            read > 0,
            "connection closed before the request headers arrived"
        );
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end - 4]);
    let mut lines = header_text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    let mut content_length = 0_usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    while bytes.len().saturating_sub(header_end) < content_length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(
            read > 0,
            "connection closed before the request body arrived"
        );
        bytes.extend_from_slice(&chunk[..read]);
    }
    CapturedRequest {
        method,
        headers,
        body: String::from_utf8_lossy(
            &bytes[header_end..header_end.saturating_add(content_length)],
        )
        .into_owned(),
    }
}

async fn serve_once(
    status: u16,
    headers: Vec<String>,
    response_body: impl Into<String>,
    delay: Duration,
) -> (Url, JoinHandle<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = Url::parse(&format!("http://{address}/token?route=local")).unwrap();
    let response_body = response_body.into();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let reason = match status {
            200 => "OK",
            302 => "Found",
            400 => "Bad Request",
            401 => "Unauthorized",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Fixture",
        };
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response_body.len()
        );
        for header in headers {
            response.push_str(&header);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(response_body.as_bytes()).await;
        let _ = stream.flush().await;
        request
    });
    (url, task)
}

async fn serve_two(listener: TcpListener) -> JoinHandle<Vec<CapturedRequest>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            requests.push(request);
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
            let _ = stream.write_all(response).await;
        }
        requests
    })
}

fn memory_factory(store: Arc<MemoryStore>, clock: Arc<FakeClock>) -> OAuthClientFactory {
    OAuthClientFactory::with_clock_without_proxy(store, clock).unwrap()
}

fn test_config(token_endpoint: Url) -> OAuthClientConfig {
    OAuthClientConfig::new(
        "test client +&",
        Some("dummy-client-secret".to_owned()),
        Url::parse("https://oauth.example.test/authorize?tenant=home%20assistant").unwrap(),
        token_endpoint,
    )
    .unwrap()
}

fn state_from(step: &AuthorizationStep) -> String {
    step.authorization_url()
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned()
}

fn form_params(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

#[test]
fn server_endpoints_require_https_except_literal_loopback_http() {
    let secure_authorization = Url::parse("https://oauth.example.test/authorize").unwrap();
    let secure_token = Url::parse("https://oauth.example.test/token").unwrap();

    assert_eq!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("http://oauth.example.test/authorize").unwrap(),
            secure_token.clone(),
        )
        .unwrap_err(),
        OAuthError::InsecureEndpoint
    );
    assert_eq!(
        OAuthClientConfig::new(
            "client",
            None,
            secure_authorization.clone(),
            Url::parse("http://localhost/token").unwrap(),
        )
        .unwrap_err(),
        OAuthError::InsecureEndpoint
    );
    assert_eq!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("https://user:password@oauth.example.test/authorize").unwrap(),
            secure_token.clone(),
        )
        .unwrap_err(),
        OAuthError::InvalidEndpoint
    );
    assert_eq!(
        OAuthClientConfig::new(
            "client",
            None,
            secure_authorization,
            Url::parse("https://oauth.example.test/token#fragment").unwrap(),
        )
        .unwrap_err(),
        OAuthError::InvalidEndpoint
    );

    OAuthClientConfig::new(
        "client",
        None,
        Url::parse("http://127.0.0.1:8000/authorize").unwrap(),
        Url::parse("http://[::1]:8001/token").unwrap(),
    )
    .unwrap();
}

#[test]
fn authorization_endpoint_rejects_preexisting_reserved_oauth_parameters() {
    for name in [
        "state",
        "client_id",
        "redirect_uri",
        "response_type",
        "scope",
        "code_challenge",
        "code_challenge_method",
    ] {
        let endpoint = Url::parse(&format!(
            "https://oauth.example.test/authorize?tenant=one&{name}=configured"
        ))
        .unwrap();
        assert_eq!(
            OAuthClientConfig::new(
                "client",
                None,
                endpoint,
                Url::parse("https://oauth.example.test/token").unwrap(),
            )
            .unwrap_err(),
            OAuthError::ReservedAuthorizationParameter,
            "reserved endpoint query key: {name}"
        );
    }

    let encoded = OAuthClientConfig::new(
        "client",
        None,
        Url::parse("https://oauth.example.test/authorize?%73tate=configured").unwrap(),
        Url::parse("https://oauth.example.test/token").unwrap(),
    );
    assert_eq!(
        encoded.unwrap_err(),
        OAuthError::ReservedAuthorizationParameter
    );
}

#[tokio::test]
async fn authorized_resource_requests_reject_remote_cleartext_urls() {
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "entry-http-resource",
            json!({"access_token":"resource-access","expires_at":NOW + 3600}),
        )
        .await;
    let factory = memory_factory(store, Arc::new(FakeClock::new(NOW)));
    let client = factory
        .client(test_config(
            Url::parse("https://oauth.example.test/token").unwrap(),
        ))
        .unwrap();
    let session = client.session("entry-http-resource").unwrap();
    assert_eq!(
        session
            .authorized_request(HttpClient::new().get("http://api.example.test/data"))
            .await
            .unwrap_err(),
        OAuthError::InsecureEndpoint
    );

    let request = session
        .authorized_request(HttpClient::new().get("http://127.0.0.1:8123/data"))
        .await
        .unwrap();
    assert_eq!(request.url().host_str(), Some("127.0.0.1"));
    assert_eq!(request.headers()[AUTHORIZATION], "Bearer resource-access");
    assert!(request.headers()[AUTHORIZATION].is_sensitive());
}

#[tokio::test]
async fn authorization_url_preserves_endpoint_query_and_encodes_extensions_once() {
    let store = Arc::new(MemoryStore::default());
    let clock = Arc::new(FakeClock::new(NOW));
    let factory = memory_factory(store, clock);
    let config = test_config(Url::parse("http://127.0.0.1:42424/token?tenant=local").unwrap())
        .with_authorization_parameters(vec![
            ("audience".to_owned(), "a b&c".to_owned()),
            ("resource".to_owned(), "one".to_owned()),
            ("resource".to_owned(), "two+two".to_owned()),
        ])
        .unwrap();
    let client = factory.client(config).unwrap();
    let redirect =
        Url::parse("https://frontend.example.test/auth%20external%2B%26/callback").unwrap();
    let step = client
        .start_authorization(
            AuthorizationRequest::new(redirect.clone()).with_scopes(vec![
                "read".to_owned(),
                "profile".to_owned(),
                "speaker:read+light".to_owned(),
            ]),
        )
        .await
        .unwrap();

    let pairs = step
        .authorization_url()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    assert!(pairs.contains(&("tenant".to_owned(), "home assistant".to_owned())));
    assert!(pairs.contains(&("response_type".to_owned(), "code".to_owned())));
    assert!(pairs.contains(&("client_id".to_owned(), "test client +&".to_owned())));
    assert!(pairs.contains(&("redirect_uri".to_owned(), redirect.to_string())));
    assert!(pairs.contains(&(
        "scope".to_owned(),
        "read profile speaker:read+light".to_owned()
    )));
    assert!(pairs.contains(&("audience".to_owned(), "a b&c".to_owned())));
    assert_eq!(
        pairs
            .iter()
            .filter(|(key, _)| key == "resource")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>(),
        ["one", "two+two"]
    );

    let id = step.flow_id().0;
    {
        let flows = client.inner.flows.lock().unwrap();
        let first = flows.pending.get(&id).unwrap();
        assert_eq!(first.verifier.0.len(), 128);
        assert_eq!(
            pairs
                .iter()
                .find(|(key, _)| key == "code_challenge")
                .unwrap()
                .1,
            pkce_challenge(&first.verifier.0)
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(key, _)| key == "code_challenge_method")
                .unwrap()
                .1,
            "S256"
        );
    }

    let second = client
        .start_authorization(AuthorizationRequest::new(redirect))
        .await
        .unwrap();
    let second_id = second.flow_id().0;
    let flows = client.inner.flows.lock().unwrap();
    assert_ne!(
        flows.pending.get(&id).unwrap().verifier.0,
        flows.pending.get(&second_id).unwrap().verifier.0
    );
    assert_ne!(state_from(&step), state_from(&second));
    assert!(!format!("{step:?}").contains(&state_from(&step)));
}

#[test]
fn pkce_matches_rfc7636_known_vector_and_verifier_bounds() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("https://auth.example.test/authorize").unwrap(),
            Url::parse("https://auth.example.test/token").unwrap(),
        )
        .unwrap()
        .with_pkce_verifier_length(42)
        .is_err()
    );
    assert!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("https://auth.example.test/authorize").unwrap(),
            Url::parse("https://auth.example.test/token").unwrap(),
        )
        .unwrap()
        .with_pkce_verifier_length(129)
        .is_err()
    );
    assert!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("https://auth.example.test/authorize").unwrap(),
            Url::parse("https://auth.example.test/token").unwrap(),
        )
        .unwrap()
        .with_pkce_verifier_length(43)
        .is_ok()
    );
    assert!(
        OAuthClientConfig::new(
            "client",
            None,
            Url::parse("https://auth.example.test/authorize").unwrap(),
            Url::parse("https://auth.example.test/token").unwrap(),
        )
        .unwrap()
        .with_pkce_verifier_length(128)
        .is_ok()
    );
}

#[tokio::test]
async fn callback_state_forgery_denial_expiry_and_replay_are_typed() {
    let store = Arc::new(MemoryStore::default());
    let clock = Arc::new(FakeClock::new(NOW));
    let factory = memory_factory(store, clock.clone());
    let config = test_config(Url::parse("http://127.0.0.1:42424/token").unwrap())
        .with_flow_ttl(Duration::from_secs(30))
        .unwrap();
    let client = factory.client(config).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://frontend.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let state = state_from(&step);

    let mut parts = state.split('.').map(str::to_owned).collect::<Vec<_>>();
    let first = parts[2].as_bytes()[0];
    parts[2].replace_range(0..1, if first == b'A' { "B" } else { "A" });
    let forged = parts.join(".");
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                forged,
                Some("must-not-exchange".to_owned()),
                None,
            ))
            .await,
        Err(OAuthError::InvalidState)
    );

    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(state.clone(), None, None))
            .await,
        Err(OAuthError::InvalidCallback)
    );
    let other_flow = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://frontend.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client
            .complete_callback_for(
                other_flow.flow_id(),
                OAuthCallback::new(state.clone(), Some("must-not-exchange".to_owned()), None,),
            )
            .await,
        Err(OAuthError::InvalidState)
    );
    let verified = client.verify_callback_state(&state).await.unwrap();
    assert_eq!(
        verified.flow_id().as_uuid_string(),
        step.flow_id().as_uuid_string()
    );
    assert!(!format!("{verified:?}").contains(&state));
    let result = client
        .complete_verified_callback(
            verified,
            OAuthCallbackPayload::new(None, Some("access_denied-secret".to_owned())),
        )
        .await
        .unwrap();
    assert!(matches!(result, OAuthCallbackResult::Denied { .. }));
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                state,
                None,
                Some("access_denied".to_owned()),
            ))
            .await,
        Err(OAuthError::ReplayedCallback)
    );

    let expired = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://frontend.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let expired_state = state_from(&expired);
    clock.set(NOW + 30);
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                expired_state,
                None,
                Some("access_denied".to_owned()),
            ))
            .await,
        Err(OAuthError::ExpiredFlow)
    );
}

#[tokio::test]
async fn code_exchange_uses_basic_auth_form_pkce_and_trusted_redirect_uri() {
    let secret = "dummy secret:+/&";
    let client_id = "client id:+/&";
    let (token_url, request_task) = serve_once(
        200,
        vec![],
        r#"{"access_token":"access-one","refresh_token":"refresh-one","expires_in":3600,"provider_extra":{"region":"local"}}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    let clock = Arc::new(FakeClock::new(NOW));
    let factory = memory_factory(store.clone(), clock);
    let config = OAuthClientConfig::new(
        client_id,
        Some(secret.to_owned()),
        Url::parse("https://oauth.example.test/authorize?tenant=x").unwrap(),
        token_url,
    )
    .unwrap()
    .with_authentication_method(ClientAuthenticationMethod::ClientSecretBasic);
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains(client_id));
    assert!(!config_debug.contains(secret));
    let client = factory.client(config).unwrap();
    let redirect = Url::parse("https://trusted.example.test/auth/external/callback").unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(redirect.clone()))
        .await
        .unwrap();
    let state = state_from(&step);
    let verifier = client
        .inner
        .flows
        .lock()
        .unwrap()
        .pending
        .get(&step.flow_id().0)
        .unwrap()
        .verifier
        .0
        .clone();
    let callback = OAuthCallback::new(state, Some("authorization-code-secret".to_owned()), None);
    let callback_debug = format!("{callback:?}");
    assert!(!callback_debug.contains("authorization-code-secret"));
    assert!(!callback_debug.contains("access-one"));
    let result = client.complete_callback(callback).await.unwrap();
    let OAuthCallbackResult::Authorized { token, .. } = result else {
        panic!("expected token response");
    };
    let request = request_task.await.unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some("application/x-www-form-urlencoded")
    );
    let basic = request.headers.get("authorization").unwrap();
    let basic = basic.strip_prefix("Basic ").unwrap();
    let decoded = STANDARD.decode(basic).unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        format!("{}:{}", form_component(client_id), form_component(secret))
    );
    assert!(!request.body.contains(secret));
    let params = form_params(&request.body);
    assert!(!params.iter().any(|(key, _)| key == "client_id"));
    assert!(!params.iter().any(|(key, _)| key == "client_secret"));
    assert!(params.contains(&("grant_type".to_owned(), "authorization_code".to_owned())));
    assert!(params.contains(&("code".to_owned(), "authorization-code-secret".to_owned())));
    assert!(params.contains(&("redirect_uri".to_owned(), redirect.to_string())));
    assert!(params.contains(&("code_verifier".to_owned(), verifier)));
    assert_eq!(token.expires_at_unix(), NOW + 3600);
    assert_eq!(token.to_value()["provider_extra"]["region"], "local");
    assert!(!format!("{token:?}").contains("access-one"));
    assert!(!format!("{token:?}").contains("refresh-one"));

    let session = client.session("entry-1").unwrap();
    session.save_token(&token).await.unwrap();
    assert_eq!(
        store.value("entry-1").await.unwrap()["provider_extra"]["region"],
        "local"
    );
    assert_eq!(
        session.save_token(&token).await,
        Err(OAuthError::CredentialConflict)
    );
}

#[tokio::test]
async fn code_and_error_together_follow_source_code_precedence() {
    let (token_url, request_task) = serve_once(
        200,
        vec![],
        r#"{"access_token":"access","expires_in":60}"#,
        Duration::ZERO,
    )
    .await;
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let client = factory.client(test_config(token_url)).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let result = client
        .complete_callback(OAuthCallback::new(
            state_from(&step),
            Some("accepted-code".to_owned()),
            Some("ignored-error".to_owned()),
        ))
        .await
        .unwrap();
    assert!(matches!(result, OAuthCallbackResult::Authorized { .. }));
    let captured = request_task.await.unwrap();
    assert!(form_params(&captured.body).contains(&("code".to_owned(), "accepted-code".to_owned())));
}

#[tokio::test]
async fn token_post_redirect_is_not_followed_or_forwarded() {
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_url = format!("http://{}/sink", destination.local_addr().unwrap());
    let no_forward = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(200), destination.accept())
            .await
            .is_err()
    });
    let (token_url, first_request) = serve_once(
        302,
        vec![format!("Location: {destination_url}")],
        "redirect body must not be logged",
        Duration::ZERO,
    )
    .await;
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let client = factory.client(test_config(token_url)).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                state_from(&step),
                Some("dummy-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthError::ProviderError)
    );
    let first_request = first_request.await.unwrap();
    assert!(
        form_params(&first_request.body).contains(&("code".to_owned(), "dummy-code".to_owned()))
    );
    assert!(no_forward.await.unwrap());
}

#[tokio::test]
async fn refresh_updates_the_shared_store_once_and_both_handles_use_new_bearer() {
    let (token_url, token_request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"new-access","expires_in":3600,"provider_new":"kept"}"#,
        Duration::from_millis(30),
    )
    .await;
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = Url::parse(&format!(
        "http://{}/api",
        api_listener.local_addr().unwrap()
    ))
    .unwrap();
    let api_requests = serve_two(api_listener).await;
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "entry-shared",
            json!({
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "expires_at": NOW + EXPIRY_SKEW_SECONDS,
                "provider_old": {"keep": true}
            }),
        )
        .await;
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let config = test_config(token_url);
    let client_a = factory.client(config.clone()).unwrap();
    let client_b = factory.client(config).unwrap();
    let session_a = client_a.session("entry-shared").unwrap();
    let session_b = client_b.session("entry-shared").unwrap();
    let api_http = HttpClient::builder()
        .no_proxy()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let (request_a, request_b) = tokio::join!(
        session_a.authorized_request(api_http.get(api_url.clone())),
        session_b.authorized_request(api_http.get(api_url.clone()))
    );
    let request_a = request_a.unwrap();
    let request_b = request_b.unwrap();
    let (response_a, response_b) =
        tokio::join!(api_http.execute(request_a), api_http.execute(request_b));
    assert!(response_a.unwrap().status().is_success());
    assert!(response_b.unwrap().status().is_success());
    let refresh = token_request.await.unwrap();
    let refresh_params = form_params(&refresh.body);
    assert!(refresh_params.contains(&("grant_type".to_owned(), "refresh_token".to_owned())));
    assert!(refresh_params.contains(&("refresh_token".to_owned(), "old-refresh".to_owned())));
    let api = api_requests.await.unwrap();
    assert_eq!(api.len(), 2);
    assert!(api.iter().all(|request| {
        request
            .headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer new-access")
    }));
    let saved = store.value("entry-shared").await.unwrap();
    assert_eq!(saved["access_token"], "new-access");
    assert_eq!(saved["refresh_token"], "old-refresh");
    assert_eq!(saved["provider_old"]["keep"], true);
    assert_eq!(saved["provider_new"], "kept");
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refresh_accepts_a_rotated_refresh_token() {
    let (token_url, token_request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "entry-rotated",
            json!({
                "access_token":"old-access",
                "refresh_token":"old-refresh",
                "expires_at":NOW
            }),
        )
        .await;
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let client = factory.client(test_config(token_url)).unwrap();
    let session = client.session("entry-rotated").unwrap();
    let request = session
        .authorized_request(HttpClient::new().get("http://127.0.0.1:9/api"))
        .await
        .unwrap();
    assert_eq!(
        request.headers().get(AUTHORIZATION).unwrap(),
        "Bearer rotated-access"
    );
    let sent = token_request.await.unwrap();
    assert!(
        form_params(&sent.body).contains(&("refresh_token".to_owned(), "old-refresh".to_owned()))
    );
    let saved = store.value("entry-rotated").await.unwrap();
    assert_eq!(saved["refresh_token"], "rotated-refresh");
}

#[tokio::test]
async fn refresh_requires_new_access_token_and_expiry_before_merging() {
    for body in [
        r#"{"expires_in":3600,"provider_only":"value"}"#,
        r#"{"access_token":"new-access"}"#,
        r#"{"access_token":"new-access","expires_in":null}"#,
    ] {
        let (token_url, request_task) = serve_once(200, vec![], body, Duration::ZERO).await;
        let store = Arc::new(MemoryStore::default());
        let old = json!({
            "access_token":"old-access",
            "refresh_token":"old-refresh",
            "expires_at":NOW
        });
        store.put("entry-malformed-refresh", old.clone()).await;
        let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
        let client = factory.client(test_config(token_url)).unwrap();
        let session = client.session("entry-malformed-refresh").unwrap();

        assert_eq!(
            session
                .authorized_request(HttpClient::new().get("http://127.0.0.1:9/never-send"))
                .await
                .unwrap_err(),
            OAuthError::InvalidTokenResponse,
            "refresh body: {body}"
        );
        assert_eq!(store.value("entry-malformed-refresh").await, Some(old));
        let _ = request_task.await.unwrap();
    }
}

#[tokio::test]
async fn delayed_authorization_save_conflicts_with_a_newer_refresh_commit() {
    let (authorization_token_url, authorization_request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"proof-access","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let (refresh_token_url, refresh_request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"rotated-access","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "entry-proof-race",
            json!({
                "access_token":"old-access",
                "refresh_token":"old-refresh",
                "expires_at":NOW
            }),
        )
        .await;
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let authorization_client = factory
        .client(test_config(authorization_token_url))
        .unwrap();
    let refresh_client = factory.client(test_config(refresh_token_url)).unwrap();
    let authorization_session = authorization_client.session("entry-proof-race").unwrap();
    let refresh_session = refresh_client.session("entry-proof-race").unwrap();
    let step = authorization_session
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let OAuthCallbackResult::Authorized { token, .. } = authorization_client
        .complete_callback(OAuthCallback::new(
            state_from(&step),
            Some("authorization-code".to_owned()),
            None,
        ))
        .await
        .unwrap()
    else {
        panic!("expected authorization token");
    };
    let _ = authorization_request.await.unwrap();

    refresh_session
        .authorized_request(HttpClient::new().get("http://127.0.0.1:9/never-send"))
        .await
        .unwrap();
    let _ = refresh_request.await.unwrap();

    assert_eq!(
        authorization_session.save_token(&token).await,
        Err(OAuthError::CredentialConflict)
    );
    assert_eq!(
        store.value("entry-proof-race").await.unwrap()["access_token"],
        "rotated-access"
    );
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canceled_authorized_request_keeps_refresh_commit_owned_until_shutdown() {
    let (token_url, request_task) = serve_once(
        200,
        vec![],
        r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(CommitGateStore::default());
    store
        .inner
        .put(
            "entry-cancel-refresh",
            json!({
                "access_token":"old-access",
                "refresh_token":"old-refresh",
                "expires_at":NOW
            }),
        )
        .await;
    store.block_next_store.store(true, Ordering::SeqCst);
    let factory =
        OAuthClientFactory::with_clock_without_proxy(store.clone(), Arc::new(FakeClock::new(NOW)))
            .unwrap();
    let client = factory.client(test_config(token_url)).unwrap();
    let session = client.session("entry-cancel-refresh").unwrap();
    let request_session = session.clone();
    let request_task_handle = tokio::spawn(async move {
        request_session
            .authorized_request(HttpClient::new().get("http://127.0.0.1:9/never-send"))
            .await
    });

    store.wait_for_blocked_store().await;
    request_task_handle.abort();
    assert!(request_task_handle.await.unwrap_err().is_cancelled());

    let shutdown_factory = factory.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_factory.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err()
    );
    store.release_blocked_store();
    shutdown.await.unwrap().unwrap();

    assert_eq!(
        store.inner.value("entry-cancel-refresh").await.unwrap()["access_token"],
        "rotated-access"
    );
    let error = session
        .authorized_request(HttpClient::new().get("http://127.0.0.1:9/after-shutdown"))
        .await
        .unwrap_err();
    assert_eq!(error, OAuthError::ShuttingDown);
    assert_eq!(
        client
            .start_authorization(AuthorizationRequest::new(
                Url::parse("https://trusted.example.test/callback").unwrap(),
            ))
            .await
            .unwrap_err(),
        OAuthError::ShuttingDown
    );
    factory.shutdown().await.unwrap();
    let _ = request_task.await.unwrap();
}

#[tokio::test]
async fn refreshed_expiry_uses_response_receipt_time_and_normalizes_numeric_strings() {
    let clock = Arc::new(FakeClock::new(NOW));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
    let response_clock = clock.clone();
    let response = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        response_clock.set(NOW + 120);
        let body = r#"{"access_token":"refreshed-access","expires_in":"3600"}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        request
    });
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "receipt-clock",
            json!({"access_token":"old", "refresh_token":"synthetic-refresh", "expires_at":NOW}),
        )
        .await;
    let factory = memory_factory(store.clone(), clock);
    let client = factory.client(test_config(endpoint)).unwrap();
    client
        .session("receipt-clock")
        .unwrap()
        .authorized_request(HttpClient::new().get("http://127.0.0.1:9/never-send"))
        .await
        .unwrap();
    assert_eq!(
        store.value("receipt-clock").await.unwrap()["expires_at"],
        NOW + 120 + 3600
    );
    assert_eq!(
        store.value("receipt-clock").await.unwrap()["expires_in"],
        3600
    );
    response.await.unwrap();
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_storage_failure_never_authorizes_request_or_changes_old_value() {
    let (token_url, token_request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"new-access","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    let old = json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":NOW});
    store.put("entry-fail", old.clone()).await;
    store.fail_save.store(true, Ordering::SeqCst);
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let client = factory.client(test_config(token_url)).unwrap();
    let session = client.session("entry-fail").unwrap();
    let api_http = HttpClient::new();
    let error = session
        .authorized_request(api_http.get("http://127.0.0.1:9/never-send"))
        .await
        .unwrap_err();
    assert_eq!(error, OAuthError::Storage);
    assert_eq!(store.value("entry-fail").await.unwrap(), old);
    let _ = token_request.await.unwrap();
}

#[tokio::test]
async fn authorized_request_replaces_auth_with_one_sensitive_bearer_header() {
    let store = Arc::new(MemoryStore::default());
    store
        .put(
            "entry-header",
            json!({
                "access_token": "header-access",
                "expires_at": NOW + 3600
            }),
        )
        .await;
    let factory = memory_factory(store, Arc::new(FakeClock::new(NOW)));
    let client = factory
        .client(test_config(
            Url::parse("http://127.0.0.1:42424/token").unwrap(),
        ))
        .unwrap();
    let session = client.session("entry-header").unwrap();
    let api_http = HttpClient::new();
    let request = session
        .authorized_request(
            api_http
                .get("http://127.0.0.1:9/api")
                .header(AUTHORIZATION, "Basic stale-auth"),
        )
        .await
        .unwrap();
    let authorization = request.headers().get_all(AUTHORIZATION);
    let values = authorization.iter().collect::<Vec<_>>();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].to_str().unwrap(), "Bearer header-access");
    assert!(values[0].is_sensitive());
}

#[test]
fn token_parsing_coerces_valid_expiry_and_rejects_invalid_or_overflowing_values() {
    let invalid_expiries = [
        Value::Null,
        Value::Bool(true),
        Value::String("not-a-number".to_owned()),
        json!(0.5),
        json!(0),
        json!(-1),
        json!(i64::MAX),
    ];
    for expires_in in invalid_expiries {
        let mut fields = Map::new();
        fields.insert("access_token".to_owned(), json!("valid-access"));
        fields.insert("expires_in".to_owned(), expires_in);
        assert_eq!(
            ExternalOAuthToken::from_provider_map(fields, 1),
            Err(OAuthError::InvalidTokenResponse)
        );
    }

    let mut fields = Map::new();
    fields.insert("access_token".to_owned(), json!("valid-access"));
    fields.insert("expires_in".to_owned(), json!(1));
    assert_eq!(
        ExternalOAuthToken::from_provider_map(fields, i64::MAX),
        Err(OAuthError::InvalidTokenResponse)
    );

    let mut fields = Map::new();
    fields.insert("access_token".to_owned(), json!("  "));
    fields.insert("expires_in".to_owned(), json!(1));
    assert_eq!(
        ExternalOAuthToken::from_provider_map(fields, NOW),
        Err(OAuthError::InvalidTokenResponse)
    );
    assert!(serde_json::from_slice::<Value>(br#"{"expires_in":NaN}"#).is_err());
    for expires_in in [json!("3600"), json!(3600.9)] {
        let token = ExternalOAuthToken::from_provider_map(
            json!({"access_token":"valid-access", "expires_in": expires_in})
                .as_object()
                .unwrap()
                .clone(),
            NOW,
        )
        .unwrap();
        assert_eq!(token.expires_at_unix(), NOW + 3600);
        assert_eq!(token.to_value()["expires_in"], json!(3600));
    }
}

#[tokio::test]
async fn cancel_authorization_releases_pending_capacity_and_invalidates_verified_proofs() {
    let (endpoint, server) = serve_once(
        200,
        vec![],
        r#"{"access_token":"must-not-be-issued","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let client = factory.client(test_config(endpoint)).unwrap();
    let request =
        AuthorizationRequest::new(Url::parse("https://trusted.example.test/callback").unwrap());
    let step = client.start_authorization(request.clone()).await.unwrap();
    let proof = client
        .verify_callback_state(&state_from(&step))
        .await
        .unwrap();
    assert!(client.cancel_authorization(step.flow_id()));
    assert!(!client.cancel_authorization(step.flow_id()));
    assert_eq!(
        client
            .complete_verified_callback(
                proof,
                OAuthCallbackPayload::new(Some("must-not-be-sent".into()), None)
            )
            .await,
        Err(OAuthError::UnknownFlow)
    );
    for _ in 0..(MAX_PENDING_FLOWS + 1) {
        let step = client.start_authorization(request.clone()).await.unwrap();
        assert!(client.cancel_authorization(step.flow_id()));
    }
    let step = client.start_authorization(request).await.unwrap();
    factory.shutdown().await.unwrap();
    assert!(client.cancel_authorization(step.flow_id()));
    assert!(
        !server.is_finished(),
        "canceled proofs must make no token request"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn custom_authorization_commit_uses_one_private_transaction_and_advances_revision() {
    let store = Arc::new(MemoryStore::default());
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let client = factory
        .client(test_config(
            Url::parse("https://provider.example.test/token").unwrap(),
        ))
        .unwrap();
    let session = client.session("transaction-account").unwrap();
    let token = ExternalOAuthToken::from_stored_value(
        json!({"access_token":"synthetic-access", "expires_at":NOW+3600}),
    )
    .unwrap();
    let target = store.clone();
    let result = session
        .commit_authorization(&token, move |value| async move {
            target.put("transaction-account", value).await;
            Ok(42_u32)
        })
        .await
        .unwrap();
    assert_eq!(result, 42);
    assert_eq!(
        store.saves.load(Ordering::SeqCst),
        0,
        "custom commit must not perform a second store write"
    );
    assert_eq!(
        store.value("transaction-account").await.unwrap()["access_token"],
        "synthetic-access"
    );
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    assert_eq!(
        session
            .commit_authorization(&token, move |_| async move {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await,
        Err(OAuthError::CredentialConflict)
    );
    assert!(!called.load(Ordering::SeqCst));
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_custom_authorization_commit_preserves_revision_for_a_retry() {
    let store = Arc::new(MemoryStore::default());
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let client = factory
        .client(test_config(
            Url::parse("https://provider.example.test/token").unwrap(),
        ))
        .unwrap();
    let session = client.session("transaction-failure").unwrap();
    let token = ExternalOAuthToken::from_stored_value(
        json!({"access_token":"synthetic-access", "expires_at":NOW+3600}),
    )
    .unwrap();
    assert_eq!(
        session
            .commit_authorization(&token, |_| async { Err::<(), _>(OAuthError::Storage) })
            .await,
        Err(OAuthError::Storage)
    );
    assert!(store.value("transaction-failure").await.is_none());
    session.save_token(&token).await.unwrap();
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn authorization_save_permits_are_bound_to_the_issuing_factory_and_client() {
    let (endpoint, request) = serve_once(
        200,
        vec![],
        r#"{"access_token":"issued-access","expires_in":3600}"#,
        Duration::ZERO,
    )
    .await;
    let store = Arc::new(MemoryStore::default());
    let factory = memory_factory(store.clone(), Arc::new(FakeClock::new(NOW)));
    let client = factory.client(test_config(endpoint.clone())).unwrap();
    let session = client.session("same-account").unwrap();
    let step = session
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let OAuthCallbackResult::Authorized { token, .. } = client
        .complete_callback(OAuthCallback::new(
            state_from(&step),
            Some("synthetic-code".into()),
            None,
        ))
        .await
        .unwrap()
    else {
        panic!("expected authorization");
    };
    request.await.unwrap();

    let other_store = Arc::new(MemoryStore::default());
    let preserved = json!({"access_token":"other-account-access", "expires_at": NOW+1000});
    other_store.put("same-account", preserved.clone()).await;
    let other_factory = memory_factory(other_store.clone(), Arc::new(FakeClock::new(NOW)));
    let other_client = other_factory.client(test_config(endpoint.clone())).unwrap();
    assert_eq!(
        other_client
            .session("same-account")
            .unwrap()
            .save_token(&token)
            .await,
        Err(OAuthError::CredentialConflict)
    );
    assert_eq!(other_store.value("same-account").await.unwrap(), preserved);
    let wrong_client = factory.client(test_config(endpoint)).unwrap();
    assert_eq!(
        wrong_client
            .session("same-account")
            .unwrap()
            .save_token(&token)
            .await,
        Err(OAuthError::CredentialConflict)
    );
    assert!(store.value("same-account").await.is_none());
    session.save_token(&token).await.unwrap();
    assert_eq!(
        store.value("same-account").await.unwrap()["access_token"],
        "issued-access"
    );
    factory.shutdown().await.unwrap();
    other_factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn last_factory_drop_aborts_a_pending_job_that_retains_shared_state() {
    struct Released(Arc<AtomicBool>);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let weak = Arc::downgrade(&factory.shared);
    let retained = factory.shared.clone();
    let released = Arc::new(AtomicBool::new(false));
    let observed = released.clone();
    let started = Arc::new(Notify::new());
    let signal = started.clone();
    let receiver = factory
        .shared
        .spawn_owned_job(async move {
            let _retained = retained;
            let _released = Released(observed);
            signal.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        })
        .await
        .unwrap();
    started.notified().await;
    drop(factory);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !released.load(Ordering::SeqCst) || weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("last owner must release cyclic retained work");
    assert!(receiver.await.is_err());
}

#[tokio::test]
async fn reaping_a_canceled_callers_panicked_job_preserves_shutdown_failure() {
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let receiver = factory
        .shared
        .spawn_owned_job(async {
            panic!("synthetic job panic");
            #[allow(unreachable_code)]
            Ok::<(), OAuthError>(())
        })
        .await
        .unwrap();
    assert!(receiver.await.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if factory
                .shared
                .jobs
                .lock()
                .unwrap()
                .tasks
                .iter()
                .all(JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    factory
        .shared
        .spawn_owned_job(async { Ok(()) })
        .await
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(factory.shutdown().await, Err(OAuthError::BackgroundTask));
    assert_eq!(factory.shutdown().await, Err(OAuthError::BackgroundTask));
}

#[test]
fn expiry_skew_boundary_and_storage_token_validation_are_explicit() {
    let mut fields = Map::new();
    fields.insert("access_token".to_owned(), json!("token"));
    fields.insert("refresh_token".to_owned(), json!("refresh"));
    fields.insert("expires_in".to_owned(), json!(60));
    fields.insert("expires_at".to_owned(), json!(NOW + 21));
    fields.insert("provider_extra".to_owned(), json!([1, 2, 3]));
    let token = ExternalOAuthToken::from_provider_map(fields, NOW).unwrap();
    assert!(token.is_usable_at(NOW));
    let boundary = ExternalOAuthToken::from_stored_value(json!({
        "access_token":"token", "refresh_token":"refresh", "expires_at":NOW + 20
    }))
    .unwrap();
    assert!(!boundary.is_usable_at(NOW));
    let stored = ExternalOAuthToken::from_stored_value(token.to_value()).unwrap();
    assert_eq!(stored.to_value()["provider_extra"], json!([1, 2, 3]));
    assert!(matches!(
        ExternalOAuthToken::from_stored_value(json!({"access_token":"token","expires_at":true})),
        Err(OAuthError::InvalidTokenResponse)
    ));
}

#[tokio::test]
async fn status_mapping_body_limit_timeout_and_error_display_are_sanitized() {
    for (status, expected) in [
        (400, OAuthError::ReauthenticationRequired(400)),
        (401, OAuthError::ReauthenticationRequired(401)),
        (429, OAuthError::Transient(429)),
        (500, OAuthError::Transient(500)),
        (503, OAuthError::Transient(503)),
    ] {
        let (token_url, request_task) = serve_once(
            status,
            vec![],
            r#"{"error":"invalid_grant","error_description":"response-secret"}"#,
            Duration::ZERO,
        )
        .await;
        let token_url = Url::parse(&format!("{}&private-url-query=url-secret", token_url)).unwrap();
        let factory = memory_factory(
            Arc::new(MemoryStore::default()),
            Arc::new(FakeClock::new(NOW)),
        );
        let client = factory.client(test_config(token_url)).unwrap();
        let step = client
            .start_authorization(AuthorizationRequest::new(
                Url::parse("https://trusted.example.test/callback").unwrap(),
            ))
            .await
            .unwrap();
        let error = client
            .complete_callback(OAuthCallback::new(
                state_from(&step),
                Some("authorization-code-secret".to_owned()),
                None,
            ))
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        let diagnostic = format!("{error:?} {error}");
        for secret in [
            "response-secret",
            "url-secret",
            "authorization-code-secret",
            "dummy-client-secret",
        ] {
            assert!(!diagnostic.contains(secret));
        }
        let _ = request_task.await.unwrap();
    }

    let (token_url, request_task) = serve_once(
        200,
        vec![],
        "oversized-body-that-is-not-a-token",
        Duration::ZERO,
    )
    .await;
    let config = test_config(token_url)
        .with_max_token_response_bytes(16)
        .unwrap();
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let client = factory.client(config).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                state_from(&step),
                Some("dummy-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthError::ResponseTooLarge)
    );
    let _ = request_task.await.unwrap();

    let (token_url, request_task) = serve_once(
        200,
        vec![],
        r#"{"access_token":"access","expires_in":3600}"#,
        Duration::from_millis(100),
    )
    .await;
    let config = test_config(token_url)
        .with_request_timeout(Duration::from_millis(20))
        .unwrap();
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let client = factory.client(config).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client
            .complete_callback(OAuthCallback::new(
                state_from(&step),
                Some("dummy-code".to_owned()),
                None,
            ))
            .await,
        Err(OAuthError::Timeout)
    );
    let _ = request_task.await.unwrap();
}

#[tokio::test]
async fn connection_failure_does_not_echo_url_credentials_or_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let token_url = Url::parse(&format!(
        "http://user:url-secret@{address}/token?query-secret=value"
    ))
    .unwrap();
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let config = OAuthClientConfig::new(
        "dummy-client-id",
        Some("dummy-client-secret".to_owned()),
        Url::parse("https://oauth.example.test/authorize").unwrap(),
        token_url,
    );
    assert_eq!(config.unwrap_err(), OAuthError::InvalidEndpoint);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let token_url = Url::parse(&format!("http://{address}/token?query-secret=value")).unwrap();
    let client = factory.client(test_config(token_url)).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    let error = client
        .complete_callback(OAuthCallback::new(
            state_from(&step),
            Some("authorization-code-secret".to_owned()),
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(error, OAuthError::Network);
    let diagnostic = format!("{error:?} {error}");
    for secret in [
        "query-secret",
        "url-secret",
        "authorization-code-secret",
        "dummy-client-secret",
    ] {
        assert!(!diagnostic.contains(secret));
    }
}

#[tokio::test]
async fn public_client_posts_client_id_without_empty_secret_or_basic_header() {
    let (token_url, request_task) = serve_once(
        200,
        vec![],
        r#"{"access_token":"public-access","expires_in":60}"#,
        Duration::ZERO,
    )
    .await;
    let factory = memory_factory(
        Arc::new(MemoryStore::default()),
        Arc::new(FakeClock::new(NOW)),
    );
    let config = OAuthClientConfig::new(
        "public-client",
        Some(String::new()),
        Url::parse("https://oauth.example.test/authorize").unwrap(),
        token_url,
    )
    .unwrap()
    .with_authentication_method(ClientAuthenticationMethod::ClientSecretBasic);
    let client = factory.client(config).unwrap();
    let step = client
        .start_authorization(AuthorizationRequest::new(
            Url::parse("https://trusted.example.test/callback").unwrap(),
        ))
        .await
        .unwrap();
    client
        .complete_callback(OAuthCallback::new(
            state_from(&step),
            Some("dummy-code".to_owned()),
            None,
        ))
        .await
        .unwrap();
    let request = request_task.await.unwrap();
    let params = form_params(&request.body);
    assert!(params.contains(&("client_id".to_owned(), "public-client".to_owned())));
    assert!(!params.iter().any(|(key, _)| key == "client_secret"));
    assert!(!request.headers.contains_key("authorization"));
}
