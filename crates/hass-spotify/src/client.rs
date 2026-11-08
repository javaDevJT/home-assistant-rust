use std::{collections::HashSet, fmt, net::IpAddr, time::Duration};

use hass_oauth2_client::ExternalOAuthToken;
use reqwest::{
    Client, Method, RequestBuilder, Response, StatusCode,
    header::{AUTHORIZATION, CONTENT_LENGTH, RETRY_AFTER},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use url::Url;

use crate::{
    Artist, AuthorizationError, ForbiddenReason, PlayedTrack, Playlist, PlaylistItem, Profile,
    RateLimitReason, RequestAuthorizer, SavedAlbum, SavedShow, SavedTrack, Show, SpotifyError,
    SpotifyObject, UserProfile,
    authorization::InitialTokenAuthorizer,
    models::{Device, PlaybackState},
};

/// Default Spotify Web API base.
pub const DEFAULT_API_BASE_URL: &str = "https://api.spotify.com/v1/";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_PAGES: usize = 50;
const DEFAULT_MAX_PAGE_ITEMS: usize = 48;
const HARD_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const HARD_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const HARD_MAX_PAGES: usize = 100;
const HARD_MAX_PAGE_ITEMS: usize = 50;

/// Bounds for each API request and paginated read.
#[derive(Clone, Debug)]
pub struct SpotifyClientOptions {
    /// Total time allowed for the request including response-body reads.
    pub request_timeout: Duration,
    /// Maximum accepted response bytes per request.
    pub max_response_bytes: usize,
    /// Maximum serialized JSON request body bytes.
    pub max_request_body_bytes: usize,
    /// Maximum number of pages fetched by a list method.
    pub max_pages: usize,
    /// Maximum item count accepted on one page.
    pub max_page_items: usize,
}

impl Default for SpotifyClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_pages: DEFAULT_MAX_PAGES,
            max_page_items: DEFAULT_MAX_PAGE_ITEMS,
        }
    }
}

impl SpotifyClientOptions {
    fn validate(&self) -> Result<(), SpotifyError> {
        if self.request_timeout.is_zero()
            || self.request_timeout > Duration::from_secs(300)
            || self.max_response_bytes == 0
            || self.max_response_bytes > HARD_MAX_RESPONSE_BYTES
            || self.max_request_body_bytes == 0
            || self.max_request_body_bytes > HARD_MAX_REQUEST_BYTES
            || self.max_pages == 0
            || self.max_pages > HARD_MAX_PAGES
            || self.max_page_items == 0
            || self.max_page_items > HARD_MAX_PAGE_ITEMS
        {
            return Err(SpotifyError::InvalidRequest);
        }
        Ok(())
    }
}

/// Request to start playback, used by play, resume, and play-media.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StartPlaybackRequest {
    pub device_id: Option<String>,
    pub context_uri: Option<String>,
    pub uris: Option<Vec<String>>,
    pub position_ms: Option<u64>,
    pub position_offset: Option<u32>,
    pub uri_offset: Option<String>,
}

/// Play-media target selected by Home Assistant's media type mapper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayMediaTarget {
    /// Play a playlist, album, artist, show, or other context URI.
    Context(String),
    /// Play one or more track or episode URIs.
    Uris(Vec<String>),
}

/// Spotify repeat modes accepted by the Player API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepeatMode {
    Off,
    Context,
    Track,
}

impl RepeatMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Context => "context",
            Self::Track => "track",
        }
    }
}

/// Native HTTP client for the pinned Spotify account, playback, browse, and
/// Player API operations.
#[derive(Clone)]
pub struct SpotifyClient {
    http: Client,
    base_url: Url,
    authorizer: std::sync::Arc<dyn RequestAuthorizer>,
    options: SpotifyClientOptions,
}

impl fmt::Debug for SpotifyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpotifyClient")
            .field("base_url", &self.base_url)
            .field("authorizer", &"[redacted]")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl SpotifyClient {
    /// Create a client using Spotify's production API base.
    pub fn new(authorizer: std::sync::Arc<dyn RequestAuthorizer>) -> Result<Self, SpotifyError> {
        Self::with_options(
            Url::parse(DEFAULT_API_BASE_URL).map_err(|_| SpotifyError::InvalidBaseUrl)?,
            authorizer,
            SpotifyClientOptions::default(),
        )
    }

    /// Create a client with an injected base URL. HTTPS is required except for
    /// literal loopback IPs, which support owned protocol fixtures.
    pub fn with_base_url(
        base_url: Url,
        authorizer: std::sync::Arc<dyn RequestAuthorizer>,
    ) -> Result<Self, SpotifyError> {
        Self::with_options(base_url, authorizer, SpotifyClientOptions::default())
    }

    /// Create a client with explicit but still hard-bounded request options.
    pub fn with_options(
        mut base_url: Url,
        authorizer: std::sync::Arc<dyn RequestAuthorizer>,
        options: SpotifyClientOptions,
    ) -> Result<Self, SpotifyError> {
        validate_base_url(&mut base_url)?;
        options.validate()?;
        let http = Client::builder()
            .timeout(options.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SpotifyError::Transport)?;
        Ok(Self {
            http,
            base_url,
            authorizer,
            options,
        })
    }

    /// Fetch the current profile using the runtime OAuth session.
    pub async fn get_current_user(&self) -> Result<UserProfile, SpotifyError> {
        self.get_json(self.endpoint(&["me"])?, &[], self.authorizer.as_ref())
            .await
    }

    /// Fetch the profile once with a validated callback token before it is
    /// committed to host storage.
    pub async fn get_current_user_with_token(
        &self,
        token: &ExternalOAuthToken,
    ) -> Result<Profile, SpotifyError> {
        let authorizer = InitialTokenAuthorizer::new(token);
        self.get_json(self.endpoint(&["me"])?, &[], &authorizer)
            .await
    }

    /// Fetch current playback. HTTP 204 means there is no active playback;
    /// HTTP 200 with a null `item` remains a present state.
    pub async fn get_playback(&self) -> Result<Option<PlaybackState>, SpotifyError> {
        let url = self.endpoint(&["me", "player"])?;
        let query = [("additional_types", "track,episode")];
        let response = self
            .send(Method::GET, url, &query, None, self.authorizer.as_ref())
            .await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        self.decode_json(response).await.map(Some)
    }

    /// Fetch the current Connect device list.
    pub async fn get_devices(&self) -> Result<Vec<Device>, SpotifyError> {
        #[derive(Deserialize)]
        struct DeviceResponse {
            devices: Option<Vec<Device>>,
        }
        let response: DeviceResponse = self
            .get_json(
                self.endpoint(&["me", "player", "devices"])?,
                &[],
                self.authorizer.as_ref(),
            )
            .await?;
        response.devices.ok_or(SpotifyError::InvalidPayload)
    }

    /// Fetch playlist metadata. The integration uses this for playlist context
    /// enrichment and the browser also reads its inline items page.
    pub async fn get_playlist(&self, playlist_id: &str) -> Result<Playlist, SpotifyError> {
        let id = spotify_identifier(playlist_id)?;
        let url = self.endpoint(&["playlists", &id])?;
        self.get_json(
            url,
            &[("additional_types", "track,episode")],
            self.authorizer.as_ref(),
        )
        .await
    }

    /// Fetch all current-user playlists.
    pub async fn get_playlists_for_current_user(&self) -> Result<Vec<SpotifyObject>, SpotifyError> {
        self.get_pages(&["me", "playlists"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch artists followed by the current user.
    pub async fn get_followed_artists(&self) -> Result<Vec<Artist>, SpotifyError> {
        self.get_pages(
            &["me", "following"],
            &[("limit", "48"), ("type", "artist")],
            &["artists"],
        )
        .await
    }

    /// Fetch the current user's saved albums.
    pub async fn get_saved_albums(&self) -> Result<Vec<SavedAlbum>, SpotifyError> {
        self.get_pages(&["me", "albums"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch the current user's saved tracks.
    pub async fn get_saved_tracks(&self) -> Result<Vec<SavedTrack>, SpotifyError> {
        self.get_pages(&["me", "tracks"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch the current user's saved shows.
    pub async fn get_saved_shows(&self) -> Result<Vec<SavedShow>, SpotifyError> {
        self.get_pages(&["me", "shows"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch recently played tracks.
    pub async fn get_recently_played_tracks(&self) -> Result<Vec<PlayedTrack>, SpotifyError> {
        self.get_pages(
            &["me", "player", "recently-played"],
            &[("limit", "48")],
            &[],
        )
        .await
    }

    /// Fetch the current user's top artists.
    pub async fn get_top_artists(&self) -> Result<Vec<Artist>, SpotifyError> {
        self.get_pages(&["me", "top", "artists"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch the current user's top tracks.
    pub async fn get_top_tracks(&self) -> Result<Vec<SpotifyObject>, SpotifyError> {
        self.get_pages(&["me", "top", "tracks"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch all current playlist items using Spotify's current `/items` route.
    pub async fn get_playlist_items(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<PlaylistItem>, SpotifyError> {
        let id = spotify_identifier(playlist_id)?;
        self.get_pages(&["playlists", &id, "items"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch an album.
    pub async fn get_album(&self, album_id: &str) -> Result<SpotifyObject, SpotifyError> {
        let id = spotify_identifier(album_id)?;
        self.get_json(
            self.endpoint(&["albums", &id])?,
            &[],
            self.authorizer.as_ref(),
        )
        .await
    }

    /// Fetch all tracks for an album.
    pub async fn get_album_tracks(
        &self,
        album_id: &str,
    ) -> Result<Vec<SpotifyObject>, SpotifyError> {
        let id = spotify_identifier(album_id)?;
        self.get_pages(&["albums", &id, "tracks"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch an artist.
    pub async fn get_artist(&self, artist_id: &str) -> Result<Artist, SpotifyError> {
        let id = spotify_identifier(artist_id)?;
        self.get_json(
            self.endpoint(&["artists", &id])?,
            &[],
            self.authorizer.as_ref(),
        )
        .await
    }

    /// Fetch all albums for an artist.
    pub async fn get_artist_albums(
        &self,
        artist_id: &str,
    ) -> Result<Vec<SpotifyObject>, SpotifyError> {
        let id = spotify_identifier(artist_id)?;
        self.get_pages(&["artists", &id, "albums"], &[("limit", "48")], &[])
            .await
    }

    /// Fetch a show.
    pub async fn get_show(&self, show_id: &str) -> Result<Show, SpotifyError> {
        let id = spotify_identifier(show_id)?;
        self.get_json(
            self.endpoint(&["shows", &id])?,
            &[],
            self.authorizer.as_ref(),
        )
        .await
    }

    /// Fetch all episodes for a show.
    pub async fn get_show_episodes(
        &self,
        show_id: &str,
    ) -> Result<Vec<SpotifyObject>, SpotifyError> {
        let id = spotify_identifier(show_id)?;
        self.get_pages(&["shows", &id, "episodes"], &[("limit", "48")], &[])
            .await
    }

    /// Transfer playback to a device.
    pub async fn transfer_playback(&self, device_id: &str) -> Result<(), SpotifyError> {
        let body = serde_json::json!({"device_ids": [device_id]});
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player"])?,
            &[],
            Some(&body),
        )
        .await
    }

    /// Start or resume playback with the upstream-compatible request fields.
    pub async fn start_playback(&self, request: StartPlaybackRequest) -> Result<(), SpotifyError> {
        let mut body = Map::new();
        body.insert(
            "position_ms".to_owned(),
            Value::from(request.position_ms.unwrap_or(0)),
        );
        if let Some(context_uri) = request.context_uri.filter(|uri| !uri.is_empty()) {
            body.insert("context_uri".to_owned(), Value::String(context_uri));
        }
        if let Some(uris) = request.uris.filter(|uris| !uris.is_empty()) {
            body.insert(
                "uris".to_owned(),
                serde_json::to_value(uris).map_err(|_| SpotifyError::InvalidRequest)?,
            );
        }
        if let Some(position) = request.position_offset.filter(|position| *position > 0) {
            body.insert(
                "offset".to_owned(),
                serde_json::json!({"position": position}),
            );
        }
        if let Some(uri) = request.uri_offset.filter(|uri| !uri.is_empty()) {
            body.insert("offset".to_owned(), serde_json::json!({"uri": uri}));
        }
        let query = optional_device_query(request.device_id.as_deref());
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "play"])?,
            &query,
            Some(&Value::Object(body)),
        )
        .await
    }

    /// Resume playback at the beginning of the current item, matching the
    /// pinned integration's play/resume call.
    pub async fn resume_playback(&self, device_id: Option<&str>) -> Result<(), SpotifyError> {
        self.start_playback(StartPlaybackRequest {
            device_id: device_id.map(str::to_owned),
            position_ms: Some(0),
            ..StartPlaybackRequest::default()
        })
        .await
    }

    /// Play a media context URI or a list of track/episode URIs.
    pub async fn play_media(
        &self,
        target: PlayMediaTarget,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut request = StartPlaybackRequest {
            device_id: device_id.map(str::to_owned),
            ..StartPlaybackRequest::default()
        };
        match target {
            PlayMediaTarget::Context(uri) => request.context_uri = Some(uri),
            PlayMediaTarget::Uris(uris) => request.uris = Some(uris),
        }
        self.start_playback(request).await
    }

    /// Pause playback.
    pub async fn pause_playback(&self, device_id: Option<&str>) -> Result<(), SpotifyError> {
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "pause"])?,
            &optional_device_query(device_id),
            None,
        )
        .await
    }

    /// Skip to the next track.
    pub async fn next_track(&self, device_id: Option<&str>) -> Result<(), SpotifyError> {
        self.write(
            Method::POST,
            self.endpoint(&["me", "player", "next"])?,
            &optional_device_query(device_id),
            None,
        )
        .await
    }

    /// Skip to the previous track.
    pub async fn previous_track(&self, device_id: Option<&str>) -> Result<(), SpotifyError> {
        self.write(
            Method::POST,
            self.endpoint(&["me", "player", "previous"])?,
            &optional_device_query(device_id),
            None,
        )
        .await
    }

    /// Seek to a position in milliseconds.
    pub async fn seek_track(
        &self,
        position_ms: u64,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut query = vec![("position_ms".to_owned(), position_ms.to_string())];
        append_device_query(&mut query, device_id);
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "seek"])?,
            &query,
            None,
        )
        .await
    }

    /// Set repeat mode.
    pub async fn set_repeat(
        &self,
        state: RepeatMode,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut query = vec![("state".to_owned(), state.as_str().to_owned())];
        append_device_query(&mut query, device_id);
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "repeat"])?,
            &query,
            None,
        )
        .await
    }

    /// Set device volume as a percent.
    pub async fn set_volume(
        &self,
        volume_percent: u32,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut query = vec![("volume_percent".to_owned(), volume_percent.to_string())];
        append_device_query(&mut query, device_id);
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "volume"])?,
            &query,
            None,
        )
        .await
    }

    /// Set shuffle state.
    pub async fn set_shuffle(
        &self,
        state: bool,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut query = vec![("state".to_owned(), state.to_string())];
        append_device_query(&mut query, device_id);
        self.write(
            Method::PUT,
            self.endpoint(&["me", "player", "shuffle"])?,
            &query,
            None,
        )
        .await
    }

    /// Add a track or episode URI to the current queue.
    pub async fn add_to_queue(
        &self,
        uri: &str,
        device_id: Option<&str>,
    ) -> Result<(), SpotifyError> {
        let mut query = vec![("uri".to_owned(), uri.to_owned())];
        append_device_query(&mut query, device_id);
        self.write(
            Method::POST,
            self.endpoint(&["me", "player", "queue"])?,
            &query,
            None,
        )
        .await
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, SpotifyError> {
        let mut url = self.base_url.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| SpotifyError::InvalidBaseUrl)?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        url: Url,
        query: &[(&str, &str)],
        authorizer: &dyn RequestAuthorizer,
    ) -> Result<T, SpotifyError> {
        let response = self.send(Method::GET, url, query, None, authorizer).await?;
        self.decode_json(response).await
    }

    async fn decode_json<T: DeserializeOwned>(
        &self,
        response: Response,
    ) -> Result<T, SpotifyError> {
        let status = response.status();
        let retry_after = retry_after(&response);
        if status == StatusCode::NO_CONTENT {
            return Err(SpotifyError::InvalidPayload);
        }
        if is_redirect(status) {
            return Err(SpotifyError::RedirectBlocked);
        }
        let (body, truncated) = self.read_bounded_body(response).await?;
        if !status.is_success() {
            return Err(map_http_error(status, retry_after, &body, truncated));
        }
        if truncated {
            return Err(SpotifyError::ResponseTooLarge);
        }
        serde_json::from_slice(&body).map_err(|_| SpotifyError::InvalidPayload)
    }

    async fn write(
        &self,
        method: Method,
        url: Url,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<(), SpotifyError> {
        let response = self
            .send_owned_query(method, url, query, body, self.authorizer.as_ref())
            .await?;
        let status = response.status();
        let retry_after = retry_after(&response);
        if is_redirect(status) {
            return Err(SpotifyError::RedirectBlocked);
        }
        if status.is_success() {
            return Ok(());
        }
        let (body, truncated) = self.read_bounded_body(response).await?;
        Err(map_http_error(status, retry_after, &body, truncated))
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        query: &[(&str, &str)],
        json_body: Option<&Value>,
        authorizer: &dyn RequestAuthorizer,
    ) -> Result<Response, SpotifyError> {
        let mut builder = self.http.request(method, url);
        if !query.is_empty() {
            builder = builder.query(query);
        }
        if let Some(body) = json_body {
            builder = self.with_json_body(builder, body)?;
        }
        let request = self.authorize_request(authorizer, builder).await?;
        self.http
            .execute(request)
            .await
            .map_err(map_transport_error)
    }

    async fn send_owned_query(
        &self,
        method: Method,
        url: Url,
        query: &[(String, String)],
        json_body: Option<&Value>,
        authorizer: &dyn RequestAuthorizer,
    ) -> Result<Response, SpotifyError> {
        let mut builder = self.http.request(method, url);
        if !query.is_empty() {
            builder = builder.query(query);
        }
        if let Some(body) = json_body {
            builder = self.with_json_body(builder, body)?;
        }
        let request = self.authorize_request(authorizer, builder).await?;
        self.http
            .execute(request)
            .await
            .map_err(map_transport_error)
    }

    async fn authorize_request(
        &self,
        authorizer: &dyn RequestAuthorizer,
        builder: RequestBuilder,
    ) -> Result<reqwest::Request, SpotifyError> {
        let mut request = authorizer.authorize(builder).await?;
        let authorization = request
            .headers_mut()
            .get_mut(AUTHORIZATION)
            .ok_or(AuthorizationError::InvalidRequest)?;
        authorization.set_sensitive(true);
        Ok(request)
    }

    fn with_json_body(
        &self,
        builder: RequestBuilder,
        body: &Value,
    ) -> Result<RequestBuilder, SpotifyError> {
        let body = serde_json::to_vec(body).map_err(|_| SpotifyError::InvalidRequest)?;
        if body.len() > self.options.max_request_body_bytes {
            return Err(SpotifyError::RequestTooLarge);
        }
        Ok(builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body))
    }

    async fn read_bounded_body(
        &self,
        mut response: Response,
    ) -> Result<(Vec<u8>, bool), SpotifyError> {
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok())
            .and_then(|length| length.parse::<usize>().ok())
            .is_some_and(|length| length > self.options.max_response_bytes)
        {
            return Ok((Vec::new(), true));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
            let remaining = self.options.max_response_bytes.saturating_sub(body.len());
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                return Ok((body, true));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, false))
    }

    async fn get_pages<T: DeserializeOwned>(
        &self,
        path: &[&str],
        query: &[(&str, &str)],
        item_parent: &[&str],
    ) -> Result<Vec<T>, SpotifyError> {
        let initial_url = self.endpoint(path)?;
        let mut next_url = Some(initial_url.clone());
        let mut next_query = query
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        let mut items = Vec::new();
        let mut advertised_total = None;
        let mut page_count = 0usize;

        while let Some(url) = next_url.take() {
            let request_url = with_query(&url, &next_query);
            if !visited.insert(request_url.as_str().to_owned()) {
                return Err(SpotifyError::PaginationLoop);
            }
            page_count = page_count.saturating_add(1);
            if page_count > self.options.max_pages {
                return Err(SpotifyError::PaginationLimitExceeded);
            }
            let response = self
                .send_owned_query(
                    Method::GET,
                    url.clone(),
                    &next_query,
                    None,
                    self.authorizer.as_ref(),
                )
                .await?;
            next_query.clear();
            let body: Value = self.decode_json(response).await?;
            let page = page_from_value::<T>(body, item_parent, self.options.max_page_items)?;
            if let Some(total) = page.total {
                advertised_total = Some(total);
            }
            if items.len().saturating_add(page.items.len())
                > self
                    .options
                    .max_pages
                    .saturating_mul(self.options.max_page_items)
            {
                return Err(SpotifyError::PaginationLimitExceeded);
            }
            items.extend(page.items);

            match page.next {
                Some(next) => {
                    let validated = self.validate_next_url(&url, &next)?;
                    if visited.contains(validated.as_str()) {
                        return Err(SpotifyError::PaginationLoop);
                    }
                    next_url = Some(validated);
                }
                None => {
                    if advertised_total.is_some_and(|total| total > items.len() as u64) {
                        return Err(SpotifyError::PaginationIncomplete);
                    }
                }
            }
        }
        Ok(items)
    }

    fn validate_next_url(&self, current: &Url, next: &str) -> Result<Url, SpotifyError> {
        let candidate = current
            .join(next)
            .map_err(|_| SpotifyError::UnsafePaginationUrl)?;
        if candidate.origin() != self.base_url.origin()
            || has_userinfo(&candidate)
            || !candidate.username().is_empty()
            || candidate.password().is_some()
            || candidate.fragment().is_some()
            || !candidate.path().starts_with(self.base_url.path())
        {
            return Err(SpotifyError::UnsafePaginationUrl);
        }
        Ok(candidate)
    }
}

#[derive(Debug)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
    total: Option<u64>,
}

fn page_from_value<T: DeserializeOwned>(
    mut value: Value,
    item_parent: &[&str],
    max_page_items: usize,
) -> Result<Page<T>, SpotifyError> {
    let mut parent = &mut value;
    for key in item_parent {
        parent = parent.get_mut(*key).ok_or(SpotifyError::InvalidPayload)?;
    }
    let object = parent.as_object().ok_or(SpotifyError::InvalidPayload)?;
    let values = object
        .get("items")
        .and_then(Value::as_array)
        .ok_or(SpotifyError::InvalidPayload)?;
    if values.len() > max_page_items {
        return Err(SpotifyError::PaginationLimitExceeded);
    }
    let mut items = Vec::with_capacity(values.len());
    for value in values {
        items
            .push(serde_json::from_value(value.clone()).map_err(|_| SpotifyError::InvalidPayload)?);
    }
    let next = match object.get("next").ok_or(SpotifyError::InvalidPayload)? {
        Value::Null => None,
        Value::String(next) if !next.is_empty() => Some(next.clone()),
        _ => return Err(SpotifyError::InvalidPayload),
    };
    let total = match object.get("total") {
        None | Some(Value::Null) => None,
        Some(Value::Number(total)) => Some(total.as_u64().ok_or(SpotifyError::InvalidPayload)?),
        Some(_) => return Err(SpotifyError::InvalidPayload),
    };
    Ok(Page { items, next, total })
}

fn validate_base_url(base_url: &mut Url) -> Result<(), SpotifyError> {
    if has_userinfo(base_url)
        || !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
        || base_url.host_str().is_none()
    {
        return Err(SpotifyError::InvalidBaseUrl);
    }
    match base_url.scheme() {
        "https" => {}
        "http" => {
            let host = base_url.host_str().ok_or(SpotifyError::InvalidBaseUrl)?;
            let ip = host
                .parse::<IpAddr>()
                .map_err(|_| SpotifyError::InvalidBaseUrl)?;
            if !ip.is_loopback() {
                return Err(SpotifyError::InvalidBaseUrl);
            }
        }
        _ => return Err(SpotifyError::InvalidBaseUrl),
    }
    if !base_url.path().ends_with('/') {
        let path = format!("{}/", base_url.path());
        base_url.set_path(&path);
    }
    Ok(())
}

fn has_userinfo(url: &Url) -> bool {
    url.as_str()
        .split_once("://")
        .and_then(|(_, authority_and_path)| authority_and_path.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

fn spotify_identifier(input: &str) -> Result<String, SpotifyError> {
    let trimmed = input.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("spotify:") {
        rest.rsplit(':').next().unwrap_or_default()
    } else {
        trimmed
    };
    if candidate.is_empty()
        || candidate.len() > 256
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(SpotifyError::InvalidRequest);
    }
    Ok(candidate.to_owned())
}

fn optional_device_query(device_id: Option<&str>) -> Vec<(String, String)> {
    let mut query = Vec::new();
    append_device_query(&mut query, device_id);
    query
}

fn append_device_query(query: &mut Vec<(String, String)>, device_id: Option<&str>) {
    if let Some(device_id) = device_id.filter(|value| !value.is_empty()) {
        query.push(("device_id".to_owned(), device_id.to_owned()));
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn map_http_error(
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &[u8],
    truncated: bool,
) -> SpotifyError {
    if status == StatusCode::UNAUTHORIZED {
        return SpotifyError::Unauthorized;
    }
    if status == StatusCode::FORBIDDEN {
        return SpotifyError::Forbidden {
            reason: forbidden_reason(body),
        };
    }
    if status == StatusCode::NOT_FOUND || body_status_is(body, 404, truncated) {
        return SpotifyError::NotFound;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return SpotifyError::RateLimited {
            retry_after,
            reason: rate_limit_reason(body),
        };
    }
    if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
        return SpotifyError::Transient {
            status: status.as_u16(),
        };
    }
    if matches!(status.as_u16(), 400 | 422) {
        return SpotifyError::InvalidRequest;
    }
    if is_redirect(status) {
        return SpotifyError::RedirectBlocked;
    }
    SpotifyError::UnexpectedStatus {
        status: status.as_u16(),
    }
}

fn body_status_is(body: &[u8], wanted: u16, truncated: bool) -> bool {
    if truncated {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_u64)
                .or_else(|| value.pointer("/error/status").and_then(Value::as_u64))
        })
        .is_some_and(|status| status == u64::from(wanted))
}

fn rate_limit_reason(body: &[u8]) -> RateLimitReason {
    if body_has_reason(body, "QUOTA_EXCEEDED") {
        RateLimitReason::QuotaExceeded
    } else {
        RateLimitReason::Unknown
    }
}

fn forbidden_reason(body: &[u8]) -> ForbiddenReason {
    let Some(value) = serde_json::from_slice::<Value>(body).ok() else {
        return ForbiddenReason::Unknown;
    };
    let text = safe_text(&value);
    if text.contains("premium") {
        ForbiddenReason::PremiumRequired
    } else if text.contains("scope") || text.contains("permission") {
        ForbiddenReason::MissingScope
    } else if text.contains("app") || text.contains("user") || text.contains("development") {
        ForbiddenReason::AppAccessRestricted
    } else {
        ForbiddenReason::Unknown
    }
}

fn body_has_reason(body: &[u8], wanted: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| value_contains_string(&value, wanted))
}

fn value_contains_string(value: &Value, wanted: &str) -> bool {
    match value {
        Value::String(value) => value.eq_ignore_ascii_case(wanted),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_string(value, wanted)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_string(value, wanted)),
        _ => false,
    }
}

fn safe_text(value: &Value) -> String {
    fn collect(value: &Value, output: &mut String) {
        match value {
            Value::String(text) => {
                output.push(' ');
                output.push_str(&text.to_ascii_lowercase());
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, output)),
            Value::Object(values) => values.values().for_each(|value| collect(value, output)),
            _ => {}
        }
    }
    let mut output = String::new();
    collect(value, &mut output);
    output
}

fn is_redirect(status: StatusCode) -> bool {
    (300..400).contains(&status.as_u16())
}

fn map_transport_error(error: reqwest::Error) -> SpotifyError {
    if error.is_timeout() {
        SpotifyError::Timeout
    } else {
        SpotifyError::Transport
    }
}

fn with_query(url: &Url, query: &[(String, String)]) -> Url {
    let mut output = url.clone();
    if !query.is_empty() {
        output.query_pairs_mut().extend_pairs(query.iter());
    }
    output
}
