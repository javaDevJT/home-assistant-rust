//! Direct media-player commands exercised through the real WebSocket router.

use std::{
    future::{Future, pending},
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_auth::AuthManager;
use hass_core::{Context, MemoryStore, Runtime};
use hass_entity::{EntityLifecycle, EntitySpec};
use hass_media_player::{
    BrowseMedia, BrowseMediaRequest, MediaClass, MediaCommand, MediaPlayerDispatcher,
    MediaPlayerEntitySpec, MediaPlayerError, MediaPlayerFeatures, MediaPlayerProvider,
    MediaPlayerProviderContext, MediaPlayerRegistration, MediaPlayerResult, MediaPlayerSnapshot,
    ProviderCommandOutcome, SearchMedia, SearchMediaRequest,
};
use hass_registry::{EntityUpdate, Registries};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Notify, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CLIENT_ID: &str = "http://localhost:3000/";
const PASSWORD: &str = "media-player-fixture-password";

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = MediaPlayerResult<T>> + Send + 'a>>;

struct FakeProvider {
    browse_result: Mutex<BrowseMedia>,
    search_result: Mutex<SearchMedia>,
    browse_calls: Mutex<Vec<(String, Context, BrowseMediaRequest)>>,
    search_calls: Mutex<Vec<(String, Context, SearchMediaRequest)>>,
    fail_browse: AtomicBool,
    fail_search: AtomicBool,
    block_browse: AtomicBool,
    browse_started: Notify,
    browse_dropped: Arc<AtomicBool>,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            browse_result: Mutex::new(BrowseMedia::new(
                "Root",
                "directory",
                "fixture",
                "root",
                false,
                true,
            )),
            search_result: Mutex::new(SearchMedia::new(Vec::new())),
            browse_calls: Mutex::new(Vec::new()),
            search_calls: Mutex::new(Vec::new()),
            fail_browse: AtomicBool::new(false),
            fail_search: AtomicBool::new(false),
            block_browse: AtomicBool::new(false),
            browse_started: Notify::new(),
            browse_dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl FakeProvider {
    fn set_results(&self, browse: BrowseMedia, search: SearchMedia) {
        *self.browse_result.lock().unwrap() = browse;
        *self.search_result.lock().unwrap() = search;
    }

    fn browse_call_count(&self) -> usize {
        self.browse_calls.lock().unwrap().len()
    }

    fn search_call_count(&self) -> usize {
        self.search_calls.lock().unwrap().len()
    }
}

struct DropMark(Arc<AtomicBool>);

impl Drop for DropMark {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl MediaPlayerProvider for FakeProvider {
    fn snapshot<'a, 'b, 'async_trait>(
        &'a self,
        _context: &'b MediaPlayerProviderContext,
    ) -> ProviderFuture<'async_trait, MediaPlayerSnapshot>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(MediaPlayerSnapshot::default()) })
    }

    fn execute<'a, 'b, 'async_trait>(
        &'a self,
        _context: &'b MediaPlayerProviderContext,
        _command: MediaCommand,
    ) -> ProviderFuture<'async_trait, ProviderCommandOutcome>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(ProviderCommandOutcome::Executed) })
    }

    fn browse<'a, 'b, 'async_trait>(
        &'a self,
        context: &'b MediaPlayerProviderContext,
        request: BrowseMediaRequest,
    ) -> ProviderFuture<'async_trait, BrowseMedia>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if self.block_browse.load(Ordering::Acquire) {
                let _drop_mark = DropMark(Arc::clone(&self.browse_dropped));
                self.browse_started.notify_one();
                return pending::<MediaPlayerResult<BrowseMedia>>().await;
            }
            self.browse_calls.lock().unwrap().push((
                context.entity_id().to_owned(),
                context.context().clone(),
                request,
            ));
            if self.fail_browse.load(Ordering::Acquire) {
                return Err(MediaPlayerError::Provider(
                    "provider secret: fixture-token-should-not-escape".into(),
                ));
            }
            Ok(self.browse_result.lock().unwrap().clone())
        })
    }

    fn search<'a, 'b, 'async_trait>(
        &'a self,
        context: &'b MediaPlayerProviderContext,
        request: SearchMediaRequest,
    ) -> ProviderFuture<'async_trait, SearchMedia>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.search_calls.lock().unwrap().push((
                context.entity_id().to_owned(),
                context.context().clone(),
                request,
            ));
            if self.fail_search.load(Ordering::Acquire) {
                return Err(MediaPlayerError::Provider(
                    "provider secret: fixture-token-should-not-escape".into(),
                ));
            }
            Ok(self.search_result.lock().unwrap().clone())
        })
    }
}

struct Harness {
    runtime: Runtime,
    registries: Registries,
    lifecycle: EntityLifecycle,
    dispatcher: MediaPlayerDispatcher,
    registration: MediaPlayerRegistration,
    provider: Arc<FakeProvider>,
    auth: AuthManager,
    entity_id: String,
    admin_token: String,
    ordinary_token: String,
    read_only_token: String,
    admin_user_id: String,
    ordinary_user_id: String,
    read_only_user_id: String,
}

impl Harness {
    fn new(features: MediaPlayerFeatures) -> Self {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let dispatcher = MediaPlayerDispatcher::new(lifecycle.clone());
        let provider = Arc::new(FakeProvider::default());
        let registration = dispatcher
            .register_provider(
                "fixture",
                provider.clone(),
                vec![MediaPlayerEntitySpec::new(
                    EntitySpec::new("media_player", "fixture", "fixture", "one")
                        .with_unique_id("media-player-protocol-one")
                        .with_suggested_object_id("media_player_protocol_one"),
                    features,
                )],
            )
            .unwrap();
        let entity_id = registration.entity_id("one").unwrap();

        let auth = AuthManager::new(runtime.clone()).unwrap();
        let owner = auth
            .bootstrap_owner_with_username("media-owner", "Media Owner", PASSWORD)
            .unwrap();
        let admin_token = auth
            .issue_tokens(&owner.id, CLIENT_ID, None)
            .unwrap()
            .access_token;
        let ordinary = auth
            .create_user_with_username(&owner.id, "media-user", "Media User", PASSWORD, false)
            .unwrap();
        let ordinary_token = auth
            .issue_tokens(&ordinary.id, CLIENT_ID, None)
            .unwrap()
            .access_token;
        let read_only = auth
            .create_user_with_username(&owner.id, "media-reader", "Media Reader", PASSWORD, false)
            .unwrap();
        let read_only_groups = vec!["system-read-only".to_owned()];
        auth.update_user(
            &owner.id,
            &read_only.id,
            None,
            None,
            Some(&read_only_groups),
            None,
        )
        .unwrap();
        let read_only_token = auth
            .issue_tokens(&read_only.id, CLIENT_ID, None)
            .unwrap()
            .access_token;

        Self {
            runtime,
            registries,
            lifecycle,
            dispatcher,
            registration,
            provider,
            auth,
            entity_id,
            admin_token,
            ordinary_token,
            read_only_token,
            admin_user_id: owner.id,
            ordinary_user_id: ordinary.id,
            read_only_user_id: read_only.id,
        }
    }

    fn router(&self, shutdown: Option<watch::Receiver<bool>>) -> axum::Router {
        router_with_features(
            ApiState {
                runtime: self.runtime.clone(),
                access_token: String::new(),
                config: json!({"version":hass_core::VERSION}),
            },
            ApiFeatures {
                auth: Some(self.auth.clone()),
                registries: Some(self.registries.clone()),
                media_players: Some(self.dispatcher.clone()),
                ..ApiFeatures::default()
            },
            shutdown,
        )
    }

    async fn shutdown(self) {
        let _ = self.registration.unregister();
        self.lifecycle.shutdown().await.unwrap();
    }
}

async fn start_server(router: axum::Router) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (address, server)
}

async fn receive(socket: &mut Socket) -> Value {
    let message = timeout(Duration::from_secs(4), socket.next())
        .await
        .expect("WebSocket response timed out")
        .expect("WebSocket closed before response")
        .expect("WebSocket receive failed");
    serde_json::from_str(message.to_text().expect("expected a text WebSocket frame")).unwrap()
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

fn assert_error(response: &Value, expected_code: &str) {
    assert_eq!(response["type"], "result", "{response}");
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["error"]["code"], expected_code, "{response}");
}

fn expected_browse_result() -> Value {
    json!({
        "title":"Albums",
        "media_class":"directory",
        "media_content_type":"library",
        "media_content_id":"albums",
        "children_media_class":"genre",
        "can_play":false,
        "can_expand":true,
        "can_search":true,
        "search_media_classes":["album","artist"],
        "thumbnail":"/browse_media/library/albums",
        "not_shown":2,
        "children":[{
            "title":"Jazz",
            "media_class":"genre",
            "media_content_type":"music",
            "media_content_id":"genre:jazz",
            "children_media_class":"album",
            "can_play":false,
            "can_expand":true,
            "can_search":false,
            "search_media_classes":null,
            "thumbnail":null
        }]
    })
}

fn browse_result() -> BrowseMedia {
    let mut root = BrowseMedia::new("Albums", "directory", "library", "albums", false, true);
    let mut genre = BrowseMedia::new("Jazz", "genre", "music", "genre:jazz", false, true);
    genre.children.push(BrowseMedia::new(
        "Blue Train",
        "album",
        "music",
        "album:blue-train",
        true,
        false,
    ));
    genre.not_shown = 9;
    root.children.push(genre);
    root.not_shown = 2;
    root.can_search = true;
    root.search_media_classes = Some(vec!["album".into(), "artist".into()]);
    root.thumbnail = Some("/browse_media/library/albums".into());
    root
}

fn search_result() -> SearchMedia {
    let mut album = BrowseMedia::new(
        "Blue Train",
        "album",
        "music",
        "album:blue-train",
        true,
        false,
    );
    album.not_shown = 1;
    SearchMedia::new(vec![album])
}

#[tokio::test]
async fn browse_and_search_return_pinned_shapes_for_admin_ordinary_and_read_only_users() {
    let harness =
        Harness::new(MediaPlayerFeatures::BROWSE_MEDIA | MediaPlayerFeatures::SEARCH_MEDIA);
    harness
        .provider
        .set_results(browse_result(), search_result());
    let (address, server) = start_server(harness.router(None)).await;

    for (token, expected_user) in [
        (harness.admin_token.as_str(), harness.admin_user_id.as_str()),
        (
            harness.ordinary_token.as_str(),
            harness.ordinary_user_id.as_str(),
        ),
        (
            harness.read_only_token.as_str(),
            harness.read_only_user_id.as_str(),
        ),
    ] {
        let mut socket = connect(address, token).await;
        let browse = command(
            &mut socket,
            json!({
                "id":1,
                "type":"media_player/browse_media",
                "entity_id":harness.entity_id,
                "media_content_type":"library",
                "media_content_id":"albums"
            }),
        )
        .await;
        assert_eq!(browse["success"], true, "{browse}");
        assert_eq!(browse["result"], expected_browse_result());

        let search = command(
            &mut socket,
            json!({
                "id":2,
                "type":"media_player/search_media",
                "entity_id":harness.entity_id,
                "search_query":"Blue Train",
                "media_content_type":"music",
                "media_content_id":"library",
                "media_filter_classes":["album","artist"]
            }),
        )
        .await;
        assert_eq!(search["success"], true, "{search}");
        assert_eq!(
            search["result"],
            json!({
                "result":[{
                    "title":"Blue Train",
                    "media_class":"album",
                    "media_content_type":"music",
                    "media_content_id":"album:blue-train",
                    "children_media_class":null,
                    "can_play":true,
                    "can_expand":false,
                    "can_search":false,
                    "search_media_classes":null,
                    "thumbnail":null,
                    "not_shown":1,
                    "children":[]
                }]
            })
        );
        socket.close(None).await.unwrap();
        let browse_call = harness
            .provider
            .browse_calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let search_call = harness
            .provider
            .search_calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(browse_call.1.user_id.as_deref(), Some(expected_user));
        assert_eq!(search_call.1.user_id.as_deref(), Some(expected_user));
    }

    let browse_calls = harness.provider.browse_calls.lock().unwrap().clone();
    assert_eq!(browse_calls.len(), 3);
    assert!(browse_calls.iter().all(|(entity_id, _, request)| {
        entity_id == &harness.entity_id
            && request.media_content_type.as_deref() == Some("library")
            && request.media_content_id.as_deref() == Some("albums")
    }));
    let search_calls = harness.provider.search_calls.lock().unwrap().clone();
    assert_eq!(search_calls.len(), 3);
    assert!(search_calls.iter().all(|(entity_id, _, request)| {
        entity_id == &harness.entity_id
            && request.search_query == "Blue Train"
            && request.media_content_type.as_deref() == Some("music")
            && request.media_content_id.as_deref() == Some("library")
            && request
                .media_filter_classes
                .as_ref()
                .is_some_and(|classes| {
                    classes.as_slice() == [MediaClass::Album, MediaClass::Artist]
                })
    }));

    server.abort();
    harness.shutdown().await;
}

#[tokio::test]
async fn strict_schemas_reject_invalid_fields_types_pairs_and_filters_before_provider_calls() {
    let harness =
        Harness::new(MediaPlayerFeatures::BROWSE_MEDIA | MediaPlayerFeatures::SEARCH_MEDIA);
    let (address, server) = start_server(harness.router(None)).await;
    let mut socket = connect(address, &harness.admin_token).await;

    let invalid_commands = [
        json!({
            "id":1,
            "type":"media_player/browse_media",
            "entity_id":harness.entity_id,
            "media_content_type":"music"
        }),
        json!({
            "id":2,
            "type":"media_player/search_media",
            "entity_id":harness.entity_id,
            "search_query":"track",
            "media_content_id":"id-without-type"
        }),
        json!({
            "id":3,
            "type":"media_player/search_media",
            "entity_id":harness.entity_id,
            "search_query":"track",
            "media_filter_classes":"album"
        }),
        json!({
            "id":4,
            "type":"media_player/search_media",
            "entity_id":harness.entity_id,
            "search_query":17
        }),
        json!({
            "id":5,
            "type":"media_player/search_media",
            "entity_id":harness.entity_id,
            "search_query":"track",
            "media_filter_classes":["album","not_a_media_class"]
        }),
        json!({
            "id":6,
            "type":"media_player/browse_media",
            "entity_id":harness.entity_id,
            "media_content_type":false,
            "media_content_id":"root"
        }),
        json!({
            "id":7,
            "type":"media_player/browse_media",
            "entity_id":harness.entity_id,
            "provider_secret":"must-not-be-accepted"
        }),
        json!({
            "id":8,
            "type":"media_player/browse_media",
            "entity_id":"invalid entity id"
        }),
        json!({
            "id":9,
            "type":"media_player/browse_media",
            "media_content_id":"missing-entity-id"
        }),
    ];

    for (index, value) in invalid_commands.into_iter().enumerate() {
        let response = command(&mut socket, value).await;
        assert_error(&response, "invalid_format");
        assert_eq!(
            response["error"]["message"], "Invalid media-player command.",
            "schema errors use safe fixed messages"
        );
        assert_eq!(harness.provider.browse_call_count(), 0, "case {index}");
        assert_eq!(harness.provider.search_call_count(), 0, "case {index}");
    }

    socket.close(None).await.unwrap();
    server.abort();
    harness.shutdown().await;
}

#[tokio::test]
async fn missing_entities_feature_errors_and_provider_failures_have_safe_codes() {
    let harness = Harness::new(MediaPlayerFeatures::BROWSE_MEDIA);
    let (address, server) = start_server(harness.router(None)).await;
    let mut socket = connect(address, &harness.admin_token).await;

    let missing = command(
        &mut socket,
        json!({
            "id":1,
            "type":"media_player/browse_media",
            "entity_id":"media_player.missing"
        }),
    )
    .await;
    assert_error(&missing, "entity_not_found");
    assert_eq!(missing["error"]["message"], "Entity not found");

    let unsupported = command(
        &mut socket,
        json!({
            "id":2,
            "type":"media_player/search_media",
            "entity_id":harness.entity_id,
            "search_query":"track"
        }),
    )
    .await;
    assert_error(&unsupported, "not_supported");
    assert_eq!(
        unsupported["error"]["message"],
        "Player does not support searching media"
    );

    harness.provider.fail_browse.store(true, Ordering::Release);
    let provider_error = command(
        &mut socket,
        json!({
            "id":3,
            "type":"media_player/browse_media",
            "entity_id":harness.entity_id
        }),
    )
    .await;
    assert_error(&provider_error, "unknown_error");
    assert_eq!(
        provider_error["error"]["message"],
        "Media-player request failed."
    );
    assert!(
        !provider_error
            .to_string()
            .contains("fixture-token-should-not-escape")
    );

    socket.close(None).await.unwrap();
    server.abort();
    harness.shutdown().await;
}

#[tokio::test]
async fn renamed_and_disabled_registry_targets_follow_current_entity_lifecycle() {
    let harness =
        Harness::new(MediaPlayerFeatures::BROWSE_MEDIA | MediaPlayerFeatures::SEARCH_MEDIA);
    let (address, server) = start_server(harness.router(None)).await;
    let mut socket = connect(address, &harness.admin_token).await;
    let old_id = harness.entity_id.clone();

    harness
        .registries
        .update_entity(
            &old_id,
            EntityUpdate {
                new_entity_id: Some("media_player.media_player_protocol_renamed".into()),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    let renamed_id = "media_player.media_player_protocol_renamed";
    wait_until(|| harness.registration.entity_id("one").as_deref() == Some(renamed_id)).await;

    let stale = command(
        &mut socket,
        json!({
            "id":1,
            "type":"media_player/browse_media",
            "entity_id":old_id
        }),
    )
    .await;
    assert_error(&stale, "entity_not_found");
    let current = command(
        &mut socket,
        json!({
            "id":2,
            "type":"media_player/browse_media",
            "entity_id":renamed_id
        }),
    )
    .await;
    assert_eq!(current["success"], true, "{current}");

    harness
        .registries
        .update_entity(
            renamed_id,
            EntityUpdate {
                disabled_by: Some(Some("user".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| harness.registration.entity_id("one").is_none()).await;
    let disabled = command(
        &mut socket,
        json!({
            "id":3,
            "type":"media_player/browse_media",
            "entity_id":renamed_id
        }),
    )
    .await;
    assert_error(&disabled, "entity_not_found");

    socket.close(None).await.unwrap();
    server.abort();
    harness.shutdown().await;
}

#[tokio::test]
async fn invalid_auth_is_rejected_and_shutdown_drops_blocked_provider_callback() {
    let harness =
        Harness::new(MediaPlayerFeatures::BROWSE_MEDIA | MediaPlayerFeatures::SEARCH_MEDIA);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (address, server) = start_server(harness.router(Some(shutdown_rx))).await;

    let (mut invalid_socket, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    assert_eq!(receive(&mut invalid_socket).await["type"], "auth_required");
    let invalid_auth = command(
        &mut invalid_socket,
        json!({"type":"auth","access_token":"invalid-media-token"}),
    )
    .await;
    assert_eq!(invalid_auth["type"], "auth_invalid");

    let mut blocked_socket = connect(address, &harness.admin_token).await;
    let mut idle_socket = connect(address, &harness.ordinary_token).await;
    harness.provider.block_browse.store(true, Ordering::Release);
    blocked_socket
        .send(Message::Text(
            json!({
                "id":1,
                "type":"media_player/browse_media",
                "entity_id":harness.entity_id
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    timeout(
        Duration::from_secs(3),
        harness.provider.browse_started.notified(),
    )
    .await
    .expect("provider callback did not start");

    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(3), async {
        while !harness.provider.browse_dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("API shutdown did not drop the blocked provider callback");

    let idle_closed = timeout(Duration::from_secs(3), idle_socket.next()).await;
    assert!(
        idle_closed.is_ok(),
        "quiet socket remained blocked on shutdown"
    );
    let blocked_response = receive(&mut blocked_socket).await;
    assert_error(&blocked_response, "home_assistant_error");
    assert_eq!(
        blocked_response["error"]["message"],
        "API is shutting down."
    );

    server.abort();
    harness.shutdown().await;
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(2), async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("entity lifecycle update was not observed");
}
