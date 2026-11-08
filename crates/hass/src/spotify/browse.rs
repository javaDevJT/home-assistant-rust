//! Spotify media browser mapping.

use hass_media_player::{BrowseMedia, BrowseMediaRequest};
use hass_spotify::{Artist, PlaylistItem, SpotifyClient, SpotifyError, SpotifyObject, UserProfile};

const MEDIA_PLAYER_PREFIX: &str = "spotify://";
const CURRENT_USER_PLAYLISTS: &str = "current_user_playlists";
const CURRENT_USER_FOLLOWED_ARTISTS: &str = "current_user_followed_artists";
const CURRENT_USER_SAVED_ALBUMS: &str = "current_user_saved_albums";
const CURRENT_USER_SAVED_TRACKS: &str = "current_user_saved_tracks";
const CURRENT_USER_SAVED_SHOWS: &str = "current_user_saved_shows";
const CURRENT_USER_RECENTLY_PLAYED: &str = "current_user_recently_played";
const CURRENT_USER_TOP_ARTISTS: &str = "current_user_top_artists";
const CURRENT_USER_TOP_TRACKS: &str = "current_user_top_tracks";
const MEDIA_TYPE_SHOW: &str = "show";
const ROOT_THUMBNAIL: &str = "/api/brands/integration/spotify/logo.png";

const LIBRARY_CATEGORIES: [(&str, &str, &str); 8] = [
    (CURRENT_USER_PLAYLISTS, "Playlists", "playlist"),
    (CURRENT_USER_FOLLOWED_ARTISTS, "Artists", "artist"),
    (CURRENT_USER_SAVED_ALBUMS, "Albums", "album"),
    (CURRENT_USER_SAVED_TRACKS, "Liked songs", "track"),
    (CURRENT_USER_SAVED_SHOWS, "Podcasts", "podcast"),
    (CURRENT_USER_RECENTLY_PLAYED, "Recently played", "track"),
    (CURRENT_USER_TOP_ARTISTS, "Top Artists", "artist"),
    (CURRENT_USER_TOP_TRACKS, "Top Tracks", "track"),
];

/// Browse one Spotify tree request using the already-authorized native client.
pub(super) async fn browse(
    client: &SpotifyClient,
    profile: &UserProfile,
    request: BrowseMediaRequest,
) -> Result<BrowseMedia, SpotifyError> {
    let (Some(media_content_type), Some(media_content_id)) = (
        request.media_content_type.as_deref(),
        request.media_content_id.as_deref(),
    ) else {
        if request.media_content_type.is_none() && request.media_content_id.is_none() {
            return root_payload(profile);
        }
        return Err(SpotifyError::InvalidRequest);
    };

    let media_type = media_content_type
        .strip_prefix(MEDIA_PLAYER_PREFIX)
        .unwrap_or(media_content_type);
    let media_id = unwrap_media_id(profile, media_content_id)?;

    if media_type == "library" {
        if !media_id.is_empty() && media_id != "library" {
            return Err(SpotifyError::InvalidRequest);
        }
        return library_payload(profile);
    }

    match media_type {
        CURRENT_USER_PLAYLISTS => {
            validate_category_id(&media_id, CURRENT_USER_PLAYLISTS)?;
            let playlists = client.get_playlists_for_current_user().await?;
            category_payload(
                profile,
                CURRENT_USER_PLAYLISTS,
                playlists
                    .iter()
                    .filter_map(|item| object_item(profile, item, "playlist", true)),
            )
        }
        CURRENT_USER_FOLLOWED_ARTISTS => {
            validate_category_id(&media_id, CURRENT_USER_FOLLOWED_ARTISTS)?;
            let artists = client.get_followed_artists().await?;
            category_payload(
                profile,
                CURRENT_USER_FOLLOWED_ARTISTS,
                artists
                    .iter()
                    .filter_map(|artist| artist_item(profile, artist)),
            )
        }
        CURRENT_USER_SAVED_ALBUMS => {
            validate_category_id(&media_id, CURRENT_USER_SAVED_ALBUMS)?;
            let albums = client.get_saved_albums().await?;
            category_payload(
                profile,
                CURRENT_USER_SAVED_ALBUMS,
                albums.iter().filter_map(|saved| {
                    saved
                        .album
                        .as_ref()
                        .and_then(|album| object_item(profile, album, "album", true))
                }),
            )
        }
        CURRENT_USER_SAVED_TRACKS => {
            validate_category_id(&media_id, CURRENT_USER_SAVED_TRACKS)?;
            let tracks = client.get_saved_tracks().await?;
            category_payload(
                profile,
                CURRENT_USER_SAVED_TRACKS,
                tracks.iter().filter_map(|saved| {
                    saved
                        .track
                        .as_ref()
                        .and_then(|track| object_item(profile, track, "track", true))
                }),
            )
        }
        CURRENT_USER_SAVED_SHOWS => {
            validate_category_id(&media_id, CURRENT_USER_SAVED_SHOWS)?;
            let shows = client.get_saved_shows().await?;
            category_payload(
                profile,
                CURRENT_USER_SAVED_SHOWS,
                shows.iter().filter_map(|saved| {
                    saved
                        .show
                        .as_ref()
                        .and_then(|show| object_item(profile, show, MEDIA_TYPE_SHOW, true))
                }),
            )
        }
        CURRENT_USER_RECENTLY_PLAYED => {
            validate_category_id(&media_id, CURRENT_USER_RECENTLY_PLAYED)?;
            let tracks = client.get_recently_played_tracks().await?;
            category_payload(
                profile,
                CURRENT_USER_RECENTLY_PLAYED,
                tracks.iter().filter_map(|played| {
                    played
                        .track
                        .as_ref()
                        .and_then(|track| object_item(profile, track, "track", true))
                }),
            )
        }
        CURRENT_USER_TOP_ARTISTS => {
            validate_category_id(&media_id, CURRENT_USER_TOP_ARTISTS)?;
            let artists = client.get_top_artists().await?;
            category_payload(
                profile,
                CURRENT_USER_TOP_ARTISTS,
                artists
                    .iter()
                    .filter_map(|artist| artist_item(profile, artist)),
            )
        }
        CURRENT_USER_TOP_TRACKS => {
            validate_category_id(&media_id, CURRENT_USER_TOP_TRACKS)?;
            let tracks = client.get_top_tracks().await?;
            category_payload(
                profile,
                CURRENT_USER_TOP_TRACKS,
                tracks
                    .iter()
                    .filter_map(|track| object_item(profile, track, "track", true)),
            )
        }
        "playlist" => {
            let _playlist_id = spotify_uri_id(&media_id, "playlist")?;
            let playlist = client.get_playlist(&media_id).await?;
            let items = client.get_playlist_items(&media_id).await?;
            let mut response = named_parent_payload(
                profile,
                "playlist",
                required_name(&playlist)?,
                first_image(&playlist),
                &media_id,
            )?;
            response.children = playlist_items(profile, &items);
            Ok(response)
        }
        "album" => {
            let _album_id = spotify_uri_id(&media_id, "album")?;
            let album = client.get_album(&media_id).await?;
            let tracks = client.get_album_tracks(&media_id).await?;
            let mut response = named_parent_payload(
                profile,
                "album",
                required_name(&album)?,
                first_image(&album),
                &media_id,
            )?;
            response.children = tracks
                .iter()
                .filter_map(|track| object_item(profile, track, "track", false))
                .collect();
            Ok(response)
        }
        "artist" => {
            let _artist_id = spotify_uri_id(&media_id, "artist")?;
            let artist = client.get_artist(&media_id).await?;
            let albums = client.get_artist_albums(&media_id).await?;
            let mut response = named_parent_payload(
                profile,
                "artist",
                required_artist_name(&artist)?,
                first_artist_image(&artist),
                &media_id,
            )?;
            response.children = albums
                .iter()
                .filter_map(|album| object_item(profile, album, "album", true))
                .collect();
            Ok(response)
        }
        MEDIA_TYPE_SHOW => {
            let _show_id = spotify_uri_id(&media_id, MEDIA_TYPE_SHOW)?;
            let show = client.get_show(&media_id).await?;
            let episodes = client.get_show_episodes(&media_id).await?;
            let mut response = named_parent_payload(
                profile,
                MEDIA_TYPE_SHOW,
                required_name(&show)?,
                first_image(&show),
                &media_id,
            )?;
            response.children = episodes
                .iter()
                .filter_map(|episode| object_item(profile, episode, "episode", true))
                .collect();
            Ok(response)
        }
        _ => Err(SpotifyError::InvalidRequest),
    }
}

fn root_payload(profile: &UserProfile) -> Result<BrowseMedia, SpotifyError> {
    let mut root = BrowseMedia::new(
        "Spotify",
        "app",
        "spotify",
        MEDIA_PLAYER_PREFIX,
        false,
        true,
    );
    root.thumbnail = Some(ROOT_THUMBNAIL.to_owned());
    if let Some(link_id) = preferred_link_id(profile)? {
        let title = profile
            .display_name
            .as_deref()
            .filter(|value| !value.is_empty())
            .or(profile.id.as_deref().filter(|value| !value.is_empty()))
            .unwrap_or("Spotify account");
        let mut account = BrowseMedia::new(
            title,
            "app",
            format!("{MEDIA_PLAYER_PREFIX}library"),
            format!("{MEDIA_PLAYER_PREFIX}{link_id}"),
            false,
            true,
        );
        account.thumbnail = Some(ROOT_THUMBNAIL.to_owned());
        root.children.push(account);
    }
    Ok(root)
}

fn library_payload(profile: &UserProfile) -> Result<BrowseMedia, SpotifyError> {
    let mut library = BrowseMedia::new(
        "Media Library",
        "directory",
        format!("{MEDIA_PLAYER_PREFIX}library"),
        linked_content_id(profile, "library")?,
        false,
        true,
    );
    library.children_media_class = Some("directory".to_owned());
    for (category, title, children_class) in LIBRARY_CATEGORIES {
        let mut child = BrowseMedia::new(
            title,
            "directory",
            format!("{MEDIA_PLAYER_PREFIX}{category}"),
            linked_content_id(profile, category)?,
            category == CURRENT_USER_SAVED_TRACKS && legacy_user_id(profile).is_some(),
            true,
        );
        child.children_media_class = Some(children_class.to_owned());
        library.children.push(child);
    }
    Ok(library)
}

fn category_payload(
    profile: &UserProfile,
    category: &str,
    children: impl IntoIterator<Item = BrowseMedia>,
) -> Result<BrowseMedia, SpotifyError> {
    let (title, children_class) = category_details(category).ok_or(SpotifyError::InvalidRequest)?;
    let can_play = category == CURRENT_USER_SAVED_TRACKS
        && legacy_user_id(profile).is_some_and(|id| !id.is_empty());
    let mut browse = BrowseMedia::new(
        title,
        "directory",
        format!("{MEDIA_PLAYER_PREFIX}{category}"),
        linked_content_id(profile, category)?,
        can_play,
        true,
    );
    browse.children_media_class = Some(children_class.to_owned());
    browse.children.extend(children);
    Ok(browse)
}

fn category_details(category: &str) -> Option<(&'static str, &'static str)> {
    LIBRARY_CATEGORIES
        .iter()
        .find(|(value, _, _)| *value == category)
        .map(|(_, title, children_class)| (*title, *children_class))
}

fn validate_category_id(id: &str, category: &str) -> Result<(), SpotifyError> {
    if id == category {
        Ok(())
    } else {
        Err(SpotifyError::InvalidRequest)
    }
}

fn named_parent_payload(
    profile: &UserProfile,
    kind: &str,
    title: &str,
    thumbnail: Option<String>,
    media_id: &str,
) -> Result<BrowseMedia, SpotifyError> {
    let (media_class, children_class) = media_classes(kind).ok_or(SpotifyError::InvalidRequest)?;
    let mut browse = BrowseMedia::new(
        title,
        media_class,
        format!("{MEDIA_PLAYER_PREFIX}{kind}"),
        linked_content_id(profile, media_id)?,
        true,
        true,
    );
    browse.children_media_class = children_class.map(str::to_owned);
    browse.thumbnail = thumbnail;
    Ok(browse)
}

fn object_item(
    profile: &UserProfile,
    object: &SpotifyObject,
    expected_kind: &str,
    show_track_thumbnail: bool,
) -> Option<BrowseMedia> {
    let uri = object.uri.as_deref()?;
    let kind = match object.kind.as_deref() {
        Some(kind) => kind,
        None => uri_kind(uri)?,
    };
    if kind != expected_kind || !known_item_uri(uri, kind) {
        return None;
    }
    let title = object.name.as_deref()?;
    let (media_class, children_class) = media_classes(kind)?;
    let mut item = BrowseMedia::new(
        title,
        media_class,
        format!("{MEDIA_PLAYER_PREFIX}{kind}"),
        linked_content_id(profile, uri).ok()?,
        true,
        !matches!(kind, "track" | "episode"),
    );
    item.children_media_class = children_class.map(str::to_owned);
    item.thumbnail = match kind {
        "track" if show_track_thumbnail => object.album.as_deref().and_then(first_image),
        "playlist" | "album" | "artist" | "show" | "episode" => first_image(object),
        _ => None,
    };
    Some(item)
}

fn artist_item(profile: &UserProfile, artist: &Artist) -> Option<BrowseMedia> {
    if artist.kind.as_deref().is_some_and(|kind| kind != "artist") {
        return None;
    }
    let uri = artist.uri.as_deref()?;
    if !known_item_uri(uri, "artist") {
        return None;
    }
    let title = artist.name.as_deref()?;
    let (media_class, children_class) = media_classes("artist")?;
    let mut item = BrowseMedia::new(
        title,
        media_class,
        format!("{MEDIA_PLAYER_PREFIX}artist"),
        linked_content_id(profile, uri).ok()?,
        true,
        true,
    );
    item.children_media_class = children_class.map(str::to_owned);
    item.thumbnail = first_artist_image(artist);
    Some(item)
}

fn playlist_items(profile: &UserProfile, items: &[PlaylistItem]) -> Vec<BrowseMedia> {
    items
        .iter()
        .filter_map(PlaylistItem::media)
        .filter_map(|item| {
            let kind = item
                .kind
                .as_deref()
                .or_else(|| item.uri.as_deref().and_then(uri_kind))?;
            if !matches!(kind, "track" | "episode") {
                return None;
            }
            object_item(profile, item, kind, true)
        })
        .collect()
}

fn media_classes(kind: &str) -> Option<(&'static str, Option<&'static str>)> {
    match kind {
        "playlist" => Some(("playlist", Some("track"))),
        "album" => Some(("album", Some("track"))),
        "artist" => Some(("artist", Some("album"))),
        "show" => Some(("podcast", Some("episode"))),
        "track" => Some(("track", None)),
        "episode" => Some(("episode", None)),
        _ => None,
    }
}

fn uri_kind(uri: &str) -> Option<&str> {
    let mut parts = uri.split(':');
    (parts.next()? == "spotify").then(|| parts.next()).flatten()
}

fn known_item_uri(uri: &str, kind: &str) -> bool {
    let Some(uri_kind) = uri_kind(uri) else {
        return false;
    };
    if uri_kind == kind {
        spotify_uri_id(uri, kind).is_ok()
    } else if kind == "track" && uri_kind == "local" {
        uri.strip_prefix("spotify:local:")
            .is_some_and(|local_id| !local_id.is_empty() && !local_id.contains('/'))
    } else {
        false
    }
}

fn spotify_uri_id<'a>(uri: &'a str, expected_kind: &str) -> Result<&'a str, SpotifyError> {
    let mut parts = uri.split(':');
    if parts.next() != Some("spotify") || parts.next() != Some(expected_kind) {
        return Err(SpotifyError::InvalidRequest);
    }
    let Some(id) = parts.next() else {
        return Err(SpotifyError::InvalidRequest);
    };
    if parts.next().is_some() || !valid_segment(id) {
        return Err(SpotifyError::InvalidRequest);
    }
    Ok(id)
}

fn unwrap_media_id(profile: &UserProfile, media_id: &str) -> Result<String, SpotifyError> {
    let Some(rest) = media_id.strip_prefix(MEDIA_PLAYER_PREFIX) else {
        return Ok(media_id.to_owned());
    };
    let (account_id, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !valid_segment(account_id)
        || !path.is_empty()
            && (path.contains('/')
                || path
                    .chars()
                    .any(|character| matches!(character, '?' | '#' | '\\')))
    {
        return Err(SpotifyError::InvalidRequest);
    }
    let linked_id = preferred_link_id(profile)?;
    let allowed = linked_id == Some(account_id)
        || profile
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_some_and(|id| id == account_id);
    if !allowed {
        return Err(SpotifyError::InvalidRequest);
    }
    decode_path_segment(path)
}

/// Decode the final URL path segment like the pinned `yarl.URL.name` helper.
/// Spotify URI colons can arrive as `%3A` after URL transport.
fn decode_path_segment(segment: &str) -> Result<String, SpotifyError> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = bytes
            .get(index + 1)
            .and_then(|byte| hex_value(*byte))
            .ok_or(SpotifyError::InvalidRequest)?;
        let low = bytes
            .get(index + 2)
            .and_then(|byte| hex_value(*byte))
            .ok_or(SpotifyError::InvalidRequest)?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| SpotifyError::InvalidRequest)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn preferred_link_id(profile: &UserProfile) -> Result<Option<&str>, SpotifyError> {
    let id = profile
        .account_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or_else(|| profile.id.as_deref().filter(|id| !id.is_empty()));
    if id.is_some_and(|id| !valid_segment(id)) {
        return Err(SpotifyError::InvalidPayload);
    }
    Ok(id)
}

fn legacy_user_id(profile: &UserProfile) -> Option<&str> {
    profile.id.as_deref().filter(|id| !id.is_empty())
}

fn linked_content_id(profile: &UserProfile, suffix: &str) -> Result<String, SpotifyError> {
    if suffix.is_empty()
        || suffix.starts_with(MEDIA_PLAYER_PREFIX)
        || suffix
            .chars()
            .any(|character| matches!(character, '?' | '#' | '\\'))
        || suffix.split('/').count() != 1
    {
        return Err(SpotifyError::InvalidRequest);
    }
    match preferred_link_id(profile)? {
        Some(link_id) => Ok(format!("{MEDIA_PLAYER_PREFIX}{link_id}/{suffix}")),
        None => Ok(suffix.to_owned()),
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn required_name(object: &SpotifyObject) -> Result<&str, SpotifyError> {
    object
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or(SpotifyError::InvalidPayload)
}

fn required_artist_name(artist: &Artist) -> Result<&str, SpotifyError> {
    artist
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or(SpotifyError::InvalidPayload)
}

fn first_image(object: &SpotifyObject) -> Option<String> {
    object
        .images
        .as_ref()?
        .first()?
        .url
        .as_ref()
        .filter(|url| !url.is_empty())
        .cloned()
}

fn first_artist_image(artist: &Artist) -> Option<String> {
    artist
        .images
        .as_ref()?
        .first()?
        .url
        .as_ref()
        .filter(|url| !url.is_empty())
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use axum::{
        Json, Router,
        extract::State,
        http::{Method, Uri, header},
        routing::any,
    };
    use hass_spotify::{AuthorizationError, RequestAuthorizer};
    use reqwest::{Request, RequestBuilder};
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use url::Url;

    use super::*;

    #[derive(Clone, Default)]
    struct FixtureState {
        requests: Arc<Mutex<Vec<String>>>,
        unsafe_page: Arc<std::sync::atomic::AtomicBool>,
    }

    struct FixtureAuthorizer {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RequestAuthorizer for FixtureAuthorizer {
        async fn authorize(&self, request: RequestBuilder) -> Result<Request, AuthorizationError> {
            let mut request = request
                .build()
                .map_err(|_| AuthorizationError::InvalidRequest)?;
            let mut authorization = header::HeaderValue::from_static("Bearer loopback-fixture");
            authorization.set_sensitive(true);
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, authorization);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(request)
        }
    }

    struct FixtureServer {
        client: SpotifyClient,
        requests: Arc<Mutex<Vec<String>>>,
        unsafe_page: Arc<std::sync::atomic::AtomicBool>,
        authorization_calls: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    impl Drop for FixtureServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start_fixture_server() -> FixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let state = FixtureState::default();
        let requests = state.requests.clone();
        let unsafe_page = state.unsafe_page.clone();
        let router = Router::new()
            .fallback(any(fixture_handler))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let authorization_calls = Arc::new(AtomicUsize::new(0));
        let authorizer = FixtureAuthorizer {
            calls: authorization_calls.clone(),
        };
        let base_url = Url::parse(&format!("http://{address}/v1/")).unwrap();
        let client = SpotifyClient::with_base_url(base_url, Arc::new(authorizer)).unwrap();
        FixtureServer {
            client,
            requests,
            unsafe_page,
            authorization_calls,
            task,
        }
    }

    async fn fixture_handler(
        State(state): State<FixtureState>,
        method: Method,
        uri: Uri,
    ) -> Json<Value> {
        state
            .requests
            .lock()
            .unwrap()
            .push(format!("{} {}", method, uri));
        Json(match uri.path() {
            "/v1/me/playlists" => {
                if state.unsafe_page.load(Ordering::SeqCst) {
                    json!({
                        "items": [playlist_object()],
                        "next": "https://outside.example/v1/me/playlists?offset=48",
                        "total": 2
                    })
                } else {
                    page(json!([playlist_object()]))
                }
            }
            "/v1/me/following" => json!({"artists": page(json!([artist_object()]))}),
            "/v1/me/albums" => page(json!([
                {"album": album_object()},
                {"album": null},
                {"album": {"id":"future","type":"future","uri":"spotify:future:future","name":"Future"}}
            ])),
            "/v1/me/tracks" => page(json!([
                {"track": track_object()},
                {"track": null},
                {"track": {"id":"future","type":"future","uri":"spotify:future:future","name":"Future"}}
            ])),
            "/v1/me/shows" => page(json!([{"show": show_object()}, {"show": null}])),
            "/v1/me/player/recently-played" => page(json!([
                {"track": track_object()},
                {"track": null}
            ])),
            "/v1/me/top/artists" => page(json!([artist_object()])),
            "/v1/me/top/tracks" => page(json!([track_object()])),
            "/v1/playlists/pl1" => playlist_object(),
            "/v1/playlists/pl1/items" => page(json!([
                {"item": track_object()},
                {"item": null},
                {"track": episode_object()},
                {"item": {"id":"future","type":"future","uri":"spotify:future:future","name":"Future"}}
            ])),
            "/v1/albums/al1" => album_object(),
            "/v1/albums/al1/tracks" => page(json!([track_object()])),
            "/v1/artists/ar1" => artist_object(),
            "/v1/artists/ar1/albums" => page(json!([album_object()])),
            "/v1/shows/sh1" => show_object(),
            "/v1/shows/sh1/episodes" => page(json!([episode_object()])),
            _ => json!({"items": [], "next": null}),
        })
    }

    fn page(items: Value) -> Value {
        json!({"items": items, "next": null})
    }

    fn track_object() -> Value {
        json!({
            "id": "t1", "type": "track", "uri": "spotify:track:t1", "name": "Song",
            "album": {"id":"al1", "type":"album", "uri":"spotify:album:al1", "name":"Album",
                "images":[{"url":"https://images.example/song.jpg"}]}
        })
    }

    fn episode_object() -> Value {
        json!({
            "id": "e1", "type": "episode", "uri": "spotify:episode:e1", "name": "Episode",
            "images":[{"url":"https://images.example/episode.jpg"}]
        })
    }

    fn playlist_object() -> Value {
        json!({
            "id": "pl1", "type": "playlist", "uri": "spotify:playlist:pl1", "name": "List",
            "images":[{"url":"https://images.example/playlist.jpg"}]
        })
    }

    fn album_object() -> Value {
        json!({
            "id": "al1", "type": "album", "uri": "spotify:album:al1", "name": "Album",
            "images":[{"url":"https://images.example/album.jpg"}]
        })
    }

    fn artist_object() -> Value {
        json!({
            "id": "ar1", "type": "artist", "uri": "spotify:artist:ar1", "name": "Artist",
            "images":[{"url":"https://images.example/artist.jpg"}]
        })
    }

    fn show_object() -> Value {
        json!({
            "id": "sh1", "type": "show", "uri": "spotify:show:sh1", "name": "Show",
            "images":[{"url":"https://images.example/show.jpg"}]
        })
    }

    fn profile() -> UserProfile {
        serde_json::from_value(json!({
            "account_id":"new-account", "id":"legacy-user", "display_name":"Living Room"
        }))
        .unwrap()
    }

    fn request(media_type: &str, media_id: &str) -> BrowseMediaRequest {
        BrowseMediaRequest {
            media_content_type: Some(media_type.to_owned()),
            media_content_id: Some(media_id.to_owned()),
        }
    }

    #[tokio::test]
    async fn root_and_library_are_local_and_keep_link_hierarchy() {
        let server = start_fixture_server().await;
        let profile = profile();

        let root = browse(
            &server.client,
            &profile,
            BrowseMediaRequest {
                media_content_type: None,
                media_content_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(root.title, "Spotify");
        assert_eq!(root.media_class, "app");
        assert_eq!(root.media_content_type, "spotify");
        assert_eq!(root.media_content_id, MEDIA_PLAYER_PREFIX);
        assert!(!root.can_play);
        assert!(root.can_expand);
        assert_eq!(
            root.thumbnail.as_deref(),
            Some("/api/brands/integration/spotify/logo.png")
        );
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].title, "Living Room");
        assert_eq!(root.children[0].media_class, "app");
        assert_eq!(root.children[0].media_content_type, "spotify://library");
        assert_eq!(root.children[0].media_content_id, "spotify://new-account");

        let library = browse(
            &server.client,
            &profile,
            request("spotify://library", "spotify://new-account"),
        )
        .await
        .unwrap();
        assert_eq!(library.title, "Media Library");
        assert_eq!(library.media_class, "directory");
        assert_eq!(library.media_content_id, "spotify://new-account/library");
        assert_eq!(library.children.len(), 8);
        assert_eq!(library.children[0].title, "Playlists");
        assert_eq!(
            library.children[0].children_media_class.as_deref(),
            Some("playlist")
        );
        assert!(library.children[0].can_expand);
        assert!(!library.children[0].can_play);
        assert_eq!(library.children[3].title, "Liked songs");
        assert!(library.children[3].can_expand);
        assert!(library.children[3].can_play);
        let serialized = library.to_service_response();
        assert_eq!(serialized["not_shown"], 0);
        assert_eq!(serialized["children"].as_array().unwrap().len(), 8);
        assert!(serialized["children"][0].get("children").is_none());
        assert!(serialized["children"][0].get("not_shown").is_none());
        assert!(server.requests.lock().unwrap().is_empty());
        assert_eq!(server.authorization_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_pinned_collection_and_detail_branches_use_current_get_routes() {
        let server = start_fixture_server().await;
        let profile = profile();

        let categories = [
            ("current_user_playlists", "Playlists", "playlist"),
            ("current_user_followed_artists", "Artists", "artist"),
            ("current_user_saved_albums", "Albums", "album"),
            ("current_user_saved_tracks", "Liked songs", "track"),
            ("current_user_saved_shows", "Podcasts", "podcast"),
            ("current_user_recently_played", "Recently played", "track"),
            ("current_user_top_artists", "Top Artists", "artist"),
            ("current_user_top_tracks", "Top Tracks", "track"),
        ];
        for (slug, title, child_class) in categories {
            let result = browse(
                &server.client,
                &profile,
                request(
                    &format!("spotify://{slug}"),
                    &format!("spotify://new-account/{slug}"),
                ),
            )
            .await
            .unwrap();
            assert_eq!(result.title, title);
            assert_eq!(result.media_class, "directory");
            assert_eq!(result.media_content_type, format!("spotify://{slug}"));
            assert_eq!(
                result.media_content_id,
                format!("spotify://new-account/{slug}")
            );
            assert_eq!(result.children_media_class.as_deref(), Some(child_class));
            assert!(result.can_expand);
            assert_eq!(result.can_play, slug == "current_user_saved_tracks");
            assert_eq!(result.children.len(), 1, "{slug}");
            assert_eq!(result.children[0].media_class, child_class_for_item(slug));
            assert!(result.children[0].can_play);
        }

        let playlist = browse(
            &server.client,
            &profile,
            request(
                "spotify://playlist",
                "spotify://new-account/spotify%3Aplaylist%3Apl1",
            ),
        )
        .await
        .unwrap();
        assert_eq!(playlist.title, "List");
        assert_eq!(playlist.media_class, "playlist");
        assert_eq!(
            playlist.media_content_id,
            "spotify://new-account/spotify:playlist:pl1"
        );
        assert_eq!(playlist.children_media_class.as_deref(), Some("track"));
        assert_eq!(
            playlist.thumbnail.as_deref(),
            Some("https://images.example/playlist.jpg")
        );
        assert_eq!(playlist.children.len(), 2);
        assert_eq!(playlist.children[0].title, "Song");
        assert_eq!(
            playlist.children[0].media_content_id,
            "spotify://new-account/spotify:track:t1"
        );
        assert_eq!(
            playlist.children[0].thumbnail.as_deref(),
            Some("https://images.example/song.jpg")
        );
        assert!(!playlist.children[0].can_expand);
        assert_eq!(playlist.children[1].title, "Episode");
        assert_eq!(playlist.children[1].media_class, "episode");
        assert_eq!(
            playlist.children[1].thumbnail.as_deref(),
            Some("https://images.example/episode.jpg")
        );

        let album = browse(
            &server.client,
            &profile,
            request("spotify://album", "spotify://new-account/spotify:album:al1"),
        )
        .await
        .unwrap();
        assert_eq!(album.title, "Album");
        assert_eq!(album.media_class, "album");
        assert_eq!(album.children.len(), 1);
        assert_eq!(album.children[0].thumbnail, None);

        let artist = browse(
            &server.client,
            &profile,
            request(
                "spotify://artist",
                "spotify://new-account/spotify:artist:ar1",
            ),
        )
        .await
        .unwrap();
        assert_eq!(artist.title, "Artist");
        assert_eq!(artist.media_class, "artist");
        assert_eq!(
            artist.thumbnail.as_deref(),
            Some("https://images.example/artist.jpg")
        );
        assert_eq!(artist.children_media_class.as_deref(), Some("album"));

        let show = browse(
            &server.client,
            &profile,
            request("spotify://show", "spotify://new-account/spotify:show:sh1"),
        )
        .await
        .unwrap();
        assert_eq!(show.title, "Show");
        assert_eq!(show.media_class, "podcast");
        assert_eq!(show.children_media_class.as_deref(), Some("episode"));
        assert_eq!(show.children[0].title, "Episode");

        let requests = server.requests.lock().unwrap().clone();
        let expected = [
            "GET /v1/me/playlists?limit=48",
            "GET /v1/me/following?limit=48&type=artist",
            "GET /v1/me/albums?limit=48",
            "GET /v1/me/tracks?limit=48",
            "GET /v1/me/shows?limit=48",
            "GET /v1/me/player/recently-played?limit=48",
            "GET /v1/me/top/artists?limit=48",
            "GET /v1/me/top/tracks?limit=48",
            "GET /v1/playlists/pl1?additional_types=track%2Cepisode",
            "GET /v1/playlists/pl1/items?limit=48",
            "GET /v1/albums/al1",
            "GET /v1/albums/al1/tracks?limit=48",
            "GET /v1/artists/ar1",
            "GET /v1/artists/ar1/albums?limit=48",
            "GET /v1/shows/sh1",
            "GET /v1/shows/sh1/episodes?limit=48",
        ];
        assert_eq!(requests.len(), expected.len());
        for route in expected {
            assert!(
                requests.iter().any(|request| request == route),
                "missing {route}: {requests:?}"
            );
        }
        assert_eq!(
            server.authorization_calls.load(Ordering::SeqCst),
            expected.len()
        );
    }

    #[tokio::test]
    async fn missing_legacy_profile_id_keeps_saved_tracks_browsable_but_not_playable() {
        let server = start_fixture_server().await;
        let mut profile = profile();
        profile.id = None;

        let library = browse(
            &server.client,
            &profile,
            request("spotify://library", "spotify://new-account"),
        )
        .await
        .unwrap();
        let folder = &library.children[3];
        assert_eq!(
            folder.media_content_type,
            "spotify://current_user_saved_tracks"
        );
        assert_eq!(
            folder.media_content_id,
            "spotify://new-account/current_user_saved_tracks"
        );
        assert!(folder.can_expand);
        assert!(!folder.can_play);

        let tracks = browse(
            &server.client,
            &profile,
            request(
                "spotify://current_user_saved_tracks",
                "spotify://new-account/current_user_saved_tracks",
            ),
        )
        .await
        .unwrap();
        assert_eq!(tracks.children.len(), 1);
        assert!(tracks.children[0].can_play);
        assert_eq!(
            tracks.children[0].media_content_id,
            "spotify://new-account/spotify:track:t1"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn incomplete_or_unsafe_pagination_errors_are_not_returned_as_partial_trees() {
        let server = start_fixture_server().await;
        server.unsafe_page.store(true, Ordering::SeqCst);
        let result = browse(
            &server.client,
            &profile(),
            request(
                "spotify://current_user_playlists",
                "spotify://new-account/current_user_playlists",
            ),
        )
        .await;
        assert_eq!(result.unwrap_err(), SpotifyError::UnsafePaginationUrl);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert_eq!(server.authorization_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_or_unknown_ids_fail_before_authorization_or_http() {
        let authorization_calls = Arc::new(AtomicUsize::new(0));
        let client = SpotifyClient::with_base_url(
            Url::parse("http://127.0.0.1:9/v1/").unwrap(),
            Arc::new(FixtureAuthorizer {
                calls: authorization_calls.clone(),
            }),
        )
        .unwrap();
        let profile = profile();
        for request in [
            request(
                "spotify://playlist",
                "spotify://other-account/spotify:playlist:pl1",
            ),
            request(
                "spotify://playlist",
                "spotify://new-account/spotify:album:al1",
            ),
            request(
                "spotify://playlist",
                "spotify://new-account/spotify:playlist:bad/id",
            ),
            request(
                "spotify://playlist",
                "spotify://new-account/spotify%3Aplaylist%3A%ZZ",
            ),
            request("spotify://unknown", "spotify://new-account/unknown"),
        ] {
            assert_eq!(
                browse(&client, &profile, request).await.unwrap_err(),
                SpotifyError::InvalidRequest
            );
        }
        assert_eq!(
            browse(
                &client,
                &profile,
                BrowseMediaRequest {
                    media_content_type: Some("spotify://album".into()),
                    media_content_id: None,
                },
            )
            .await
            .unwrap_err(),
            SpotifyError::InvalidRequest
        );
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 0);
    }

    fn child_class_for_item(category: &str) -> &'static str {
        match category {
            "current_user_playlists" => "playlist",
            "current_user_followed_artists" | "current_user_top_artists" => "artist",
            "current_user_saved_albums" | "current_user_top_tracks" => {
                if category == "current_user_saved_albums" {
                    "album"
                } else {
                    "track"
                }
            }
            "current_user_saved_shows" => "podcast",
            "current_user_saved_tracks" | "current_user_recently_played" => "track",
            _ => unreachable!(),
        }
    }
}
