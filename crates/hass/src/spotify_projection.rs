//! Pure projection from Spotify coordinator data to the shared media-player
//! snapshot.

use chrono::{DateTime, Utc};
use hass_core::Context;
use hass_media_player::{MediaPlayerFeatures, MediaPlayerSnapshot, MediaPlayerState, RepeatMode};
use hass_spotify::{PlaybackState, SpotifyObject, UserProfile};

/// Features advertised by Home Assistant's pinned Spotify integration for a
/// premium account with a known, unrestricted active playback device.
fn full_spotify_features() -> MediaPlayerFeatures {
    MediaPlayerFeatures::BROWSE_MEDIA
        | MediaPlayerFeatures::NEXT_TRACK
        | MediaPlayerFeatures::PAUSE
        | MediaPlayerFeatures::PLAY
        | MediaPlayerFeatures::PLAY_MEDIA
        | MediaPlayerFeatures::PREVIOUS_TRACK
        | MediaPlayerFeatures::REPEAT_SET
        | MediaPlayerFeatures::SEEK
        | MediaPlayerFeatures::SELECT_SOURCE
        | MediaPlayerFeatures::SHUFFLE_SET
        | MediaPlayerFeatures::VOLUME_SET
}

fn supported_features(
    profile: &UserProfile,
    playback: Option<&PlaybackState>,
) -> MediaPlayerFeatures {
    // The profile product is deprecated and optional. Unknown entitlement is
    // deliberately conservative, but it is not evidence of a free account.
    if profile.product.as_deref() != Some("premium") {
        return MediaPlayerFeatures::NONE;
    }

    let has_known_unrestricted_device = playback
        .and_then(|state| state.device.as_ref())
        .is_some_and(|device| device.is_restricted == Some(false));

    if has_known_unrestricted_device {
        full_spotify_features()
    } else {
        MediaPlayerFeatures::SELECT_SOURCE
    }
}

fn rounded_seconds(milliseconds: u64) -> f64 {
    // Python's round() used by the pinned integration rounds half seconds to
    // the nearest even integer. Keep that behavior without floating-point
    // precision loss for the millisecond input.
    let whole = milliseconds / 1_000;
    let remainder = milliseconds % 1_000;
    let rounded = match remainder.cmp(&500) {
        std::cmp::Ordering::Less => whole,
        std::cmp::Ordering::Greater => whole + 1,
        std::cmp::Ordering::Equal if whole.is_multiple_of(2) => whole,
        std::cmp::Ordering::Equal => whole + 1,
    };
    rounded as f64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Track,
    Episode,
    Unknown,
}

fn item_kind(item: &SpotifyObject, playback: &PlaybackState) -> ItemKind {
    let kind = item
        .kind
        .as_deref()
        .or(playback.currently_playing_type.as_deref());
    match kind {
        Some("track") => ItemKind::Track,
        Some("episode") => ItemKind::Episode,
        _ => ItemKind::Unknown,
    }
}

fn first_image_url(images: Option<&[hass_spotify::Image]>) -> Option<String> {
    images
        .and_then(|images| images.first())
        .and_then(|image| image.url.clone())
}

fn item_image(item: &SpotifyObject, kind: ItemKind) -> Option<String> {
    match kind {
        ItemKind::Episode => {
            if let Some(images) = item.images.as_deref().filter(|images| !images.is_empty()) {
                // Match the pinned integration: if the episode has an image
                // entry, its first URL is authoritative, even if that URL is
                // absent in a partial response.
                images[0].url.clone()
            } else {
                item.show
                    .as_deref()
                    .and_then(|show| first_image_url(show.images.as_deref()))
            }
        }
        ItemKind::Track => item
            .album
            .as_deref()
            .and_then(|album| first_image_url(album.images.as_deref())),
        ItemKind::Unknown => None,
    }
}

fn item_artist(item: &SpotifyObject, kind: ItemKind) -> Option<String> {
    match kind {
        ItemKind::Episode => item.show.as_deref().and_then(|show| show.name.clone()),
        ItemKind::Track => item.artists.as_ref().map(|artists| {
            artists
                .iter()
                .filter_map(|artist| artist.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ")
        }),
        ItemKind::Unknown => None,
    }
}

/// Project successfully fetched Spotify coordinator data into a native media
/// player snapshot. `playback == None` represents a successful no-active-
/// playback response; request failures must be handled by the provider before
/// calling this function.
pub(crate) fn snapshot(
    profile: &UserProfile,
    playback: Option<&PlaybackState>,
    devices: &[hass_spotify::Device],
    playlist_name: Option<&str>,
    fetched_at: DateTime<Utc>,
    context: Context,
) -> MediaPlayerSnapshot {
    let mut source_list = devices
        .iter()
        .filter_map(|device| device.name.clone())
        .collect::<Vec<_>>();
    source_list.sort();

    let active_device = playback.and_then(|state| state.device.as_ref());
    let item = playback.and_then(|state| state.item.as_ref());
    let kind = playback
        .zip(item)
        .map(|(state, item)| item_kind(item, state));

    MediaPlayerSnapshot {
        state: Some(match playback {
            None => MediaPlayerState::Idle,
            Some(state) if state.is_playing == Some(true) => MediaPlayerState::Playing,
            Some(_) => MediaPlayerState::Paused,
        }),
        available: true,
        supported_features: supported_features(profile, playback),
        context,
        entity_picture_local: None,
        group_members: None,
        volume_level: active_device
            .and_then(|device| device.volume_percent)
            .map(|volume| f64::from(volume) / 100.0),
        is_volume_muted: None,
        media_content_id: item.and_then(|item| item.uri.clone()),
        media_content_type: kind.and_then(|kind| match kind {
            ItemKind::Episode => Some("podcast".to_owned()),
            ItemKind::Track => Some("music".to_owned()),
            ItemKind::Unknown => None,
        }),
        media_duration: item.and_then(|item| item.duration_ms).map(rounded_seconds),
        media_position: playback
            .and_then(|state| state.progress_ms)
            .map(rounded_seconds),
        // Spotify's playback timestamp is unreliable. The source integration
        // marks the position with its coordinator fetch time whenever a
        // playback response exists, even if progress itself is absent.
        media_position_updated_at: playback.map(|_| fetched_at),
        media_title: item.and_then(|item| item.name.clone()),
        media_artist: playback
            .zip(item)
            .and_then(|(state, item)| item_artist(item, item_kind(item, state))),
        media_album_name: playback.zip(item).and_then(|(state, item)| {
            (item_kind(item, state) == ItemKind::Track)
                .then(|| item.album.as_deref().and_then(|album| album.name.clone()))
                .flatten()
        }),
        media_album_artist: None,
        media_track: playback.zip(item).and_then(|(state, item)| {
            (item_kind(item, state) == ItemKind::Track)
                .then(|| item.track_number.map(i64::from))
                .flatten()
        }),
        media_series_title: None,
        media_season: None,
        media_episode: None,
        media_channel: None,
        media_playlist: playlist_name.map(str::to_owned),
        app_id: None,
        app_name: None,
        source: active_device.and_then(|device| device.name.clone()),
        source_list: Some(source_list),
        sound_mode: None,
        sound_mode_list: None,
        shuffle: playback.and_then(|state| state.shuffle_state),
        repeat: playback.and_then(|state| match state.repeat_state.as_deref() {
            Some("context") => Some(RepeatMode::All),
            Some("off") => Some(RepeatMode::Off),
            Some("track") => Some(RepeatMode::One),
            _ => None,
        }),
        entity_picture: playback
            .zip(item)
            .and_then(|(state, item)| item_image(item, item_kind(item, state))),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use hass_core::Context;
    use hass_media_player::{MediaPlayerFeatures, MediaPlayerState, RepeatMode};
    use hass_spotify::{Artist, Device, Image, PlaybackState, SpotifyObject, UserProfile};

    use super::snapshot;

    fn profile(product: Option<&str>) -> UserProfile {
        UserProfile {
            product: product.map(str::to_owned),
            ..UserProfile::default()
        }
    }

    fn device(name: Option<&str>, restriction: Option<bool>, volume: Option<u32>) -> Device {
        Device {
            name: name.map(str::to_owned),
            is_restricted: restriction,
            volume_percent: volume,
            ..Device::default()
        }
    }

    fn playback(item: Option<SpotifyObject>, device: Option<Device>) -> PlaybackState {
        PlaybackState {
            item,
            device,
            ..PlaybackState::default()
        }
    }

    fn image(url: Option<&str>) -> Image {
        Image {
            url: url.map(str::to_owned),
            ..Image::default()
        }
    }

    fn track_item() -> SpotifyObject {
        SpotifyObject {
            kind: Some("track".into()),
            uri: Some("spotify:track:track-id".into()),
            name: Some("The Track".into()),
            duration_ms: Some(2_500),
            artists: Some(vec![
                Artist {
                    name: Some("First Artist".into()),
                    ..Artist::default()
                },
                Artist {
                    name: Some("Second Artist".into()),
                    ..Artist::default()
                },
            ]),
            album: Some(Box::new(SpotifyObject {
                name: Some("The Album".into()),
                images: Some(vec![image(Some("https://image/album.jpg"))]),
                ..SpotifyObject::default()
            })),
            track_number: Some(8),
            ..SpotifyObject::default()
        }
    }

    fn at_fetch_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 12, 30, 0).unwrap()
    }

    #[test]
    fn track_projection_maps_metadata_volume_repeat_sorting_and_round_ties() {
        let playback = PlaybackState {
            item: Some(track_item()),
            device: Some(device(Some("Kitchen"), Some(false), Some(0))),
            is_playing: Some(true),
            shuffle_state: Some(false),
            repeat_state: Some("context".into()),
            progress_ms: Some(1_500),
            timestamp: Some(0),
            ..PlaybackState::default()
        };
        let devices = [
            device(Some("Zebra"), Some(false), None),
            device(None, Some(false), None),
            device(Some("Alpha"), Some(false), None),
        ];
        let fetched_at = at_fetch_time();

        let projected = snapshot(
            &profile(Some("premium")),
            Some(&playback),
            &devices,
            Some("DJ"),
            fetched_at,
            Context::default(),
        );

        assert!(projected.available);
        assert_eq!(projected.state, Some(MediaPlayerState::Playing));
        assert_eq!(
            projected.media_content_id.as_deref(),
            Some("spotify:track:track-id")
        );
        assert_eq!(projected.media_content_type.as_deref(), Some("music"));
        assert_eq!(projected.media_title.as_deref(), Some("The Track"));
        assert_eq!(
            projected.media_artist.as_deref(),
            Some("First Artist, Second Artist")
        );
        assert_eq!(projected.media_album_name.as_deref(), Some("The Album"));
        assert_eq!(projected.media_track, Some(8));
        // 2.5 seconds rounds to the even integer 2; 1.5 seconds does too.
        assert_eq!(projected.media_duration, Some(2.0));
        assert_eq!(projected.media_position, Some(2.0));
        assert_eq!(projected.media_position_updated_at, Some(fetched_at));
        assert_eq!(
            projected.entity_picture.as_deref(),
            Some("https://image/album.jpg")
        );
        assert_eq!(projected.media_playlist.as_deref(), Some("DJ"));
        assert_eq!(projected.source.as_deref(), Some("Kitchen"));
        assert_eq!(projected.volume_level, Some(0.0));
        assert_eq!(projected.shuffle, Some(false));
        assert_eq!(projected.repeat, Some(RepeatMode::All));
        assert_eq!(
            projected.source_list,
            Some(vec!["Alpha".into(), "Zebra".into()])
        );
        assert_eq!(
            projected.supported_features,
            MediaPlayerFeatures::BROWSE_MEDIA
                | MediaPlayerFeatures::NEXT_TRACK
                | MediaPlayerFeatures::PAUSE
                | MediaPlayerFeatures::PLAY
                | MediaPlayerFeatures::PLAY_MEDIA
                | MediaPlayerFeatures::PREVIOUS_TRACK
                | MediaPlayerFeatures::REPEAT_SET
                | MediaPlayerFeatures::SEEK
                | MediaPlayerFeatures::SELECT_SOURCE
                | MediaPlayerFeatures::SHUFFLE_SET
                | MediaPlayerFeatures::VOLUME_SET
        );
    }

    #[test]
    fn episode_projection_uses_show_fallback_and_maps_repeat_values() {
        let episode = SpotifyObject {
            kind: Some("episode".into()),
            uri: Some("spotify:episode:episode-id".into()),
            name: Some("Episode title".into()),
            duration_ms: Some(1_500),
            show: Some(Box::new(SpotifyObject {
                name: Some("Podcast name".into()),
                images: Some(vec![image(Some("https://image/show.jpg"))]),
                ..SpotifyObject::default()
            })),
            ..SpotifyObject::default()
        };
        let playback = PlaybackState {
            item: Some(episode),
            device: Some(device(Some("Speaker"), Some(false), Some(37))),
            is_playing: Some(false),
            progress_ms: Some(2_500),
            repeat_state: Some("track".into()),
            ..PlaybackState::default()
        };
        let projected = snapshot(
            &profile(Some("premium")),
            Some(&playback),
            &[],
            None,
            at_fetch_time(),
            Context::default(),
        );

        assert_eq!(projected.state, Some(MediaPlayerState::Paused));
        assert_eq!(projected.media_content_type.as_deref(), Some("podcast"));
        assert_eq!(projected.media_artist.as_deref(), Some("Podcast name"));
        assert_eq!(projected.media_album_name, None);
        assert_eq!(projected.media_track, None);
        assert_eq!(
            projected.entity_picture.as_deref(),
            Some("https://image/show.jpg")
        );
        assert_eq!(projected.media_duration, Some(2.0));
        assert_eq!(projected.media_position, Some(2.0));
        assert_eq!(projected.volume_level, Some(0.37));
        assert_eq!(projected.repeat, Some(RepeatMode::One));

        let off_playback = PlaybackState {
            repeat_state: Some("off".into()),
            ..playback.clone()
        };
        let projected = snapshot(
            &profile(Some("premium")),
            Some(&off_playback),
            &[],
            None,
            at_fetch_time(),
            Context::default(),
        );
        assert_eq!(projected.repeat, Some(RepeatMode::Off));
    }

    #[test]
    fn successful_no_playback_is_idle_and_has_source_list_only_control() {
        let devices = [
            device(Some("Bedroom"), Some(false), None),
            device(Some("Office"), Some(false), None),
        ];
        let fetched_at = at_fetch_time();
        let projected = snapshot(
            &profile(Some("premium")),
            None,
            &devices,
            None,
            fetched_at,
            Context::default(),
        );

        assert_eq!(projected.state, Some(MediaPlayerState::Idle));
        assert_eq!(
            projected.supported_features,
            MediaPlayerFeatures::SELECT_SOURCE
        );
        assert_eq!(
            projected.source_list,
            Some(vec!["Bedroom".into(), "Office".into()])
        );
        assert_eq!(projected.media_position, None);
        assert_eq!(projected.media_position_updated_at, None);
        assert_eq!(projected.source, None);
        assert_eq!(projected.volume_level, None);
        assert_eq!(projected.shuffle, None);
        assert_eq!(projected.repeat, None);
    }

    #[test]
    fn nonpremium_and_unknown_entitlement_advertise_no_controls() {
        let active_playback = playback(
            Some(track_item()),
            Some(device(Some("Speaker"), Some(false), Some(50))),
        );

        for profile in [profile(Some("free")), profile(None)] {
            let projected = snapshot(
                &profile,
                Some(&active_playback),
                &[],
                None,
                at_fetch_time(),
                Context::default(),
            );
            assert_eq!(projected.supported_features, MediaPlayerFeatures::NONE);
        }
    }

    #[test]
    fn restricted_or_unknown_device_restricts_premium_controls() {
        let restricted = playback(None, Some(device(Some("Restricted"), Some(true), Some(50))));
        let unknown_restriction =
            playback(None, Some(device(Some("Maybe restricted"), None, Some(50))));
        let missing_device = playback(None, None);

        for playback in [restricted, unknown_restriction, missing_device] {
            let projected = snapshot(
                &profile(Some("premium")),
                Some(&playback),
                &[],
                None,
                at_fetch_time(),
                Context::default(),
            );
            assert_eq!(
                projected.supported_features,
                MediaPlayerFeatures::SELECT_SOURCE
            );
        }
    }

    #[test]
    fn nullable_and_unknown_items_keep_only_known_metadata() {
        let unknown = SpotifyObject {
            kind: Some("a_future_item".into()),
            uri: Some("spotify:future:item-id".into()),
            name: Some("Future item".into()),
            ..SpotifyObject::default()
        };
        let unknown_playback = PlaybackState {
            item: Some(unknown),
            device: Some(device(Some("Speaker"), None, None)),
            is_playing: None,
            progress_ms: None,
            currently_playing_type: Some("a_future_item".into()),
            repeat_state: Some("future_repeat".into()),
            ..PlaybackState::default()
        };
        let fetched_at = at_fetch_time();
        let projected = snapshot(
            &profile(Some("premium")),
            Some(&unknown_playback),
            &[],
            None,
            fetched_at,
            Context::default(),
        );

        assert_eq!(projected.state, Some(MediaPlayerState::Paused));
        assert_eq!(
            projected.supported_features,
            MediaPlayerFeatures::SELECT_SOURCE
        );
        assert_eq!(
            projected.media_content_id.as_deref(),
            Some("spotify:future:item-id")
        );
        assert_eq!(projected.media_content_type, None);
        assert_eq!(projected.media_title.as_deref(), Some("Future item"));
        assert_eq!(projected.media_duration, None);
        assert_eq!(projected.media_position, None);
        assert_eq!(projected.media_position_updated_at, Some(fetched_at));
        assert_eq!(projected.media_artist, None);
        assert_eq!(projected.media_album_name, None);
        assert_eq!(projected.entity_picture, None);
        assert_eq!(projected.volume_level, None);
        assert_eq!(projected.repeat, None);

        let no_item = playback(None, Some(device(None, Some(false), None)));
        let projected = snapshot(
            &profile(Some("premium")),
            Some(&no_item),
            &[],
            None,
            fetched_at,
            Context::default(),
        );
        assert_eq!(projected.media_content_id, None);
        assert_eq!(projected.media_content_type, None);
        assert_eq!(projected.media_title, None);
        assert_eq!(projected.media_position_updated_at, Some(fetched_at));
    }
}
