use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use hass_oauth2_client::{
    ExternalOAuthToken, OAuthClientConfig, OAuthClientFactory, OAuthTokenStore, TokenStoreError,
};
use hass_spotify::{
    AuthorizationError, OAuthSessionAuthorizer, PlayMediaTarget, RepeatMode, RequestAuthorizer,
    SpotifyClient, SpotifyClientOptions, SpotifyError, StartPlaybackRequest,
};
use reqwest::{Request, RequestBuilder, header};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};
use url::Url;

const TEST_ACCESS_TOKEN: &str = "fixture-access-token-secret";

#[derive(Default)]
struct MemoryTokenStore {
    values: Mutex<HashMap<String, Value>>,
}

impl OAuthTokenStore for MemoryTokenStore {
    fn load<'a>(
        &'a self,
        account_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, TokenStoreError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.values.lock().unwrap().get(account_id).cloned()) })
    }

    fn store<'a>(
        &'a self,
        account_id: &'a str,
        provider_token: Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), TokenStoreError>> + Send + 'a>> {
        Box::pin(async move {
            self.values
                .lock()
                .unwrap()
                .insert(account_id.to_owned(), provider_token);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct FixedAuthorizer {
    token: String,
    calls: Arc<AtomicUsize>,
}

impl std::fmt::Debug for FixedAuthorizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixedAuthorizer")
            .field("token", &"[redacted]")
            .finish()
    }
}

#[async_trait]
impl RequestAuthorizer for FixedAuthorizer {
    async fn authorize(&self, request: RequestBuilder) -> Result<Request, AuthorizationError> {
        let mut request = request
            .build()
            .map_err(|_| AuthorizationError::InvalidRequest)?;
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| AuthorizationError::InvalidRequest)?;
        bearer.set_sensitive(true);
        request.headers_mut().insert(header::AUTHORIZATION, bearer);
        assert!(request.headers()[header::AUTHORIZATION].is_sensitive());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(request)
    }
}

#[derive(Clone, Debug)]
struct ResponseSpec {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl ResponseSpec {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&value).unwrap(),
            delay: Duration::ZERO,
        }
    }

    fn body(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn no_content() -> Self {
        Self::body(204, Vec::new())
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WireRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct FixtureServer {
    base_url: Url,
    requests: JoinHandle<Vec<WireRequest>>,
}

async fn fixture(responses: Vec<ResponseSpec>) -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base_url = Url::parse(&format!("http://{address}/v1/")).unwrap();
    let requests = serve(listener, responses);
    FixtureServer { base_url, requests }
}

fn serve(listener: TcpListener, responses: Vec<ResponseSpec>) -> JoinHandle<Vec<WireRequest>> {
    tokio::spawn(async move {
        let mut captured = Vec::with_capacity(responses.len());
        for response in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut stream, request) = read_request(stream).await.unwrap();
            captured.push(request);
            if !response.delay.is_zero() {
                sleep(response.delay).await;
            }
            let _ = write_response(&mut stream, response).await;
        }
        captured
    })
}

async fn read_request(stream: TcpStream) -> io::Result<(TcpStream, WireRequest)> {
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).await?;
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok((
        reader.into_inner(),
        WireRequest {
            method,
            target,
            headers,
            body,
        },
    ))
}

async fn write_response(stream: &mut TcpStream, response: ResponseSpec) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Fixture",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n",
        response.status,
        reason,
        response.body.len()
    );
    for (name, value) in response.headers {
        headers.push_str(&format!("{name}: {value}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await
}

fn new_client(base_url: Url) -> (SpotifyClient, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let authorizer = FixedAuthorizer {
        token: TEST_ACCESS_TOKEN.to_owned(),
        calls: calls.clone(),
    };
    let client = SpotifyClient::with_base_url(base_url, Arc::new(authorizer)).unwrap();
    (client, calls)
}

fn options_with_timeout(request_timeout: Duration) -> SpotifyClientOptions {
    SpotifyClientOptions {
        request_timeout,
        ..SpotifyClientOptions::default()
    }
}

fn query_pairs(target: &str) -> Vec<(String, String)> {
    let url = Url::parse(&format!("http://fixture{target}")).unwrap();
    url.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn assert_request(request: &WireRequest, method: &str, path: &str, query: &[(&str, &str)]) {
    assert_eq!(request.method, method);
    let (actual_path, _) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    assert_eq!(actual_path, path);
    assert_eq!(
        query_pairs(&request.target),
        query
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer fixture-access-token-secret")
    );
}

#[tokio::test]
async fn profile_playback_devices_and_all_pinned_browse_routes_are_exact() {
    let unknown_playback = json!({
        "device": {
            "id": null,
            "volume_percent": null,
            "supports_volume": null,
            "future_device_field": "preserved"
        },
        "shuffle_state": false,
        "repeat_state": "off",
        "context": null,
        "progress_ms": 0,
        "is_playing": true,
        "currently_playing_type": "future_type",
        "item": {
            "type": "future_type",
            "uri": "spotify:future:item-1",
            "future_media_field": {"nested": true}
        },
        "future_state_field": [1, 2]
    });
    let empty_page = || json!({"items": [], "next": null, "total": 0});
    let mut responses = vec![
        ResponseSpec::json(json!({
            "account_id": "new-account-id",
            "id": "legacy-account-id",
            "display_name": "Kitchen",
            "profile_future_field": {"retained": true}
        })),
        ResponseSpec::json(unknown_playback),
        ResponseSpec::json(json!({
            "devices": [{"id": null, "volume_percent": null, "type": "Speaker", "future": 9}]
        })),
        ResponseSpec::json(json!({
            "id": "playlist-1",
            "name": "List",
            "items": {"items": [], "next": null, "total": 0},
            "tracks": {"items": [{"track": {"id": "legacy-inline-track"}}], "next": null, "total": 1},
            "playlist_future_field": true
        })),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(json!({"artists": empty_page()})),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(json!({
            "items": [{"item": {"type": "future_type", "future_playlist_field": "x"}}],
            "next": null,
            "total": 1
        })),
        ResponseSpec::json(json!({
            "id": "album-1",
            "album_type": "album",
            "tracks": {"items": [{"id": "album-track", "type": "track"}], "next": null, "total": 1}
        })),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(json!({"id": "artist-1", "name": "Artist"})),
        ResponseSpec::json(empty_page()),
        ResponseSpec::json(json!({"id": "show-1", "name": "Show"})),
        ResponseSpec::json(empty_page()),
    ];
    assert_eq!(responses.len(), 19);
    let server = fixture(std::mem::take(&mut responses)).await;
    let (client, authorized_calls) = new_client(server.base_url.clone());

    let profile = client.get_current_user().await.unwrap();
    assert_eq!(profile.account_id.as_deref(), Some("new-account-id"));
    assert_eq!(profile.id.as_deref(), Some("legacy-account-id"));
    assert_eq!(profile.product, None);
    assert_eq!(profile.extra["profile_future_field"]["retained"], true);

    let playback = client.get_playback().await.unwrap().unwrap();
    assert_eq!(
        playback.item.as_ref().unwrap().kind.as_deref(),
        Some("future_type")
    );
    assert_eq!(
        playback.item.as_ref().unwrap().extra["future_media_field"]["nested"],
        true
    );
    assert_eq!(playback.progress_ms, Some(0));
    assert_eq!(playback.device.as_ref().unwrap().id, None);
    assert_eq!(playback.device.as_ref().unwrap().volume_percent, None);
    assert_eq!(
        playback.device.as_ref().unwrap().extra["future_device_field"],
        "preserved"
    );
    assert_eq!(playback.extra["future_state_field"], json!([1, 2]));

    let devices = client.get_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].volume_percent, None);
    assert_eq!(devices[0].extra["future"], 9);

    let playlist = client
        .get_playlist("spotify:playlist:playlist-1")
        .await
        .unwrap();
    assert_eq!(playlist.name.as_deref(), Some("List"));
    assert_eq!(playlist.extra["playlist_future_field"], true);
    assert_eq!(
        playlist.tracks.as_ref().unwrap().items.as_ref().unwrap()[0]
            .track
            .as_ref()
            .unwrap()
            .id
            .as_deref(),
        Some("legacy-inline-track")
    );
    assert!(
        client
            .get_playlists_for_current_user()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.get_followed_artists().await.unwrap().is_empty());
    assert!(client.get_saved_albums().await.unwrap().is_empty());
    assert!(client.get_saved_tracks().await.unwrap().is_empty());
    assert!(client.get_saved_shows().await.unwrap().is_empty());
    assert!(
        client
            .get_recently_played_tracks()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.get_top_artists().await.unwrap().is_empty());
    assert!(client.get_top_tracks().await.unwrap().is_empty());
    let entries = client
        .get_playlist_items("spotify:playlist:playlist-1")
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].media().unwrap().kind.as_deref(),
        Some("future_type")
    );
    let album = client.get_album("spotify:album:album-1").await.unwrap();
    assert_eq!(
        album.tracks.as_ref().unwrap().items.as_ref().unwrap()[0]
            .id
            .as_deref(),
        Some("album-track")
    );
    assert!(client.get_album_tracks("album-1").await.unwrap().is_empty());
    client.get_artist("artist-1").await.unwrap();
    assert!(
        client
            .get_artist_albums("artist-1")
            .await
            .unwrap()
            .is_empty()
    );
    client.get_show("show-1").await.unwrap();
    assert!(client.get_show_episodes("show-1").await.unwrap().is_empty());

    let requests = server.requests.await.unwrap();
    assert_eq!(requests.len(), 19);
    let expected = [
        ("/v1/me", vec![]),
        ("/v1/me/player", vec![("additional_types", "track,episode")]),
        ("/v1/me/player/devices", vec![]),
        (
            "/v1/playlists/playlist-1",
            vec![("additional_types", "track,episode")],
        ),
        ("/v1/me/playlists", vec![("limit", "48")]),
        (
            "/v1/me/following",
            vec![("limit", "48"), ("type", "artist")],
        ),
        ("/v1/me/albums", vec![("limit", "48")]),
        ("/v1/me/tracks", vec![("limit", "48")]),
        ("/v1/me/shows", vec![("limit", "48")]),
        ("/v1/me/player/recently-played", vec![("limit", "48")]),
        ("/v1/me/top/artists", vec![("limit", "48")]),
        ("/v1/me/top/tracks", vec![("limit", "48")]),
        ("/v1/playlists/playlist-1/items", vec![("limit", "48")]),
        ("/v1/albums/album-1", vec![]),
        ("/v1/albums/album-1/tracks", vec![("limit", "48")]),
        ("/v1/artists/artist-1", vec![]),
        ("/v1/artists/artist-1/albums", vec![("limit", "48")]),
        ("/v1/shows/show-1", vec![]),
        ("/v1/shows/show-1/episodes", vec![("limit", "48")]),
    ];
    for (request, (path, query)) in requests.iter().zip(expected) {
        assert_request(
            request,
            "GET",
            path,
            &query
                .iter()
                .map(|(key, value)| (*key, *value))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(authorized_calls.load(Ordering::SeqCst), 19);
}

#[tokio::test]
async fn player_writes_use_exact_methods_queries_bodies_and_accept_204() {
    let server = fixture((0..13).map(|_| ResponseSpec::no_content()).collect()).await;
    let (client, _) = new_client(server.base_url.clone());

    client.transfer_playback("device-a").await.unwrap();
    client
        .start_playback(StartPlaybackRequest {
            device_id: Some("device-b".to_owned()),
            context_uri: Some("spotify:playlist:pl-1".to_owned()),
            position_ms: Some(1234),
            position_offset: Some(2),
            uri_offset: Some("spotify:track:t1".to_owned()),
            ..StartPlaybackRequest::default()
        })
        .await
        .unwrap();
    client.resume_playback(Some("device-c")).await.unwrap();
    client
        .play_media(
            PlayMediaTarget::Context("spotify:album:album-1".to_owned()),
            None,
        )
        .await
        .unwrap();
    client
        .play_media(
            PlayMediaTarget::Uris(vec![
                "spotify:track:t1".to_owned(),
                "spotify:episode:e1".to_owned(),
            ]),
            None,
        )
        .await
        .unwrap();
    client.pause_playback(Some("device-pause")).await.unwrap();
    client.next_track(None).await.unwrap();
    client.previous_track(Some("device-prev")).await.unwrap();
    client
        .seek_track(12_345, Some("device-seek"))
        .await
        .unwrap();
    client
        .set_repeat(RepeatMode::Context, Some("device-repeat"))
        .await
        .unwrap();
    client.set_volume(35, None).await.unwrap();
    client
        .set_shuffle(false, Some("device-shuffle"))
        .await
        .unwrap();
    client
        .add_to_queue("spotify:track:queued", Some("device-queue"))
        .await
        .unwrap();

    let requests = server.requests.await.unwrap();
    assert_eq!(requests.len(), 13);
    let expected = [
        (
            "PUT",
            "/v1/me/player",
            vec![],
            json!({"device_ids": ["device-a"]}),
        ),
        (
            "PUT",
            "/v1/me/player/play",
            vec![("device_id", "device-b")],
            json!({
                "position_ms": 1234,
                "context_uri": "spotify:playlist:pl-1",
                "offset": {"uri": "spotify:track:t1"}
            }),
        ),
        (
            "PUT",
            "/v1/me/player/play",
            vec![("device_id", "device-c")],
            json!({"position_ms": 0}),
        ),
        (
            "PUT",
            "/v1/me/player/play",
            vec![],
            json!({"position_ms": 0, "context_uri": "spotify:album:album-1"}),
        ),
        (
            "PUT",
            "/v1/me/player/play",
            vec![],
            json!({"position_ms": 0, "uris": ["spotify:track:t1", "spotify:episode:e1"]}),
        ),
        (
            "PUT",
            "/v1/me/player/pause",
            vec![("device_id", "device-pause")],
            json!(null),
        ),
        ("POST", "/v1/me/player/next", vec![], json!(null)),
        (
            "POST",
            "/v1/me/player/previous",
            vec![("device_id", "device-prev")],
            json!(null),
        ),
        (
            "PUT",
            "/v1/me/player/seek",
            vec![("position_ms", "12345"), ("device_id", "device-seek")],
            json!(null),
        ),
        (
            "PUT",
            "/v1/me/player/repeat",
            vec![("state", "context"), ("device_id", "device-repeat")],
            json!(null),
        ),
        (
            "PUT",
            "/v1/me/player/volume",
            vec![("volume_percent", "35")],
            json!(null),
        ),
        (
            "PUT",
            "/v1/me/player/shuffle",
            vec![("state", "false"), ("device_id", "device-shuffle")],
            json!(null),
        ),
        (
            "POST",
            "/v1/me/player/queue",
            vec![
                ("uri", "spotify:track:queued"),
                ("device_id", "device-queue"),
            ],
            json!(null),
        ),
    ];
    for (request, (method, path, query, body)) in requests.iter().zip(expected) {
        assert_request(
            request,
            method,
            path,
            &query
                .iter()
                .map(|(key, value)| (*key, *value))
                .collect::<Vec<_>>(),
        );
        if request.body.is_empty() {
            assert_eq!(body, Value::Null);
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                body
            );
        }
    }
}

#[tokio::test]
async fn initial_external_token_profile_lookup_is_one_shot_and_redacted() {
    let server = fixture(vec![ResponseSpec::json(json!({
        "account_id": "initial-account",
        "id": "legacy-account"
    }))])
    .await;
    let token = ExternalOAuthToken::from_stored_value(json!({
        "access_token": "callback-access-secret",
        "refresh_token": "callback-refresh-secret",
        "expires_at": i64::MAX,
        "provider_extra": "kept by OAuth layer"
    }))
    .unwrap();
    let client = SpotifyClient::with_base_url(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            // The runtime authorizer is not used by this method.
            token: "runtime-token-must-not-be-used".to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    let profile = client.get_current_user_with_token(&token).await.unwrap();
    assert_eq!(profile.account_id.as_deref(), Some("initial-account"));
    let requests = server.requests.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/v1/me");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer callback-access-secret")
    );
    assert!(!format!("{token:?}").contains("callback-access-secret"));
    assert!(!format!("{token:?}").contains("callback-refresh-secret"));
}

#[tokio::test]
async fn persisted_oauth_session_authorizer_is_sensitive_and_uses_current_token() {
    let server = fixture(vec![ResponseSpec::json(json!({
        "account_id": "session-account",
        "id": "legacy-session-account"
    }))])
    .await;
    let store = Arc::new(MemoryTokenStore::default());
    let token = ExternalOAuthToken::from_stored_value(json!({
        "access_token": "session-access-secret",
        "refresh_token": "session-refresh-secret",
        "expires_at": i64::MAX
    }))
    .unwrap();
    store.store("account", token.to_value()).await.unwrap();
    let factory = OAuthClientFactory::new(store).unwrap();
    let oauth_client = factory
        .client(
            OAuthClientConfig::new(
                "fixture-client-id",
                None,
                Url::parse("http://127.0.0.1:43123/authorize").unwrap(),
                Url::parse("http://127.0.0.1:43123/token").unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let authorizer = Arc::new(OAuthSessionAuthorizer::new(
        oauth_client.session("account").unwrap(),
    ));
    let probe = reqwest::Client::new().get("http://127.0.0.1:43123/probe");
    let request = authorizer.authorize(probe).await.unwrap();
    assert_eq!(
        request.headers()[header::AUTHORIZATION],
        "Bearer session-access-secret"
    );
    assert!(request.headers()[header::AUTHORIZATION].is_sensitive());
    assert!(!format!("{authorizer:?}").contains("session-access-secret"));
    assert!(!format!("{authorizer:?}").contains("session-refresh-secret"));

    let client = SpotifyClient::with_base_url(server.base_url.clone(), authorizer).unwrap();
    assert_eq!(
        client
            .get_current_user()
            .await
            .unwrap()
            .account_id
            .as_deref(),
        Some("session-account")
    );
    assert!(!format!("{client:?}").contains("session-access-secret"));
    assert!(!format!("{client:?}").contains("session-refresh-secret"));
    let requests = server.requests.await.unwrap();
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer session-access-secret")
    );
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn playback_204_is_no_state_while_200_null_item_is_present_state() {
    let server = fixture(vec![
        ResponseSpec::json(json!({
            "device": null,
            "is_playing": false,
            "currently_playing_type": "track",
            "item": null,
            "progress_ms": null
        })),
        ResponseSpec::no_content(),
    ])
    .await;
    let (client, _) = new_client(server.base_url);
    let state = client.get_playback().await.unwrap().unwrap();
    assert_eq!(state.item, None);
    assert_eq!(state.is_playing, Some(false));
    assert_eq!(client.get_playback().await.unwrap(), None);
    server.requests.await.unwrap();
}

#[tokio::test]
async fn empty_lists_are_success_but_missing_payload_fields_are_not() {
    let server = fixture(vec![
        ResponseSpec::json(json!({"devices": []})),
        ResponseSpec::json(json!({"other": []})),
        ResponseSpec::json(json!({"items": [], "next": null, "total": 0})),
        ResponseSpec::json(json!({"next": null, "total": 0})),
        ResponseSpec::json(json!({"items": [], "total": 0})),
        ResponseSpec::json(json!({
            "items": [{"track": {"id": "legacy-track", "type": "track"}}],
            "next": null,
            "total": 1
        })),
    ])
    .await;
    let (client, _) = new_client(server.base_url);
    assert!(client.get_devices().await.unwrap().is_empty());
    assert_eq!(
        client.get_devices().await.unwrap_err(),
        SpotifyError::InvalidPayload
    );
    assert!(client.get_saved_tracks().await.unwrap().is_empty());
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::InvalidPayload
    );
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::InvalidPayload
    );
    let legacy = client.get_playlist_items("playlist-1").await.unwrap();
    assert_eq!(
        legacy[0].media().unwrap().id.as_deref(),
        Some("legacy-track")
    );
    server.requests.await.unwrap();
}

#[tokio::test]
async fn paginated_reads_follow_same_origin_next_and_preserve_all_results() {
    let server = fixture(vec![
        ResponseSpec::json(json!({
            "items": [{"track": {"id": "track-1", "type": "track"}}],
            "next": "/v1/me/tracks?limit=48&offset=48",
            "total": 2
        })),
        ResponseSpec::json(json!({
            "items": [{"track": {"id": "track-2", "type": "track"}}],
            "next": null,
            "total": 2
        })),
    ])
    .await;
    let (client, _) = new_client(server.base_url);
    let tracks = client.get_saved_tracks().await.unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(
        tracks[0].track.as_ref().unwrap().id.as_deref(),
        Some("track-1")
    );
    assert_eq!(
        tracks[1].track.as_ref().unwrap().id.as_deref(),
        Some("track-2")
    );
    let requests = server.requests.await.unwrap();
    assert_request(&requests[0], "GET", "/v1/me/tracks", &[("limit", "48")]);
    assert_request(
        &requests[1],
        "GET",
        "/v1/me/tracks",
        &[("limit", "48"), ("offset", "48")],
    );
}

#[tokio::test]
async fn pagination_limit_incomplete_loop_and_unsafe_origin_are_typed_errors() {
    let page_with_next = || {
        ResponseSpec::json(json!({
            "items": [{"track": {"id": "track-1"}}],
            "next": "/v1/me/tracks?limit=48&offset=48",
            "total": 2
        }))
    };

    let server = fixture(vec![page_with_next()]).await;
    let options = SpotifyClientOptions {
        max_pages: 1,
        ..SpotifyClientOptions::default()
    };
    let client = SpotifyClient::with_options(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        options,
    )
    .unwrap();
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::PaginationLimitExceeded
    );
    server.requests.await.unwrap();

    let server = fixture(vec![ResponseSpec::json(json!({
        "items": [{"track": {"id": "track-1"}}],
        "next": null,
        "total": 2
    }))])
    .await;
    let (client, _) = new_client(server.base_url);
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::PaginationIncomplete
    );
    server.requests.await.unwrap();

    let server = fixture(vec![ResponseSpec::json(json!({
        "items": [{"track": {"id": "track-1"}}],
        "next": "?limit=48",
        "total": 2
    }))])
    .await;
    let (client, _) = new_client(server.base_url);
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::PaginationLoop
    );
    server.requests.await.unwrap();

    let server = fixture(vec![ResponseSpec::json(json!({
        "items": [{"track": {"id": "track-1"}}],
        "next": "https://attacker.example/v1/me/tracks?access_token=leak",
        "total": 2
    }))])
    .await;
    let (client, _) = new_client(server.base_url);
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::UnsafePaginationUrl
    );
    server.requests.await.unwrap();

    let server = fixture(vec![ResponseSpec::json(json!({
        "items": [
            {"track": {"id": "track-1"}},
            {"track": {"id": "track-2"}}
        ],
        "next": null,
        "total": 2
    }))])
    .await;
    let options = SpotifyClientOptions {
        max_page_items: 1,
        ..SpotifyClientOptions::default()
    };
    let client = SpotifyClient::with_options(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        options,
    )
    .unwrap();
    assert_eq!(
        client.get_saved_tracks().await.unwrap_err(),
        SpotifyError::PaginationLimitExceeded
    );
    server.requests.await.unwrap();
}

#[tokio::test]
async fn status_mapping_retry_after_quota_reason_and_provider_secrets_are_sanitized() {
    let server = fixture(vec![
        ResponseSpec::body(401, br#"{"access_token":"provider-body-secret"}"#.to_vec()),
        ResponseSpec::body(
            403,
            br#"{"error":{"message":"Premium required","private":"provider-body-secret"}}"#
                .to_vec(),
        ),
        ResponseSpec::body(
            400,
            br#"{"status":404,"private":"provider-body-secret"}"#.to_vec(),
        ),
        ResponseSpec::body(
            429,
            br#"{"error":{"reason":"QUOTA_EXCEEDED","private":"provider-body-secret"}}"#.to_vec(),
        )
        .header("Retry-After", "17"),
        ResponseSpec::body(400, br#"{"private":"provider-body-secret"}"#.to_vec()),
        ResponseSpec::body(503, br#"{"private":"provider-body-secret"}"#.to_vec()),
        ResponseSpec::body(
            403,
            br#"{"error":{"message":"scope denied","private":"provider-body-secret"}}"#.to_vec(),
        ),
    ])
    .await;
    let (client, _) = new_client(server.base_url);

    let unauthorized = client.get_current_user().await.unwrap_err();
    assert_eq!(unauthorized, SpotifyError::Unauthorized);
    assert!(!format!("{unauthorized:?}").contains("provider-body-secret"));

    let forbidden = client.get_current_user().await.unwrap_err();
    assert_eq!(
        forbidden,
        SpotifyError::Forbidden {
            reason: hass_spotify::ForbiddenReason::PremiumRequired
        }
    );
    assert!(!format!("{forbidden:?}").contains("provider-body-secret"));

    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::NotFound
    );

    let rate = client.get_current_user().await.unwrap_err();
    assert_eq!(
        rate,
        SpotifyError::RateLimited {
            retry_after: Some(Duration::from_secs(17)),
            reason: hass_spotify::RateLimitReason::QuotaExceeded
        }
    );
    assert!(!format!("{rate:?}").contains("provider-body-secret"));

    let invalid = client.get_current_user().await.unwrap_err();
    assert_eq!(invalid, SpotifyError::InvalidRequest);
    assert!(!format!("{invalid:?}").contains("provider-body-secret"));

    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::Transient { status: 503 }
    );
    let queued_error = client
        .add_to_queue(
            "spotify:track:private-query-value",
            Some("private-device-query"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        queued_error,
        SpotifyError::Forbidden {
            reason: hass_spotify::ForbiddenReason::MissingScope
        }
    ));
    let queued_error_debug = format!("{queued_error:?}");
    assert!(!queued_error_debug.contains("private-query-value"));
    assert!(!queued_error_debug.contains("private-device-query"));
    assert!(!queued_error_debug.contains("provider-body-secret"));
    let requests = server.requests.await.unwrap();
    assert_request(
        &requests[6],
        "POST",
        "/v1/me/player/queue",
        &[
            ("uri", "spotify:track:private-query-value"),
            ("device_id", "private-device-query"),
        ],
    );
}

#[tokio::test]
async fn invalid_json_oversized_body_and_timeout_are_not_empty_successes() {
    let oversized = ResponseSpec::body(200, vec![b'x'; 128]);
    let server = fixture(vec![
        ResponseSpec::body(200, b"{not-json".to_vec()),
        oversized,
    ])
    .await;
    let options = SpotifyClientOptions {
        max_response_bytes: 32,
        ..SpotifyClientOptions::default()
    };
    let client = SpotifyClient::with_options(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        options,
    )
    .unwrap();
    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::InvalidPayload
    );
    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::ResponseTooLarge
    );
    server.requests.await.unwrap();

    let server = fixture(vec![
        ResponseSpec::json(json!({"account_id": "unused"})).after(Duration::from_millis(200)),
    ])
    .await;
    let client = SpotifyClient::with_options(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        options_with_timeout(Duration::from_millis(30)),
    )
    .unwrap();
    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::Timeout
    );
    server.requests.await.unwrap();
}

#[tokio::test]
async fn oversized_command_request_body_is_rejected_before_transport() {
    let server = fixture(Vec::new()).await;
    let options = SpotifyClientOptions {
        max_request_body_bytes: 16,
        ..SpotifyClientOptions::default()
    };
    let client = SpotifyClient::with_options(
        server.base_url.clone(),
        Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        options,
    )
    .unwrap();
    assert_eq!(
        client
            .play_media(PlayMediaTarget::Uris(vec!["x".repeat(64)]), None)
            .await
            .unwrap_err(),
        SpotifyError::RequestTooLarge
    );
    assert!(server.requests.await.unwrap().is_empty());
}

#[tokio::test]
async fn caller_cancellation_drops_the_in_flight_api_request() {
    let server = fixture(vec![
        ResponseSpec::json(json!({"account_id": "unused"})).after(Duration::from_millis(150)),
    ])
    .await;
    let (client, _) = new_client(server.base_url);
    let request = tokio::spawn(async move { client.get_current_user().await });
    let result = timeout(Duration::from_millis(30), async {
        loop {
            if request.is_finished() {
                return;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(
        result.is_err(),
        "the request unexpectedly completed before cancellation"
    );
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let requests = server.requests.await.unwrap();
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn credentialed_redirect_is_not_forwarded() {
    let redirect_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_address = redirect_listener.local_addr().unwrap();
    let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target_listener.local_addr().unwrap();
    let target_task = tokio::spawn(async move {
        timeout(Duration::from_millis(250), target_listener.accept())
            .await
            .is_ok()
    });
    let redirect_url = Url::parse(&format!("http://{redirect_address}/v1/")).unwrap();
    let redirect = serve(
        redirect_listener,
        vec![ResponseSpec::body(302, Vec::new()).header(
            "Location",
            &format!("http://{target_address}/steal?secret=query"),
        )],
    );
    let (client, _) = new_client(redirect_url);
    assert_eq!(
        client.get_current_user().await.unwrap_err(),
        SpotifyError::RedirectBlocked
    );
    let requests = redirect.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        !target_task.await.unwrap(),
        "the redirect target received credentials"
    );
}

#[test]
fn injected_bases_require_https_or_literal_loopback_http() {
    for unsafe_base in [
        "http://localhost:1234/v1/",
        "http://192.0.2.4:1234/v1/",
        "http://user:pass@127.0.0.1:1234/v1/",
        "http://127.0.0.1:1234/v1/?token=secret",
        "http://127.0.0.1:1234/v1/#fragment",
        "ftp://127.0.0.1:1234/v1/",
    ] {
        let base = Url::parse(unsafe_base).unwrap();
        assert!(matches!(
            SpotifyClient::with_base_url(
                base,
                Arc::new(FixedAuthorizer {
                    token: TEST_ACCESS_TOKEN.to_owned(),
                    calls: Arc::new(AtomicUsize::new(0)),
                })
            ),
            Err(SpotifyError::InvalidBaseUrl)
        ));
    }
    assert!(
        SpotifyClient::new(Arc::new(FixedAuthorizer {
            token: TEST_ACCESS_TOKEN.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .is_ok()
    );
}
