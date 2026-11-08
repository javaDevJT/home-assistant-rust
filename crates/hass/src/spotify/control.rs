//! Spotify-specific media-player service mapping.

use hass_media_player::{
    MediaCommand, MediaCommandKind, MediaEnqueue, ProviderCommandOutcome, RepeatMode,
};
use hass_spotify::{
    Device, PlayMediaTarget, PlaybackState, RepeatMode as SpotifyRepeatMode, SpotifyClient,
    SpotifyError, StartPlaybackRequest,
};

const SPOTIFY_MEDIA_TYPE_PREFIX: &str = "spotify://";
const CURRENT_USER_SAVED_TRACKS: &str = "current_user_saved_tracks";
const U64_EXCLUSIVE_UPPER_BOUND: f64 = 18_446_744_073_709_551_616.0;

fn milliseconds_from_seconds(seconds: f64) -> Result<u64, SpotifyError> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(SpotifyError::InvalidRequest);
    }

    let milliseconds = seconds * 1_000.0;
    if !milliseconds.is_finite() || milliseconds >= U64_EXCLUSIVE_UPPER_BOUND {
        return Err(SpotifyError::InvalidRequest);
    }

    // The pinned Python integration uses int(seconds * 1000), which truncates
    // toward zero. The validated nonnegative value therefore truncates down.
    Ok(milliseconds.trunc() as u64)
}

fn volume_percent_from_level(level: f64) -> Result<u32, SpotifyError> {
    if !level.is_finite() || !(0.0..=1.0).contains(&level) {
        return Err(SpotifyError::InvalidRequest);
    }

    // Home Assistant validates the level range before dispatch; retain a
    // defensive check here so direct typed calls cannot hide invalid input in
    // a saturating numeric cast.
    Ok((level * 100.0).trunc() as u32)
}

fn clean_media_id(media_id: &str) -> Result<String, SpotifyError> {
    if media_id.starts_with("spotify://") {
        let url = url::Url::parse(media_id).map_err(|_| SpotifyError::InvalidRequest)?;
        let name = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or_default();
        // Yarl URL.name decodes the last raw path segment, independently of
        // the wrapper's account host. This never changes the API destination.
        return percent_encoding::percent_decode_str(name)
            .decode_utf8()
            .map(|name| name.into_owned())
            .map_err(|_| SpotifyError::InvalidRequest);
    }
    let end = media_id.find(['?', '#']).unwrap_or(media_id.len());
    Ok(media_id[..end].to_owned())
}

fn media_type_without_prefix(media_type: &str) -> &str {
    media_type
        .strip_prefix(SPOTIFY_MEDIA_TYPE_PREFIX)
        .unwrap_or(media_type)
}

fn first_device_id_when_idle<'a>(
    devices: &'a [Device],
    playback: Option<&PlaybackState>,
) -> Option<&'a str> {
    if playback.is_some() {
        return None;
    }
    devices.first().and_then(|device| device.id.as_deref())
}

/// Execute a Spotify media-player command using only the supplied cached
/// snapshots and the typed Spotify client. Feature and availability gates are
/// owned by the media-player dispatcher.
pub(super) async fn execute(
    client: &SpotifyClient,
    command: MediaCommand,
    devices: &[Device],
    playback: Option<&PlaybackState>,
    legacy_user_id: Option<&str>,
) -> Result<ProviderCommandOutcome, SpotifyError> {
    match command.kind {
        MediaCommandKind::MediaPlay => {
            client
                .start_playback(StartPlaybackRequest::default())
                .await?;
        }
        MediaCommandKind::MediaPlayPause => {
            if playback.is_some_and(|state| state.is_playing == Some(true)) {
                client.pause_playback(None).await?;
            } else {
                client
                    .start_playback(StartPlaybackRequest::default())
                    .await?;
            }
        }
        MediaCommandKind::MediaPause => client.pause_playback(None).await?,
        MediaCommandKind::MediaNextTrack => client.next_track(None).await?,
        MediaCommandKind::MediaPreviousTrack => client.previous_track(None).await?,
        MediaCommandKind::MediaSeek { seek_position } => {
            client
                .seek_track(milliseconds_from_seconds(seek_position)?, None)
                .await?;
        }
        MediaCommandKind::VolumeSet { volume_level } => {
            client
                .set_volume(volume_percent_from_level(volume_level)?, None)
                .await?;
        }
        MediaCommandKind::ShuffleSet { shuffle } => {
            client.set_shuffle(shuffle, None).await?;
        }
        MediaCommandKind::RepeatSet { repeat } => {
            let repeat = match repeat {
                RepeatMode::Off => SpotifyRepeatMode::Off,
                RepeatMode::All => SpotifyRepeatMode::Context,
                RepeatMode::One => SpotifyRepeatMode::Track,
            };
            client.set_repeat(repeat, None).await?;
        }
        MediaCommandKind::SelectSource { source } => {
            let Some(device_id) = devices
                .iter()
                .find(|device| device.name.as_deref() == Some(source.as_str()))
                .and_then(|device| device.id.as_deref())
            else {
                return Ok(ProviderCommandOutcome::NoOp);
            };
            client.transfer_playback(device_id).await?;
        }
        MediaCommandKind::PlayMedia(play_media) => {
            let media_type = media_type_without_prefix(&play_media.media_content_type);
            let media_id = clean_media_id(&play_media.media_content_id)?;
            let device_id = first_device_id_when_idle(devices, playback);
            let enqueue_add = play_media.enqueue == Some(MediaEnqueue::Add);

            let target = match media_type {
                "track" | "episode" | "music" => PlayMediaTarget::Uris(vec![media_id.clone()]),
                "playlist" | "album" | "artist" | "show" => {
                    if enqueue_add {
                        return Err(SpotifyError::InvalidRequest);
                    }
                    PlayMediaTarget::Context(media_id.clone())
                }
                CURRENT_USER_SAVED_TRACKS => {
                    if enqueue_add {
                        return Err(SpotifyError::InvalidRequest);
                    }
                    let Some(user_id) = legacy_user_id.filter(|user_id| !user_id.is_empty()) else {
                        return Err(SpotifyError::InvalidRequest);
                    };
                    PlayMediaTarget::Context(format!("spotify:user:{user_id}:collection"))
                }
                _ => return Ok(ProviderCommandOutcome::NoOp),
            };

            if enqueue_add {
                let PlayMediaTarget::Uris(uris) = target else {
                    return Err(SpotifyError::InvalidRequest);
                };
                let Some(uri) = uris.first() else {
                    return Err(SpotifyError::InvalidRequest);
                };
                client.add_to_queue(uri, device_id).await?;
            } else {
                client.play_media(target, device_id).await?;
            }
        }
        _ => return Err(SpotifyError::InvalidRequest),
    }

    Ok(ProviderCommandOutcome::Executed)
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

    use hass_media_player::{
        MediaCommand, MediaCommandKind, MediaEnqueue, ProviderCommandOutcome, RepeatMode,
    };
    use hass_spotify::{
        AuthorizationError, Device, PlaybackState, RequestAuthorizer, SpotifyClient, SpotifyError,
    };
    use reqwest::{
        Request, RequestBuilder,
        header::{AUTHORIZATION, HeaderValue},
    };
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
        time::timeout,
    };
    use url::Url;

    use super::{clean_media_id, execute};

    #[test]
    fn browser_wrapper_uses_once_decoded_final_segment() {
        for (input, expected) in [
            (
                "spotify://other-account/path/spotify%3Atrack%3Atrack-2?si=x#y",
                "spotify:track:track-2",
            ),
            (
                "spotify://account/spotify:playlist:list+one",
                "spotify:playlist:list+one",
            ),
            ("spotify://account/%252F", "%2F"),
            ("spotify:track:plain?si=x#y", "spotify:track:plain"),
        ] {
            assert_eq!(clean_media_id(input).unwrap(), expected);
        }
        assert!(matches!(
            clean_media_id("spotify://account/%FF"),
            Err(SpotifyError::InvalidRequest)
        ));
    }

    struct FixtureAuthorizer;

    impl RequestAuthorizer for FixtureAuthorizer {
        fn authorize<'life0, 'async_trait>(
            &'life0 self,
            request: RequestBuilder,
        ) -> Pin<Box<dyn Future<Output = Result<Request, AuthorizationError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                let mut request = request
                    .build()
                    .map_err(|_| AuthorizationError::InvalidRequest)?;
                let mut authorization = HeaderValue::from_static("Bearer fixture-token");
                authorization.set_sensitive(true);
                request.headers_mut().insert(AUTHORIZATION, authorization);
                Ok(request)
            })
        }
    }

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        target: String,
        body: String,
    }

    async fn read_and_respond(stream: &mut TcpStream) -> RecordedRequest {
        let mut headers = Vec::new();
        let mut byte = [0_u8; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            headers.push(byte[0]);
        }

        let headers = String::from_utf8(headers).unwrap();
        let mut lines = headers.split("\r\n");
        let mut request_line = lines.next().unwrap().split_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let target = request_line.next().unwrap().to_owned();
        let content_length = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();

        let mut body = vec![0_u8; content_length];
        if !body.is_empty() {
            stream.read_exact(&mut body).await.unwrap();
        }

        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        RecordedRequest {
            method,
            target,
            body: String::from_utf8(body).unwrap(),
        }
    }

    async fn client_with_fixture(
        request_count: usize,
    ) -> (SpotifyClient, JoinHandle<Vec<RecordedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_and_respond(&mut stream).await);
            }
            requests
        });
        let base_url = Url::parse(&format!("http://{address}/v1/")).unwrap();
        let client = SpotifyClient::with_base_url(base_url, Arc::new(FixtureAuthorizer)).unwrap();
        (client, server)
    }

    fn command(kind: MediaCommandKind) -> MediaCommand {
        MediaCommand {
            kind,
            extra: Default::default(),
        }
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: Some(id.to_owned()),
            name: Some(name.to_owned()),
            ..Device::default()
        }
    }

    fn assert_request(
        request: &RecordedRequest,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) {
        assert_eq!(request.method, method);
        let parsed = Url::parse(&format!("http://127.0.0.1{}", request.target)).unwrap();
        assert_eq!(parsed.path(), path);
        let actual_query = parsed
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let expected_query = query
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(actual_query, expected_query);
        match body {
            Some(body) => assert_eq!(serde_json::from_str::<Value>(&request.body).unwrap(), body),
            None => assert!(request.body.is_empty()),
        }
    }

    #[tokio::test]
    async fn maps_advertised_commands_and_all_play_media_targets_to_exact_requests() {
        let (client, server) = client_with_fixture(22).await;
        let devices = [device("device-1", "Living room")];
        let playing = PlaybackState {
            is_playing: Some(true),
            ..PlaybackState::default()
        };
        let commands = [
            (command(MediaCommandKind::MediaPlay), None),
            (command(MediaCommandKind::MediaPlayPause), Some(&playing)),
            (command(MediaCommandKind::MediaPlayPause), None),
            (command(MediaCommandKind::MediaPause), None),
            (command(MediaCommandKind::MediaNextTrack), None),
            (command(MediaCommandKind::MediaPreviousTrack), None),
            (
                command(MediaCommandKind::MediaSeek {
                    seek_position: 12.3459,
                }),
                None,
            ),
            (
                command(MediaCommandKind::VolumeSet {
                    volume_level: 0.3599,
                }),
                None,
            ),
            (
                command(MediaCommandKind::ShuffleSet { shuffle: true }),
                None,
            ),
            (
                command(MediaCommandKind::RepeatSet {
                    repeat: RepeatMode::Off,
                }),
                None,
            ),
            (
                command(MediaCommandKind::RepeatSet {
                    repeat: RepeatMode::All,
                }),
                None,
            ),
            (
                command(MediaCommandKind::RepeatSet {
                    repeat: RepeatMode::One,
                }),
                None,
            ),
            (
                command(MediaCommandKind::SelectSource {
                    source: "Living room".to_owned(),
                }),
                None,
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "spotify://music".to_owned(),
                        media_content_id: "https://open.spotify.com/track/track-1?si=share#listen"
                            .to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                None,
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "track".to_owned(),
                        media_content_id:
                            "spotify://other-account/spotify%3Atrack%3Atrack-2?si=share#listen"
                                .to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "episode".to_owned(),
                        media_content_id: "spotify:episode:episode-1".to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "playlist".to_owned(),
                        media_content_id: "spotify:playlist:playlist-1".to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "album".to_owned(),
                        media_content_id: "spotify:album:album-1".to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "artist".to_owned(),
                        media_content_id: "spotify:artist:artist-1".to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "show".to_owned(),
                        media_content_id: "spotify:show:show-1".to_owned(),
                        enqueue: None,
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                Some(&playing),
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "spotify://current_user_saved_tracks".to_owned(),
                        media_content_id: "ignored".to_owned(),
                        enqueue: Some(MediaEnqueue::Replace),
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                None,
            ),
            (
                command(MediaCommandKind::PlayMedia(
                    hass_media_player::PlayMediaCommand {
                        media_content_type: "spotify://track".to_owned(),
                        media_content_id: "spotify:track:queued?si=share#listen".to_owned(),
                        enqueue: Some(MediaEnqueue::Add),
                        announce: None,
                        extra: Default::default(),
                    },
                )),
                None,
            ),
        ];

        for (command, playback) in commands {
            assert_eq!(
                execute(&client, command, &devices, playback, Some("legacy-user"),)
                    .await
                    .unwrap(),
                ProviderCommandOutcome::Executed
            );
        }

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 22);
        let play_body = Some(json!({ "position_ms": 0 }));
        assert_request(
            &requests[0],
            "PUT",
            "/v1/me/player/play",
            &[],
            play_body.clone(),
        );
        assert_request(&requests[1], "PUT", "/v1/me/player/pause", &[], None);
        assert_request(
            &requests[2],
            "PUT",
            "/v1/me/player/play",
            &[],
            play_body.clone(),
        );
        assert_request(&requests[3], "PUT", "/v1/me/player/pause", &[], None);
        assert_request(&requests[4], "POST", "/v1/me/player/next", &[], None);
        assert_request(&requests[5], "POST", "/v1/me/player/previous", &[], None);
        assert_request(
            &requests[6],
            "PUT",
            "/v1/me/player/seek",
            &[("position_ms", "12345")],
            None,
        );
        assert_request(
            &requests[7],
            "PUT",
            "/v1/me/player/volume",
            &[("volume_percent", "35")],
            None,
        );
        assert_request(
            &requests[8],
            "PUT",
            "/v1/me/player/shuffle",
            &[("state", "true")],
            None,
        );
        assert_request(
            &requests[9],
            "PUT",
            "/v1/me/player/repeat",
            &[("state", "off")],
            None,
        );
        assert_request(
            &requests[10],
            "PUT",
            "/v1/me/player/repeat",
            &[("state", "context")],
            None,
        );
        assert_request(
            &requests[11],
            "PUT",
            "/v1/me/player/repeat",
            &[("state", "track")],
            None,
        );
        assert_request(
            &requests[12],
            "PUT",
            "/v1/me/player",
            &[],
            Some(json!({ "device_ids": ["device-1"] })),
        );
        assert_request(
            &requests[13],
            "PUT",
            "/v1/me/player/play",
            &[("device_id", "device-1")],
            Some(json!({
                "position_ms": 0,
                "uris": ["https://open.spotify.com/track/track-1"]
            })),
        );
        assert_request(
            &requests[14],
            "PUT",
            "/v1/me/player/play",
            &[],
            Some(json!({ "position_ms": 0, "uris": ["spotify:track:track-2"] })),
        );
        assert_request(
            &requests[15],
            "PUT",
            "/v1/me/player/play",
            &[],
            Some(json!({ "position_ms": 0, "uris": ["spotify:episode:episode-1"] })),
        );
        for (index, uri) in [
            "spotify:playlist:playlist-1",
            "spotify:album:album-1",
            "spotify:artist:artist-1",
            "spotify:show:show-1",
        ]
        .into_iter()
        .enumerate()
        {
            assert_request(
                &requests[16 + index],
                "PUT",
                "/v1/me/player/play",
                &[],
                Some(json!({ "position_ms": 0, "context_uri": uri })),
            );
        }
        assert_request(
            &requests[20],
            "PUT",
            "/v1/me/player/play",
            &[("device_id", "device-1")],
            Some(json!({
                "position_ms": 0,
                "context_uri": "spotify:user:legacy-user:collection"
            })),
        );
        assert_request(
            &requests[21],
            "POST",
            "/v1/me/player/queue",
            &[("uri", "spotify:track:queued"), ("device_id", "device-1")],
            None,
        );
    }

    async fn no_request_client() -> (SpotifyClient, JoinHandle<Option<RecordedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            match timeout(Duration::from_millis(100), listener.accept()).await {
                Ok(Ok((mut stream, _))) => Some(read_and_respond(&mut stream).await),
                _ => None,
            }
        });
        let base_url = Url::parse(&format!("http://{address}/v1/")).unwrap();
        let client = SpotifyClient::with_base_url(base_url, Arc::new(FixtureAuthorizer)).unwrap();
        (client, server)
    }

    fn play_media(media_content_type: &str, enqueue: Option<MediaEnqueue>) -> MediaCommand {
        command(MediaCommandKind::PlayMedia(
            hass_media_player::PlayMediaCommand {
                media_content_type: media_content_type.to_owned(),
                media_content_id: "spotify:track:fixture".to_owned(),
                enqueue,
                announce: None,
                extra: Default::default(),
            },
        ))
    }

    #[tokio::test]
    async fn unsupported_source_and_media_commands_are_noops_or_typed_errors_without_http() {
        let (client, server) = no_request_client().await;
        let devices = [device("device-1", "Living room")];

        assert_eq!(
            execute(
                &client,
                command(MediaCommandKind::SelectSource {
                    source: "Missing room".to_owned(),
                }),
                &devices,
                None,
                None,
            )
            .await
            .unwrap(),
            ProviderCommandOutcome::NoOp
        );
        assert_eq!(
            execute(
                &client,
                play_media("unsupported", None),
                &devices,
                None,
                None,
            )
            .await
            .unwrap(),
            ProviderCommandOutcome::NoOp
        );
        assert_eq!(
            execute(
                &client,
                play_media("playlist", Some(MediaEnqueue::Add)),
                &devices,
                None,
                None,
            )
            .await,
            Err(SpotifyError::InvalidRequest)
        );
        assert_eq!(
            execute(
                &client,
                play_media("current_user_saved_tracks", None),
                &devices,
                None,
                None,
            )
            .await,
            Err(SpotifyError::InvalidRequest)
        );
        assert_eq!(
            execute(
                &client,
                command(MediaCommandKind::MediaStop),
                &devices,
                None,
                None,
            )
            .await,
            Err(SpotifyError::InvalidRequest)
        );
        assert_eq!(
            execute(
                &client,
                command(MediaCommandKind::MediaSeek {
                    seek_position: f64::MAX,
                }),
                &devices,
                None,
                None,
            )
            .await,
            Err(SpotifyError::InvalidRequest)
        );
        assert_eq!(
            execute(
                &client,
                command(MediaCommandKind::VolumeSet { volume_level: 1.01 }),
                &devices,
                None,
                None,
            )
            .await,
            Err(SpotifyError::InvalidRequest)
        );

        assert!(server.await.unwrap().is_none());
    }
}
