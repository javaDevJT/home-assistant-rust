use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Provider fields not yet modeled by this crate.
pub type ExtraFields = Map<String, Value>;

/// The current and legacy Spotify account identifiers from `/me`.
///
/// They remain independent optional fields. Choosing a config-entry identity is
/// a host migration decision, not a wire-model decision.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UserProfile {
    pub account_id: Option<String>,
    pub id: Option<String>,
    pub display_name: Option<String>,
    /// Deprecated by Spotify and intentionally optional.
    pub product: Option<String>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// Common Spotify resource fields used by playback and browse responses.
///
/// `kind` is a string rather than a closed enum so future item kinds remain
/// representable. Unmodeled nested metadata is retained in `extra`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SpotifyObject {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub uri: Option<String>,
    pub href: Option<String>,
    pub name: Option<String>,
    pub external_urls: Option<BTreeMap<String, String>>,
    pub images: Option<Vec<Image>>,
    pub artists: Option<Vec<Artist>>,
    pub album: Option<Box<SpotifyObject>>,
    /// Legacy playlist-entry wrapper field. Album `tracks` are direct Spotify
    /// objects; their entries simply have this field unset.
    pub track: Option<Box<SpotifyObject>>,
    pub show: Option<Box<SpotifyObject>>,
    pub duration_ms: Option<u64>,
    pub is_local: Option<bool>,
    pub explicit: Option<bool>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub album_type: Option<String>,
    pub total_tracks: Option<u32>,
    pub release_date: Option<String>,
    pub release_date_precision: Option<String>,
    pub description: Option<String>,
    pub followers: Option<Value>,
    pub genres: Option<Vec<String>>,
    pub owner: Option<PlaylistOwner>,
    /// Current playlist response page. The legacy `tracks` page is retained in
    /// `tracks` below and remains separate for compatibility.
    #[serde(rename = "items")]
    pub playlist_items: Option<InlinePage<PlaylistItem>>,
    pub tracks: Option<InlinePage<SpotifyObject>>,
    pub episodes: Option<InlinePage<SpotifyObject>>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// Artist identity and display fields used by albums, tracks, and browse items.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Artist {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub uri: Option<String>,
    pub href: Option<String>,
    pub name: Option<String>,
    pub external_urls: Option<BTreeMap<String, String>>,
    pub images: Option<Vec<Image>>,
    pub followers: Option<Value>,
    pub genres: Option<Vec<String>>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlaylistOwner {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub uri: Option<String>,
    pub href: Option<String>,
    pub display_name: Option<String>,
    pub external_urls: Option<BTreeMap<String, String>>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Image {
    pub url: Option<String>,
    pub height: Option<u32>,
    pub width: Option<u32>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// Connect device fields; nullable IDs and volumes stay nullable.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Device {
    pub id: Option<String>,
    pub is_active: Option<bool>,
    pub is_private_session: Option<bool>,
    pub is_restricted: Option<bool>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub device_type: Option<String>,
    pub volume_percent: Option<u32>,
    pub supports_volume: Option<bool>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlaybackContext {
    #[serde(rename = "type")]
    pub context_type: Option<String>,
    pub uri: Option<String>,
    pub href: Option<String>,
    pub external_urls: Option<BTreeMap<String, String>>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// A playback response. A present state with `item: None` is distinct from the
/// `None` returned for a genuine HTTP 204 no-active-playback response.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlaybackState {
    pub device: Option<Device>,
    pub shuffle_state: Option<bool>,
    pub repeat_state: Option<String>,
    pub context: Option<PlaybackContext>,
    pub progress_ms: Option<u64>,
    pub is_playing: Option<bool>,
    pub currently_playing_type: Option<String>,
    pub item: Option<SpotifyObject>,
    pub timestamp: Option<i64>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// A paginated object embedded in a playlist, album, or show response.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InlinePage<T> {
    pub items: Option<Vec<T>>,
    pub next: Option<String>,
    pub total: Option<u64>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

/// One playlist entry. Spotify's current `items[].item` and the legacy
/// `tracks` shape are both retained without discarding unavailable/null items.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlaylistItem {
    pub item: Option<SpotifyObject>,
    pub track: Option<SpotifyObject>,
    pub added_at: Option<String>,
    pub added_by: Option<PlaylistOwner>,
    pub is_local: Option<bool>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

impl PlaylistItem {
    /// Return the current item field, falling back to the legacy track field.
    pub fn media(&self) -> Option<&SpotifyObject> {
        self.item.as_ref().or(self.track.as_ref())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SavedAlbum {
    pub added_at: Option<String>,
    pub album: Option<SpotifyObject>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SavedTrack {
    pub added_at: Option<String>,
    pub track: Option<SpotifyObject>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SavedShow {
    pub added_at: Option<String>,
    pub show: Option<SpotifyObject>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlayedTrack {
    pub played_at: Option<String>,
    pub track: Option<SpotifyObject>,
    pub context: Option<PlaybackContext>,
    #[serde(flatten, default)]
    pub extra: ExtraFields,
}

pub type AlbumTrack = SpotifyObject;
pub type Playlist = SpotifyObject;
pub type Show = SpotifyObject;
pub type Profile = UserProfile;
