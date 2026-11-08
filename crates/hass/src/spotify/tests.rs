use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, post};
use axum::{Json, Router};
use hass_application_credentials::{ApplicationCredentialsIntegration, CreateCredential};
use hass_config::{ConfigEntries, ConfigEntry, ConfigEntryState};
use hass_core::{Context, MemoryStore, Runtime};
use hass_entity::EntityLifecycle;
use hass_media_player::MediaPlayerDispatcher;
use hass_media_player::MediaPlayerError;
use hass_oauth2_client::OAuthClientFactory;
use hass_oauth2_flow::{
    ConfigEntryTokenStore, OAuthCallbackParams, OAuthFlowHost, OAuthFlowHostOptions,
};
use hass_registry::Registries;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

use super::{DOMAIN, SpotifyIntegration, SpotifyOptions};

const SCOPES: &str = "user-modify-playback-state user-read-playback-state user-read-private playlist-read-private playlist-read-collaborative user-library-read user-top-read user-read-playback-position user-read-recently-played user-follow-read";

#[derive(Clone)]
struct LoopbackState {
    playback: Arc<Mutex<Option<Value>>>,
    devices: Arc<Mutex<Value>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    token_forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
    token_authorization: Arc<Mutex<Vec<Option<String>>>>,
    profile_calls: Arc<AtomicUsize>,
    fail_profile_call: Arc<AtomicUsize>,
    fail_profile_status: Arc<AtomicUsize>,
    profile_notify: Arc<tokio::sync::Notify>,
    playback_calls: Arc<AtomicUsize>,
    playback_hold: Arc<Mutex<Option<PeriodicPlaybackHold>>>,
    playlist_status: Arc<AtomicUsize>,
    command_status: Arc<AtomicUsize>,
    refresh_count: Arc<AtomicUsize>,
}

struct PeriodicPlaybackHold {
    call: usize,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    finished: oneshot::Sender<()>,
}

impl Default for LoopbackState {
    fn default() -> Self {
        Self {
            playback: Arc::new(Mutex::new(Some(playing_fixture()))),
            devices: Arc::new(Mutex::new(json!({
                "devices": [{
                    "id": "connect-device",
                    "is_active": true,
                    "is_restricted": false,
                    "name": "Kitchen",
                    "type": "speaker",
                    "volume_percent": 34
                }]
            }))),
            requests: Arc::new(Mutex::new(Vec::new())),
            token_forms: Arc::new(Mutex::new(Vec::new())),
            token_authorization: Arc::new(Mutex::new(Vec::new())),
            profile_calls: Arc::new(AtomicUsize::new(0)),
            fail_profile_call: Arc::new(AtomicUsize::new(0)),
            fail_profile_status: Arc::new(AtomicUsize::new(
                StatusCode::SERVICE_UNAVAILABLE.as_u16() as usize,
            )),
            profile_notify: Arc::new(tokio::sync::Notify::new()),
            playback_calls: Arc::new(AtomicUsize::new(0)),
            playback_hold: Arc::new(Mutex::new(None)),
            playlist_status: Arc::new(AtomicUsize::new(StatusCode::OK.as_u16() as usize)),
            command_status: Arc::new(AtomicUsize::new(204)),
            refresh_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: Method,
    target: String,
    authorization: Option<String>,
}

async fn token_endpoint(
    State(state): State<LoopbackState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let form = url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<HashMap<_, _>>();
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let refreshing = form.get("grant_type").map(String::as_str) == Some("refresh_token");
    if refreshing {
        state.refresh_count.fetch_add(1, Ordering::AcqRel);
    }
    state.token_forms.lock().unwrap().push(form);
    state
        .token_authorization
        .lock()
        .unwrap()
        .push(authorization);
    if refreshing {
        return Json(json!({
            "access_token": "runtime-refreshed-token",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": SCOPES,
            "refresh_marker": "new-response"
        }))
        .into_response();
    }
    Json(json!({
        "access_token": "runtime-initial-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token": "runtime-refresh-token",
        "scope": SCOPES,
        "provider_unknown_field": "keep-me"
    }))
    .into_response()
}

async fn spotify_endpoint(
    State(state): State<LoopbackState>,
    request: Request<Body>,
) -> Response<Body> {
    let target = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
    let authorization = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.requests.lock().unwrap().push(RecordedRequest {
        method: request.method().clone(),
        target: target.clone(),
        authorization,
    });

    match (
        request.method(),
        target.split('?').next().unwrap_or_default(),
    ) {
        (&Method::GET, "/v1/me") => {
            let call = state.profile_calls.fetch_add(1, Ordering::AcqRel) + 1;
            state.profile_notify.notify_waiters();
            if state.fail_profile_call.load(Ordering::Acquire) == call {
                let status =
                    StatusCode::from_u16(state.fail_profile_status.load(Ordering::Acquire) as u16)
                        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
                return response(status, "synthetic profile failure");
            }
            json_response(json!({
                "account_id": "immutable-account-1",
                "id": "legacy-user-1",
                "display_name": "Loopback Listener",
                "product": "premium",
                "unknown_profile_field": true
            }))
        }
        (&Method::GET, "/v1/me/player") => {
            let call = state.playback_calls.fetch_add(1, Ordering::AcqRel) + 1;
            let hold = {
                let mut configured = state.playback_hold.lock().unwrap();
                if configured.as_ref().is_some_and(|hold| hold.call == call) {
                    configured.take()
                } else {
                    None
                }
            };
            if let Some(hold) = hold {
                let _ = hold.entered.send(());
                let _ = hold.release.await;
                let _ = hold.finished.send(());
            }
            let playback = state.playback.lock().unwrap().clone();
            match playback {
                Some(playback) => json_response(playback),
                None => response(StatusCode::NO_CONTENT, ""),
            }
        }
        (&Method::GET, "/v1/me/player/devices") => {
            json_response(state.devices.lock().unwrap().clone())
        }
        (&Method::POST, "/v1/me/player/next") => response(
            StatusCode::from_u16(state.command_status.load(Ordering::Acquire) as u16)
                .unwrap_or(StatusCode::NO_CONTENT),
            "",
        ),
        (&Method::GET, path) if path.starts_with("/v1/playlists/") => {
            let status = StatusCode::from_u16(state.playlist_status.load(Ordering::Acquire) as u16)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if status.is_success() {
                json_response(json!({
                    "id": "playlist-1",
                    "type": "playlist",
                    "name": "Loopback Playlist"
                }))
            } else {
                response(status, "synthetic playlist failure")
            }
        }
        _ => response(StatusCode::NOT_FOUND, "unknown synthetic endpoint"),
    }
}

fn response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "text/plain")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn json_response(value: Value) -> Response<Body> {
    (StatusCode::OK, Json(value)).into_response()
}

fn playing_fixture() -> Value {
    json!({
        "device": {
            "id": "connect-device",
            "is_active": true,
            "is_restricted": false,
            "name": "Kitchen",
            "type": "speaker",
            "volume_percent": 34
        },
        "is_playing": true,
        "progress_ms": 12000,
        "repeat_state": "context",
        "shuffle_state": true,
        "currently_playing_type": "track",
        "item": {
            "id": "track-1",
            "type": "track",
            "uri": "spotify:track:track-1",
            "name": "Fixture Track",
            "duration_ms": 180000,
            "track_number": 4,
            "artists": [{"name": "Fixture Artist"}],
            "album": {"name": "Fixture Album", "images": [{"url": "https://images.example/album.jpg"}]}
        },
        "context": null
    })
}

struct RuntimeFixture {
    state: LoopbackState,
    runtime: Runtime,
    entries: ConfigEntries,
    registries: Registries,
    media: MediaPlayerDispatcher,
    credentials: ApplicationCredentialsIntegration,
    factory: OAuthClientFactory,
    host: OAuthFlowHost,
    options: SpotifyOptions,
    integration: Option<SpotifyIntegration>,
    server_task: JoinHandle<()>,
}

impl RuntimeFixture {
    async fn new() -> Self {
        Self::with_state(LoopbackState::default()).await
    }

    async fn with_state(state: LoopbackState) -> Self {
        Self::with_state_and_playback_interval(state, Duration::from_secs(60 * 60)).await
    }

    async fn with_state_and_playback_interval(
        state: LoopbackState,
        playback_interval: Duration,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/token", post(token_endpoint))
            .fallback(any(spotify_endpoint))
            .with_state(state.clone());
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let media = MediaPlayerDispatcher::new(lifecycle);
        let credentials =
            ApplicationCredentialsIntegration::new(runtime.clone(), entries.clone()).unwrap();
        let factory =
            OAuthClientFactory::new(Arc::new(ConfigEntryTokenStore::new(entries.clone()))).unwrap();
        let callback = Url::parse("http://127.0.0.1:8123/auth/external/callback").unwrap();
        let host = OAuthFlowHost::new(
            entries.clone(),
            credentials.manager(),
            credentials.platform_registry(),
            factory.clone(),
            OAuthFlowHostOptions::new(callback),
        )
        .unwrap();
        let base = format!("http://{address}");
        let options = SpotifyOptions {
            authorize_url: Url::parse(&format!("{base}/authorize")).unwrap(),
            token_url: Url::parse(&format!("{base}/token")).unwrap(),
            api_base_url: Url::parse(&format!("{base}/v1/")).unwrap(),
            playback_interval,
            device_interval: Duration::from_secs(60 * 60),
            command_refresh_delay: Duration::from_millis(10),
            ..SpotifyOptions::default()
        };
        let integration = SpotifyIntegration::start_with_options(
            runtime.clone(),
            entries.clone(),
            registries.clone(),
            &host,
            media.clone(),
            options.clone(),
        )
        .await
        .unwrap();
        credentials
            .manager()
            .create_admin(CreateCredential {
                domain: DOMAIN.to_owned(),
                client_id: "synthetic-client-id".into(),
                client_secret: "synthetic-client-secret".into(),
                auth_domain: Some("synthetic-spotify-auth".into()),
                name: Some("Spotify loopback".into()),
            })
            .unwrap();
        Self {
            state,
            runtime,
            entries,
            registries,
            media,
            credentials,
            factory,
            host,
            options,
            integration: Some(integration),
            server_task,
        }
    }

    async fn authorize(&self) -> (ConfigEntry, Value, Url) {
        let manager = self.host.manager();
        let started = manager.begin(DOMAIN, "user", Map::new()).await.unwrap();
        assert_eq!(started["type"], "external");
        assert_eq!(started["step_id"], "auth");
        let flow_id = started["flow_id"].as_str().unwrap().to_owned();
        let url = Url::parse(started["url"].as_str().unwrap()).unwrap();
        let query = url.query_pairs().collect::<HashMap<_, _>>();
        let state = query["state"].clone().into_owned();
        manager
            .callback(OAuthCallbackParams::new(
                state,
                Some("synthetic-authorization-code".into()),
                None,
            ))
            .await
            .unwrap();
        let result = manager.get(&flow_id).await.unwrap();
        let entry = self
            .entries
            .get_entry(result["result"]["entry_id"].as_str().unwrap())
            .unwrap();
        (entry, result, url)
    }

    async fn shutdown(&mut self) {
        if let Some(integration) = self.integration.take() {
            integration.shutdown().await.unwrap();
        }
        self.host.shutdown().await;
        let _ = self.factory.shutdown().await;
        self.credentials.shutdown().await;
        self.server_task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_oauth_entry_setup_refresh_command_and_joined_unload() {
    let mut fixture = RuntimeFixture::new().await;
    let (entry, result, authorization_url) = fixture.authorize().await;
    assert_eq!(result["type"], "create_entry");
    assert_eq!(result["result"]["state"], "loaded");
    assert_eq!(entry.state, ConfigEntryState::Loaded);
    assert_eq!(entry.unique_id.as_deref(), Some("immutable-account-1"));
    assert_eq!(entry.data["account_id"], "immutable-account-1");
    assert_eq!(entry.data["id"], "legacy-user-1");

    let query = authorization_url.query_pairs().collect::<HashMap<_, _>>();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "synthetic-client-id");
    assert_eq!(query["scope"], SCOPES);
    assert!(!query["scope"].contains(','));
    assert_eq!(
        query["redirect_uri"],
        "http://127.0.0.1:8123/auth/external/callback"
    );
    let initial_token_form = fixture.state.token_forms.lock().unwrap()[0].clone();
    assert_eq!(initial_token_form["grant_type"], "authorization_code");
    assert_eq!(initial_token_form["code"], "synthetic-authorization-code");
    assert!(!initial_token_form.contains_key("client_secret"));
    assert!(
        fixture.state.token_authorization.lock().unwrap()[0]
            .as_deref()
            .is_some_and(|value| value.starts_with("Basic "))
    );

    let player_entity = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity");
    let initial = fixture.runtime.get_state(&player_entity.entity_id).unwrap();
    assert_eq!(initial.state, "playing");
    assert_eq!(initial.attributes["media_title"], "Fixture Track");
    assert_eq!(initial.attributes["media_artist"], "Fixture Artist");
    assert_eq!(initial.attributes["source"], "Kitchen");
    assert_eq!(initial.attributes["source_list"], json!(["Kitchen"]));
    assert_eq!(
        initial.attributes["media_content_id"],
        "spotify:track:track-1"
    );
    assert_eq!(initial.attributes["volume_level"], 0.34);
    assert!(
        !entry
            .public_json()
            .to_string()
            .contains("runtime-initial-token")
    );

    let mut token = entry.data["token"].clone();
    token["expires_at"] = json!(chrono::Utc::now().timestamp() - 1);
    fixture
        .entries
        .patch_entry_data(
            &entry.entry_id,
            DOMAIN,
            Map::from_iter([("token".into(), token)]),
        )
        .unwrap();

    *fixture.state.playback.lock().unwrap() = None;
    let context = Context {
        id: "spotify-command-context".into(),
        parent_id: Some("parent-context".into()),
        user_id: Some("user-1".into()),
    };
    let outcome = fixture
        .media
        .dispatch_service_with_context(
            "media_next_track",
            &Map::new(),
            std::slice::from_ref(&player_entity.entity_id),
            context,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.dispatched_entity_ids,
        vec![player_entity.entity_id.clone()]
    );
    assert_eq!(fixture.state.refresh_count.load(Ordering::Acquire), 1);
    let refreshed_entry = fixture.entries.get_entry(&entry.entry_id).unwrap();
    assert_eq!(
        refreshed_entry.data["token"]["access_token"],
        "runtime-refreshed-token"
    );
    assert_eq!(
        refreshed_entry.data["token"]["refresh_token"],
        "runtime-refresh-token"
    );
    assert_eq!(
        refreshed_entry.data["token"]["provider_unknown_field"],
        "keep-me"
    );
    let refreshed = fixture.runtime.get_state(&player_entity.entity_id).unwrap();
    assert_eq!(refreshed.state, "idle");
    assert_eq!(
        refreshed.context.parent_id.as_deref(),
        Some("parent-context")
    );

    let requests = fixture.state.requests.lock().unwrap().clone();
    assert!(requests.iter().any(|request| {
        request.method == Method::GET
            && request.target.starts_with("/v1/me/player?")
            && request.target.contains("additional_types=track%2Cepisode")
    }));
    assert!(requests.iter().any(|request| {
        request.method == Method::POST
            && request.target == "/v1/me/player/next"
            && request.authorization.as_deref() == Some("Bearer runtime-refreshed-token")
    }));
    assert!(requests.iter().all(|request| {
        request
            .authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("Bearer "))
    }));

    let before_shutdown = requests.len();
    fixture.shutdown().await;
    assert!(matches!(
        fixture
            .media
            .refresh_snapshot(&format!("spotify:{}", entry.entry_id), "media_player")
            .await,
        Err(MediaPlayerError::EntityNotFound)
    ));
    assert_eq!(
        fixture.state.requests.lock().unwrap().len(),
        before_shutdown
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancels_and_joins_held_periodic_playback_request() {
    let state = LoopbackState::default();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    *state.playback_hold.lock().unwrap() = Some(PeriodicPlaybackHold {
        call: 2,
        entered: entered_tx,
        release: release_rx,
        finished: finished_tx,
    });
    let mut fixture =
        RuntimeFixture::with_state_and_playback_interval(state, Duration::from_millis(25)).await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "loaded");

    let player_entity = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity");
    assert!(
        fixture
            .runtime
            .get_state(&player_entity.entity_id)
            .is_some()
    );
    tokio::time::timeout(Duration::from_secs(3), entered_rx)
        .await
        .expect("periodic playback request did not reach the held loopback response")
        .expect("loopback server dropped the periodic playback barrier");
    assert_eq!(fixture.state.playback_calls.load(Ordering::Acquire), 2);

    let integration = fixture.integration.take().unwrap();
    let shutdown = tokio::time::timeout(Duration::from_secs(3), integration.shutdown()).await;
    // Always release the synthetic HTTP handler before reporting a shutdown
    // timeout so a failed assertion cannot strand a server task.
    let _ = release_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), finished_rx).await;
    shutdown
        .expect("explicit Spotify shutdown did not cancel and join the held poll")
        .unwrap();

    assert!(matches!(
        fixture
            .media
            .refresh_snapshot(&format!("spotify:{}", entry.entry_id), "media_player")
            .await,
        Err(MediaPlayerError::EntityNotFound)
    ));
    let state_after_shutdown = fixture.runtime.get_state(&player_entity.entity_id);
    let requests_after_shutdown = fixture.state.requests.lock().unwrap().len();
    assert!(requests_after_shutdown > 0);
    assert_eq!(
        fixture.state.requests.lock().unwrap().len(),
        requests_after_shutdown,
        "releasing the canceled response must not start another Spotify request"
    );
    assert_eq!(
        fixture.runtime.get_state(&player_entity.entity_id),
        state_after_shutdown,
        "releasing the canceled response must not publish a late entity state"
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn canceled_shutdown_keeps_cleanup_running_after_held_poll() {
    let state = LoopbackState::default();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    *state.playback_hold.lock().unwrap() = Some(PeriodicPlaybackHold {
        call: 2,
        entered: entered_tx,
        release: release_rx,
        finished: finished_tx,
    });
    let mut fixture =
        RuntimeFixture::with_state_and_playback_interval(state, Duration::from_millis(25)).await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "loaded");
    let player_entity = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity");
    tokio::time::timeout(Duration::from_secs(3), entered_rx)
        .await
        .expect("periodic playback request did not reach the held loopback response")
        .expect("loopback server dropped the periodic playback barrier");
    let mut events = fixture.runtime.subscribe();

    let integration = fixture.integration.take().unwrap();
    let mut shutdown = Box::pin(integration.shutdown());
    let first_poll =
        std::future::poll_fn(|context| Poll::Ready(shutdown.as_mut().poll(context))).await;
    assert!(
        first_poll.is_pending(),
        "shutdown should have transferred cleanup to its owned coordinator"
    );
    drop(shutdown);

    let requests_while_held = fixture.state.requests.lock().unwrap().len();
    let _ = release_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), finished_rx).await;
    let unloaded = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("runtime event stream closed");
            if event.event_type == "config_entry_changed"
                && event.data["entry"]["entry_id"] == entry.entry_id
                && event.data["entry"]["state"] == "not_loaded"
            {
                break;
            }
        }
    })
    .await;
    assert!(
        unloaded.is_ok(),
        "detached shutdown did not unload the Spotify entry"
    );
    assert_eq!(
        fixture.entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::NotLoaded
    );
    assert!(matches!(
        fixture
            .media
            .refresh_snapshot(&format!("spotify:{}", entry.entry_id), "media_player")
            .await,
        Err(MediaPlayerError::EntityNotFound)
    ));
    assert_eq!(
        fixture.state.requests.lock().unwrap().len(),
        requests_while_held,
        "aborting the shutdown caller must not leave a poll task behind"
    );
    assert_eq!(
        fixture
            .runtime
            .get_state(&player_entity.entity_id)
            .map(|state| state.state),
        Some("unavailable".into()),
        "the completed unload should remain the final entity publication"
    );

    let restarted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match SpotifyIntegration::start_with_options(
                fixture.runtime.clone(),
                fixture.entries.clone(),
                fixture.registries.clone(),
                &fixture.host,
                fixture.media.clone(),
                fixture.options.clone(),
            )
            .await
            {
                Ok(integration) => break integration,
                Err(error)
                    if error
                        .to_string()
                        .contains("register Spotify OAuth provider") =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("retry after canceled shutdown failed: {error}"),
            }
        }
    })
    .await
    .expect("detached shutdown did not release the OAuth provider registration");
    fixture.integration = Some(restarted);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_start_joins_restoration_and_releases_provider() {
    let mut fixture = RuntimeFixture::new().await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "loaded");
    let player_entity_id = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity")
        .entity_id;
    fixture
        .integration
        .take()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    assert_eq!(
        fixture.entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::NotLoaded
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let next_playback_call = fixture.state.playback_calls.load(Ordering::Acquire) + 1;
    *fixture.state.playback_hold.lock().unwrap() = Some(PeriodicPlaybackHold {
        call: next_playback_call,
        entered: entered_tx,
        release: release_rx,
        finished: finished_tx,
    });
    let mut events = fixture.runtime.subscribe();
    let state_before_cancelled_start = fixture.runtime.get_state(&player_entity_id);
    {
        let mut start = Box::pin(SpotifyIntegration::start_with_options(
            fixture.runtime.clone(),
            fixture.entries.clone(),
            fixture.registries.clone(),
            &fixture.host,
            fixture.media.clone(),
            fixture.options.clone(),
        ));
        let held = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                _ = &mut start => false,
                _ = entered_rx => true,
            }
        })
        .await
        .expect("restored startup did not reach the held HTTP response");
        assert!(
            held,
            "restored startup completed before reaching the barrier"
        );
        drop(start);
    }
    let requests_while_held = fixture.state.requests.lock().unwrap().len();
    let _ = release_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), finished_rx).await;
    assert_eq!(
        fixture.state.requests.lock().unwrap().len(),
        requests_while_held,
        "dropping the startup caller must not leave restored polling behind"
    );

    let settled = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("runtime event stream closed");
            if event.event_type == "config_entry_changed"
                && event.data["entry"]["entry_id"] == entry.entry_id
                && event.data["entry"]["state"] != "setup_in_progress"
            {
                break;
            }
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "canceled restoration did not settle the config entry"
    );
    assert_eq!(
        fixture.entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::SetupError,
        "Home Assistant marks a canceled in-progress setup as setup_error"
    );
    assert!(matches!(
        fixture
            .media
            .refresh_snapshot(&format!("spotify:{}", entry.entry_id), "media_player")
            .await,
        Err(MediaPlayerError::EntityNotFound)
    ));
    assert_eq!(
        fixture.runtime.get_state(&player_entity_id),
        state_before_cancelled_start,
        "canceled restoration must not publish state after its request is released"
    );

    let restarted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match SpotifyIntegration::start_with_options(
                fixture.runtime.clone(),
                fixture.entries.clone(),
                fixture.registries.clone(),
                &fixture.host,
                fixture.media.clone(),
                fixture.options.clone(),
            )
            .await
            {
                Ok(integration) => break integration,
                Err(error)
                    if error
                        .to_string()
                        .contains("register Spotify OAuth provider") =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("retry after canceled startup failed: {error}"),
            }
        }
    })
    .await
    .expect("canceled startup did not release its OAuth provider registration");
    fixture.integration = Some(restarted);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_authorize_profile_failure_does_not_create_a_loaded_player() {
    let state = LoopbackState::default();
    state.fail_profile_call.store(2, Ordering::Release);
    let mut fixture = RuntimeFixture::with_state(state).await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["type"], "create_entry");
    assert_eq!(result["result"]["state"], "setup_retry");
    assert_eq!(entry.state, ConfigEntryState::SetupRetry);
    assert!(
        fixture
            .registries
            .entities()
            .iter()
            .all(|entity| entity.config_entry_id.as_deref() != Some(entry.entry_id.as_str()))
    );
    assert!(
        fixture
            .runtime
            .states()
            .iter()
            .all(|state| { !state.entity_id.starts_with("media_player.spotify") })
    );

    // Re-open config metadata from the owned in-memory store and let a fresh
    // integration instance restore it through the registered native driver.
    let retry_at = entry.retry_at;
    fixture
        .integration
        .take()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let restored_entries = ConfigEntries::new(fixture.runtime.clone()).unwrap();
    let restored_entry = restored_entries.get_entry(&entry.entry_id).unwrap();
    assert_eq!(restored_entry.state, ConfigEntryState::SetupRetry);
    assert_eq!(restored_entry.retry_at, retry_at);
    let integration = SpotifyIntegration::start_with_options(
        fixture.runtime.clone(),
        restored_entries.clone(),
        fixture.registries.clone(),
        &fixture.host,
        fixture.media.clone(),
        fixture.options.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored_entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::SetupRetry
    );
    assert!(
        fixture
            .registries
            .entities()
            .iter()
            .all(|entity| entity.config_entry_id.as_deref() != Some(entry.entry_id.as_str()))
    );
    fixture.integration = Some(integration);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthorized_setup_is_a_nonfatal_auth_retry() {
    let state = LoopbackState::default();
    state.fail_profile_call.store(2, Ordering::Release);
    state.fail_profile_status.store(401, Ordering::Release);
    let mut fixture = RuntimeFixture::with_state(state).await;
    let (entry, result, _) = fixture.authorize().await;

    assert_eq!(result["type"], "create_entry");
    assert_eq!(result["result"]["state"], "setup_retry");
    assert_eq!(entry.state, ConfigEntryState::SetupRetry);
    assert!(
        entry
            .retry_at
            .is_some_and(|retry_at| retry_at > chrono::Utc::now())
    );
    assert!(matches!(
        fixture
            .media
            .refresh_snapshot(&format!("spotify:{}", entry.entry_id), "media_player")
            .await,
        Err(MediaPlayerError::EntityNotFound)
    ));
    assert!(
        fixture
            .runtime
            .states()
            .iter()
            .all(|state| !state.entity_id.starts_with("media_player.spotify"))
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn due_setup_retry_is_retried_by_the_spotify_scheduler() {
    let state = LoopbackState::default();
    state.fail_profile_call.store(2, Ordering::Release);
    let mut fixture =
        RuntimeFixture::with_state_and_playback_interval(state, Duration::from_secs(1)).await;
    let mut events = fixture.runtime.subscribe();
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "setup_retry");
    assert_eq!(entry.state, ConfigEntryState::SetupRetry);
    assert!(
        entry
            .retry_at
            .is_some_and(|retry_at| retry_at > chrono::Utc::now())
    );

    let retried = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let event = events.recv().await.expect("runtime event stream closed");
            if event.event_type == "config_entry_changed"
                && event.data["entry"]["entry_id"] == entry.entry_id
                && event.data["entry"]["state"] == "loaded"
            {
                break;
            }
        }
    })
    .await;
    assert!(
        retried.is_ok(),
        "the Spotify retry scheduler should load an entry once its retry time is due"
    );
    assert_eq!(
        fixture.entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::Loaded
    );
    assert!(fixture.registries.entities().iter().any(|entity| {
        entity.domain == "media_player"
            && entity.platform == DOMAIN
            && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
    }));
    assert_eq!(fixture.state.profile_calls.load(Ordering::Acquire), 3);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_reauth_flow_allows_a_later_unauthorized_retry() {
    let mut fixture = RuntimeFixture::new().await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "loaded");
    let player_entity = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity");
    fixture.state.command_status.store(401, Ordering::Release);
    let mut events = fixture.runtime.subscribe_signals();

    let _ = fixture
        .media
        .dispatch_service_with_context(
            "media_next_track",
            &Map::new(),
            std::slice::from_ref(&player_entity.entity_id),
            Context::default(),
        )
        .await;
    assert_eq!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.method == Method::POST && request.target == "/v1/me/player/next"
            })
            .count(),
        1,
        "the first unauthorized command must reach Spotify"
    );
    let first_flow_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("runtime event stream closed");
            if event.event_type == "config_flow_progress"
                && let Some(flow_id) = event.data["flow_id"].as_str()
            {
                break flow_id.to_owned();
            }
        }
    })
    .await
    .expect("first unauthorized command did not start a reauthentication flow");
    let manager = fixture.host.manager();
    assert!(manager.owns_flow(&first_flow_id));
    assert!(manager.flow_is_active(&first_flow_id).await);
    // The progress signal is sent at the end of begin_reauth; yield once so
    // its owner can record the flow ID before the test abandons it.
    tokio::task::yield_now().await;

    manager.abort(&first_flow_id).await.unwrap();
    assert!(!manager.owns_flow(&first_flow_id));
    let entries = fixture.entries.clone();
    let entry_id = entry.entry_id.clone();
    tokio::task::spawn_blocking(move || entries.reload_entry(&entry_id))
        .await
        .expect("Spotify account reload worker panicked")
        .expect("abandoned reauthentication should leave the account retryable");
    let _ = fixture
        .media
        .dispatch_service_with_context(
            "media_next_track",
            &Map::new(),
            std::slice::from_ref(&player_entity.entity_id),
            Context::default(),
        )
        .await;
    assert_eq!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.method == Method::POST && request.target == "/v1/me/player/next"
            })
            .count(),
        2,
        "a later command should still reach Spotify after the first unauthorized response"
    );
    let second_flow_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("runtime event stream closed");
            if event.event_type == "config_flow_progress"
                && let Some(flow_id) = event.data["flow_id"].as_str()
                && flow_id != first_flow_id
            {
                break flow_id.to_owned();
            }
        }
    })
    .await
    .expect("later unauthorized command did not start a replacement reauth flow");
    assert_ne!(second_flow_id, first_flow_id);
    assert!(manager.owns_flow(&second_flow_id));
    assert!(manager.flow_is_active(&second_flow_id).await);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_playlist_unauthorized_refresh_marks_player_unavailable_once() {
    let mut fixture = RuntimeFixture::new().await;
    let (entry, result, _) = fixture.authorize().await;
    assert_eq!(result["result"]["state"], "loaded");
    let player_entity = fixture
        .registries
        .entities()
        .into_iter()
        .find(|entity| {
            entity.domain == "media_player"
                && entity.platform == DOMAIN
                && entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
        })
        .expect("loaded Spotify entry should own its media-player entity");
    let baseline_flow_count = fixture.host.active_flow_count();
    let mut events = fixture.runtime.subscribe_signals();
    let mut changed_playback = playing_fixture();
    changed_playback["context"] = json!({
        "type": "playlist",
        "uri": "spotify:playlist:changed-context"
    });
    *fixture.state.playback.lock().unwrap() = Some(changed_playback);
    fixture.state.playlist_status.store(401, Ordering::Release);

    let outcome = fixture
        .media
        .dispatch_service_with_context(
            "media_next_track",
            &Map::new(),
            std::slice::from_ref(&player_entity.entity_id),
            Context::default(),
        )
        .await
        .expect("successful playback command should return after refresh handling");
    assert_eq!(
        outcome.dispatched_entity_ids,
        vec![player_entity.entity_id.clone()]
    );
    let reauth_flow_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("runtime signal stream closed");
            if event.event_type == "config_flow_progress"
                && let Some(flow_id) = event.data["flow_id"].as_str()
            {
                break flow_id.to_owned();
            }
        }
    })
    .await
    .expect("playlist unauthorized refresh did not request reauthentication");
    let manager = fixture.host.manager();
    assert!(manager.owns_flow(&reauth_flow_id));
    assert!(manager.flow_is_active(&reauth_flow_id).await);
    assert_eq!(fixture.host.active_flow_count(), baseline_flow_count + 1);
    let state = fixture.runtime.get_state(&player_entity.entity_id).unwrap();
    assert_eq!(state.state, "unavailable");
    assert!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| {
                request.method == Method::GET
                    && request.target.starts_with("/v1/playlists/changed-context")
            })
    );
    fixture.shutdown().await;
}
