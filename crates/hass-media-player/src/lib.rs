//! Native media-player contracts shared by provider integrations and async
//! runtime adapters.
//!
//! The service and entity vocabulary is pinned to Home Assistant Core source
//! commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Provider behavior such as
//! source lookup and media-type policy stays in the provider.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use hass_core::{Context, Error as CoreError, StateChange};
use hass_entity::{EntityHandle, EntityLifecycle, EntitySnapshot, EntitySpec, EntityStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use thiserror::Error;
use tokio::sync::watch;

/// Home Assistant entity domain used by every entity owned by this crate.
pub const MEDIA_PLAYER_DOMAIN: &str = "media_player";

/// Pinned generic `media_player` service catalog, in deterministic order.
pub const MEDIA_PLAYER_SERVICES: &[&str] = &[
    "browse_media",
    "clear_playlist",
    "join",
    "media_next_track",
    "media_pause",
    "media_play",
    "media_play_pause",
    "media_previous_track",
    "media_seek",
    "media_stop",
    "play_media",
    "repeat_set",
    "search_media",
    "select_sound_mode",
    "select_source",
    "shuffle_set",
    "toggle",
    "turn_off",
    "turn_on",
    "unjoin",
    "volume_down",
    "volume_mute",
    "volume_set",
    "volume_up",
];

/// A pinned media-player feature bit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u64)]
pub enum MediaPlayerFeature {
    Pause = 1,
    Seek = 2,
    VolumeSet = 4,
    VolumeMute = 8,
    PreviousTrack = 16,
    NextTrack = 32,
    TurnOn = 128,
    TurnOff = 256,
    PlayMedia = 512,
    VolumeStep = 1024,
    SelectSource = 2048,
    Stop = 4096,
    ClearPlaylist = 8192,
    Play = 16384,
    ShuffleSet = 32768,
    SelectSoundMode = 65536,
    BrowseMedia = 131072,
    RepeatSet = 262144,
    Grouping = 524288,
    MediaAnnounce = 1048576,
    MediaEnqueue = 2097152,
    SearchMedia = 4194304,
}

/// A set of Home Assistant media-player feature bits.
///
/// Unknown bits are retained so a provider can carry a newer upstream mask
/// through an older native runtime without silently rewriting it.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MediaPlayerFeatures(u64);

impl MediaPlayerFeatures {
    pub const NONE: Self = Self(0);
    pub const PAUSE: Self = Self(MediaPlayerFeature::Pause as u64);
    pub const SEEK: Self = Self(MediaPlayerFeature::Seek as u64);
    pub const VOLUME_SET: Self = Self(MediaPlayerFeature::VolumeSet as u64);
    pub const VOLUME_MUTE: Self = Self(MediaPlayerFeature::VolumeMute as u64);
    pub const PREVIOUS_TRACK: Self = Self(MediaPlayerFeature::PreviousTrack as u64);
    pub const NEXT_TRACK: Self = Self(MediaPlayerFeature::NextTrack as u64);
    pub const TURN_ON: Self = Self(MediaPlayerFeature::TurnOn as u64);
    pub const TURN_OFF: Self = Self(MediaPlayerFeature::TurnOff as u64);
    pub const PLAY_MEDIA: Self = Self(MediaPlayerFeature::PlayMedia as u64);
    pub const VOLUME_STEP: Self = Self(MediaPlayerFeature::VolumeStep as u64);
    pub const SELECT_SOURCE: Self = Self(MediaPlayerFeature::SelectSource as u64);
    pub const STOP: Self = Self(MediaPlayerFeature::Stop as u64);
    pub const CLEAR_PLAYLIST: Self = Self(MediaPlayerFeature::ClearPlaylist as u64);
    pub const PLAY: Self = Self(MediaPlayerFeature::Play as u64);
    pub const SHUFFLE_SET: Self = Self(MediaPlayerFeature::ShuffleSet as u64);
    pub const SELECT_SOUND_MODE: Self = Self(MediaPlayerFeature::SelectSoundMode as u64);
    pub const BROWSE_MEDIA: Self = Self(MediaPlayerFeature::BrowseMedia as u64);
    pub const REPEAT_SET: Self = Self(MediaPlayerFeature::RepeatSet as u64);
    pub const GROUPING: Self = Self(MediaPlayerFeature::Grouping as u64);
    pub const MEDIA_ANNOUNCE: Self = Self(MediaPlayerFeature::MediaAnnounce as u64);
    pub const MEDIA_ENQUEUE: Self = Self(MediaPlayerFeature::MediaEnqueue as u64);
    pub const SEARCH_MEDIA: Self = Self(MediaPlayerFeature::SearchMedia as u64);

    /// Construct a feature mask from its raw upstream value.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Return the raw upstream value.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every bit in `required` is present.
    #[must_use]
    pub const fn contains_all(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Whether any bit in `other` is present.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Combine two feature masks in a const context.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl From<MediaPlayerFeature> for MediaPlayerFeatures {
    fn from(feature: MediaPlayerFeature) -> Self {
        Self(feature as u64)
    }
}

impl BitOr for MediaPlayerFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for MediaPlayerFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

impl Serialize for MediaPlayerFeatures {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for MediaPlayerFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Self)
    }
}

/// Home Assistant's standard media-player entity states.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPlayerState {
    Off,
    On,
    Idle,
    Playing,
    Paused,
    Buffering,
}

impl MediaPlayerState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Idle => "idle",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Buffering => "buffering",
        }
    }
}

impl fmt::Display for MediaPlayerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Repeat modes exposed by the common media-player contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

impl fmt::Display for RepeatMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Typed provider state projected onto `hass_entity::EntitySnapshot`.
///
/// Optional state attributes are omitted when `None`; capability lists are
/// emitted only when the matching feature is set and the list is non-empty.
/// A provider supplies `media_position_updated_at`; the SDK does not invent a
/// timestamp from its local clock.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlayerSnapshot {
    pub state: Option<MediaPlayerState>,
    pub available: bool,
    pub supported_features: MediaPlayerFeatures,
    pub context: Context,
    pub volume_level: Option<f64>,
    pub is_volume_muted: Option<bool>,
    pub media_content_id: Option<String>,
    pub media_content_type: Option<String>,
    pub media_duration: Option<f64>,
    pub media_position: Option<f64>,
    pub media_position_updated_at: Option<DateTime<Utc>>,
    pub media_title: Option<String>,
    pub media_artist: Option<String>,
    pub media_album_name: Option<String>,
    pub media_album_artist: Option<String>,
    pub media_track: Option<i64>,
    pub media_series_title: Option<String>,
    pub media_season: Option<String>,
    pub media_episode: Option<String>,
    pub media_channel: Option<String>,
    pub media_playlist: Option<String>,
    pub app_id: Option<String>,
    pub app_name: Option<String>,
    pub source: Option<String>,
    pub source_list: Option<Vec<String>>,
    pub sound_mode: Option<String>,
    pub sound_mode_list: Option<Vec<String>>,
    pub shuffle: Option<bool>,
    pub repeat: Option<RepeatMode>,
    pub entity_picture: Option<String>,
    /// Optional host-local image/proxy URL, supplied only when appropriate.
    pub entity_picture_local: Option<String>,
    pub group_members: Option<Vec<String>>,
}

impl Default for MediaPlayerSnapshot {
    fn default() -> Self {
        Self {
            state: None,
            available: true,
            supported_features: MediaPlayerFeatures::NONE,
            context: Context::default(),
            volume_level: None,
            is_volume_muted: None,
            media_content_id: None,
            media_content_type: None,
            media_duration: None,
            media_position: None,
            media_position_updated_at: None,
            media_title: None,
            media_artist: None,
            media_album_name: None,
            media_album_artist: None,
            media_track: None,
            media_series_title: None,
            media_season: None,
            media_episode: None,
            media_channel: None,
            media_playlist: None,
            app_id: None,
            app_name: None,
            source: None,
            source_list: None,
            sound_mode: None,
            sound_mode_list: None,
            shuffle: None,
            repeat: None,
            entity_picture: None,
            entity_picture_local: None,
            group_members: None,
        }
    }
}

impl MediaPlayerSnapshot {
    /// Project the typed provider snapshot into the shared entity lifecycle.
    #[must_use]
    pub fn to_entity_snapshot(&self) -> EntitySnapshot {
        let is_off = self.state == Some(MediaPlayerState::Off);
        let mut attributes = Map::new();

        // Keep this explicit even when zero so a provider can update its
        // feature mask after registration without leaving stale metadata.
        attributes.insert(
            "supported_features".into(),
            Value::from(self.supported_features.bits()),
        );
        if self
            .supported_features
            .intersects(MediaPlayerFeatures::GROUPING)
            || self.group_members.is_some()
        {
            // Source preserves null when grouping is supported but membership
            // is not known, and preserves membership even while off.
            attributes.insert("group_members".into(), json!(self.group_members));
        }

        if !is_off {
            insert_f64(&mut attributes, "volume_level", self.volume_level);
            insert_option(&mut attributes, "is_volume_muted", self.is_volume_muted);
            insert_option(
                &mut attributes,
                "media_content_id",
                self.media_content_id.as_ref(),
            );
            insert_option(
                &mut attributes,
                "media_content_type",
                self.media_content_type.as_ref(),
            );
            insert_f64(&mut attributes, "media_duration", self.media_duration);
            insert_f64(&mut attributes, "media_position", self.media_position);
            if let Some(position_updated_at) = self.media_position_updated_at.as_ref() {
                attributes.insert(
                    "media_position_updated_at".into(),
                    Value::String(position_updated_at.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
                );
            }
            insert_option(&mut attributes, "media_title", self.media_title.as_ref());
            insert_option(&mut attributes, "media_artist", self.media_artist.as_ref());
            insert_option(
                &mut attributes,
                "media_album_name",
                self.media_album_name.as_ref(),
            );
            insert_option(
                &mut attributes,
                "media_album_artist",
                self.media_album_artist.as_ref(),
            );
            insert_option(&mut attributes, "media_track", self.media_track);
            insert_option(
                &mut attributes,
                "media_series_title",
                self.media_series_title.as_ref(),
            );
            insert_option(&mut attributes, "media_season", self.media_season.as_ref());
            insert_option(
                &mut attributes,
                "media_episode",
                self.media_episode.as_ref(),
            );
            insert_option(
                &mut attributes,
                "media_channel",
                self.media_channel.as_ref(),
            );
            insert_option(
                &mut attributes,
                "media_playlist",
                self.media_playlist.as_ref(),
            );
            insert_option(&mut attributes, "app_id", self.app_id.as_ref());
            insert_option(&mut attributes, "app_name", self.app_name.as_ref());
            insert_option(&mut attributes, "source", self.source.as_ref());
            insert_option(&mut attributes, "sound_mode", self.sound_mode.as_ref());
            insert_option(&mut attributes, "shuffle", self.shuffle);
            insert_option(
                &mut attributes,
                "repeat",
                self.repeat.map(RepeatMode::as_str),
            );
            insert_option(
                &mut attributes,
                "entity_picture",
                self.entity_picture.as_ref(),
            );
            insert_option(
                &mut attributes,
                "entity_picture_local",
                self.entity_picture_local.as_ref(),
            );
        }

        if self
            .supported_features
            .intersects(MediaPlayerFeatures::SELECT_SOURCE)
            && let Some(source_list) = self.source_list.as_ref().filter(|items| !items.is_empty())
        {
            attributes.insert("source_list".into(), json!(source_list));
        }
        if self
            .supported_features
            .intersects(MediaPlayerFeatures::SELECT_SOUND_MODE)
            && let Some(sound_mode_list) = self
                .sound_mode_list
                .as_ref()
                .filter(|items| !items.is_empty())
        {
            attributes.insert("sound_mode_list".into(), json!(sound_mode_list));
        }

        EntitySnapshot {
            state: self.state.map(|state| state.as_str().to_owned()),
            available: self.available,
            attributes,
            context: self.context.clone(),
        }
    }
}

fn insert_f64(attributes: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(Number::from_f64) {
        attributes.insert(key.to_owned(), Value::Number(number));
    }
}

fn insert_option<T: Serialize>(attributes: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        attributes.insert(key.to_owned(), value);
    }
}

/// Media content classes accepted by the pinned `media_filter_classes` schema.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaClass {
    Album,
    App,
    Artist,
    Channel,
    Composer,
    ContributingArtist,
    Directory,
    Episode,
    Game,
    Genre,
    Image,
    Movie,
    Music,
    Playlist,
    Podcast,
    Season,
    Track,
    TvShow,
    Url,
    Video,
}

impl MediaClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::App => "app",
            Self::Artist => "artist",
            Self::Channel => "channel",
            Self::Composer => "composer",
            Self::ContributingArtist => "contributing_artist",
            Self::Directory => "directory",
            Self::Episode => "episode",
            Self::Game => "game",
            Self::Genre => "genre",
            Self::Image => "image",
            Self::Movie => "movie",
            Self::Music => "music",
            Self::Playlist => "playlist",
            Self::Podcast => "podcast",
            Self::Season => "season",
            Self::Track => "track",
            Self::TvShow => "tv_show",
            Self::Url => "url",
            Self::Video => "video",
        }
    }
}

impl fmt::Display for MediaClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for MediaClass {
    type Error = MediaPlayerError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "album" => Ok(Self::Album),
            "app" => Ok(Self::App),
            "artist" => Ok(Self::Artist),
            "channel" => Ok(Self::Channel),
            "composer" => Ok(Self::Composer),
            "contributing_artist" => Ok(Self::ContributingArtist),
            "directory" => Ok(Self::Directory),
            "episode" => Ok(Self::Episode),
            "game" => Ok(Self::Game),
            "genre" => Ok(Self::Genre),
            "image" => Ok(Self::Image),
            "movie" => Ok(Self::Movie),
            "music" => Ok(Self::Music),
            "playlist" => Ok(Self::Playlist),
            "podcast" => Ok(Self::Podcast),
            "season" => Ok(Self::Season),
            "track" => Ok(Self::Track),
            "tv_show" => Ok(Self::TvShow),
            "url" => Ok(Self::Url),
            "video" => Ok(Self::Video),
            _ => Err(MediaPlayerError::InvalidFormat(format!(
                "unknown media_filter_classes value `{value}`"
            ))),
        }
    }
}

/// Provider-defined browse request. Content type and content ID are
/// independently optional for services, while direct browse commands require
/// both or neither.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrowseMediaRequest {
    pub media_content_type: Option<String>,
    pub media_content_id: Option<String>,
}

impl BrowseMediaRequest {
    fn parse_service(data: &mut Map<String, Value>) -> Result<Self, MediaPlayerError> {
        Ok(Self {
            media_content_type: take_optional_string(data, "media_content_type")?,
            media_content_id: take_optional_string(data, "media_content_id")?,
        })
    }

    /// Parse the stricter direct `media_player/browse_media` command schema.
    pub fn parse_command(data: &Map<String, Value>) -> Result<Self, MediaPlayerError> {
        let mut remaining = data.clone();
        reject_entity_id_argument(&remaining)?;
        let request = Self::parse_service(&mut remaining)?;
        if request.media_content_type.is_some() != request.media_content_id.is_some() {
            return Err(MediaPlayerError::InvalidFormat(
                "media_content_type and media_content_id must be supplied together".into(),
            ));
        }
        reject_remaining_fields(&remaining)?;
        Ok(request)
    }
}

/// Provider-defined search request with validated media class filters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMediaRequest {
    pub search_query: String,
    pub media_content_type: Option<String>,
    pub media_content_id: Option<String>,
    pub media_filter_classes: Option<Vec<MediaClass>>,
}

impl SearchMediaRequest {
    fn parse_service(data: &mut Map<String, Value>) -> Result<Self, MediaPlayerError> {
        let search_query = take_required_string(data, "search_query")?;
        let media_content_type = take_optional_string(data, "media_content_type")?;
        let media_content_id = take_optional_string(data, "media_content_id")?;
        let media_filter_classes = take_optional_media_classes(data, "media_filter_classes")?;
        Ok(Self {
            search_query,
            media_content_type,
            media_content_id,
            media_filter_classes,
        })
    }

    /// Parse the stricter direct `media_player/search_media` command schema.
    pub fn parse_command(data: &Map<String, Value>) -> Result<Self, MediaPlayerError> {
        let mut remaining = data.clone();
        reject_entity_id_argument(&remaining)?;
        let request = Self::parse_service(&mut remaining)?;
        if request.media_content_type.is_some() != request.media_content_id.is_some() {
            return Err(MediaPlayerError::InvalidFormat(
                "media_content_type and media_content_id must be supplied together".into(),
            ));
        }
        reject_remaining_fields(&remaining)?;
        Ok(request)
    }
}

/// One browse-tree node. `media_class` and content type remain strings because
/// providers may use extensions; request class filters are the validated enum.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowseMedia {
    pub title: String,
    pub media_class: String,
    pub media_content_type: String,
    pub media_content_id: String,
    pub children_media_class: Option<String>,
    pub can_play: bool,
    pub can_expand: bool,
    pub can_search: bool,
    pub search_media_classes: Option<Vec<String>>,
    pub thumbnail: Option<String>,
    pub not_shown: u64,
    pub children: Vec<BrowseMedia>,
}

impl BrowseMedia {
    /// Create a browse node with required upstream fields.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        media_class: impl Into<String>,
        media_content_type: impl Into<String>,
        media_content_id: impl Into<String>,
        can_play: bool,
        can_expand: bool,
    ) -> Self {
        Self {
            title: title.into(),
            media_class: media_class.into(),
            media_content_type: media_content_type.into(),
            media_content_id: media_content_id.into(),
            children_media_class: None,
            can_play,
            can_expand,
            can_search: false,
            search_media_classes: None,
            thumbnail: None,
            not_shown: 0,
            children: Vec::new(),
        }
    }

    /// Serialize the root tree in the Home Assistant browse-service shape.
    /// Children are shallow nodes and do not recursively contain children.
    #[must_use]
    pub fn to_service_response(&self) -> Value {
        let mut response = self.item_value();
        if let Value::Object(ref mut fields) = response {
            fields.insert("not_shown".into(), Value::from(self.not_shown));
            fields.insert(
                "children".into(),
                Value::Array(self.children.iter().map(Self::item_value).collect()),
            );
        }
        response
    }

    fn item_value(&self) -> Value {
        let children_media_class = self
            .children_media_class
            .clone()
            .or_else(|| self.calculated_children_class());
        json!({
            "title": self.title,
            "media_class": self.media_class,
            "media_content_type": self.media_content_type,
            "media_content_id": self.media_content_id,
            "children_media_class": children_media_class,
            "can_play": self.can_play,
            "can_expand": self.can_expand,
            "can_search": self.can_search,
            "search_media_classes": self.search_media_classes,
            "thumbnail": self.thumbnail,
        })
    }

    fn calculated_children_class(&self) -> Option<String> {
        let (first, rest) = self.children.split_first()?;
        if rest
            .iter()
            .all(|child| child.media_class == first.media_class)
        {
            Some(first.media_class.clone())
        } else {
            Some(MediaClass::Directory.as_str().into())
        }
    }
}

/// Search response. The service payload intentionally contains only
/// `{ "result": [...] }`; each result node uses the pinned browse serializer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMedia {
    pub version: u8,
    pub result: Vec<BrowseMedia>,
}

impl Default for SearchMedia {
    fn default() -> Self {
        Self {
            version: 1,
            result: Vec::new(),
        }
    }
}

impl SearchMedia {
    #[must_use]
    pub fn new(result: Vec<BrowseMedia>) -> Self {
        Self { version: 1, result }
    }

    #[must_use]
    pub fn to_service_response(&self) -> Value {
        json!({
            "result": self.result.iter().map(BrowseMedia::to_service_response).collect::<Vec<_>>(),
        })
    }
}

/// How a provider should enqueue media when handling `play_media`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaEnqueue {
    Play,
    Next,
    Add,
    Replace,
}

/// Validated `play_media` arguments. Queue and announce are exclusive in the
/// pinned schema; provider-specific meaning remains with the provider.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayMediaCommand {
    pub media_content_type: String,
    pub media_content_id: String,
    pub enqueue: Option<MediaEnqueue>,
    pub announce: Option<bool>,
    pub extra: Map<String, Value>,
}

/// Typed command data after validating a generic media-player service call.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommandKind {
    TurnOn,
    TurnOff,
    Toggle,
    VolumeUp,
    VolumeDown,
    VolumeMute { is_volume_muted: bool },
    VolumeSet { volume_level: f64 },
    MediaPlayPause,
    MediaPlay,
    MediaPause,
    MediaStop,
    MediaNextTrack,
    MediaPreviousTrack,
    MediaSeek { seek_position: f64 },
    PlayMedia(PlayMediaCommand),
    BrowseMedia(BrowseMediaRequest),
    SearchMedia(SearchMediaRequest),
    SelectSource { source: String },
    SelectSoundMode { sound_mode: String },
    ClearPlaylist,
    ShuffleSet { shuffle: bool },
    RepeatSet { repeat: RepeatMode },
    Join { group_members: Vec<String> },
    Unjoin,
}

/// Parsed common command plus provider-specific extra service data.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaCommand {
    pub kind: MediaCommandKind,
    pub extra: Map<String, Value>,
}

/// Feature-gate semantics for a service. Each mask is ANDed internally;
/// multiple masks are ORed. Zero alternatives means no runtime gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureRequirements {
    alternatives: [MediaPlayerFeatures; 2],
    alternatives_len: u8,
}

impl FeatureRequirements {
    const fn none() -> Self {
        Self {
            alternatives: [MediaPlayerFeatures::NONE; 2],
            alternatives_len: 0,
        }
    }

    const fn one(required_all: MediaPlayerFeatures) -> Self {
        Self {
            alternatives: [required_all, MediaPlayerFeatures::NONE],
            alternatives_len: 1,
        }
    }

    const fn either(first: MediaPlayerFeatures, second: MediaPlayerFeatures) -> Self {
        Self {
            alternatives: [first, second],
            alternatives_len: 2,
        }
    }

    /// Masks that are alternatives; all bits in one mask are required.
    #[must_use]
    pub fn alternatives(&self) -> &[MediaPlayerFeatures] {
        &self.alternatives[..usize::from(self.alternatives_len)]
    }

    /// Whether a feature set satisfies at least one alternative.
    #[must_use]
    pub fn is_satisfied_by(self, features: MediaPlayerFeatures) -> bool {
        self.alternatives_len == 0
            || self.alternatives[..usize::from(self.alternatives_len)]
                .iter()
                .copied()
                .any(|required| features.contains_all(required))
    }
}

impl MediaCommand {
    /// Validate a generic `media_player` service name and its service data.
    pub fn parse_service(
        service: &str,
        data: &Map<String, Value>,
    ) -> Result<Self, MediaPlayerError> {
        let mut remaining = data.clone();
        reject_entity_id_argument(&remaining)?;
        remaining.remove("metadata");
        let kind = match service {
            "turn_on" => MediaCommandKind::TurnOn,
            "turn_off" => MediaCommandKind::TurnOff,
            "toggle" => MediaCommandKind::Toggle,
            "volume_up" => MediaCommandKind::VolumeUp,
            "volume_down" => MediaCommandKind::VolumeDown,
            "volume_mute" => MediaCommandKind::VolumeMute {
                is_volume_muted: take_required_bool(&mut remaining, "is_volume_muted")?,
            },
            "volume_set" => {
                let volume_level = take_required_number(&mut remaining, "volume_level")?;
                if !(0.0..=1.0).contains(&volume_level) {
                    return Err(MediaPlayerError::InvalidFormat(
                        "volume_level must be between 0 and 1".into(),
                    ));
                }
                MediaCommandKind::VolumeSet { volume_level }
            }
            "media_play_pause" => MediaCommandKind::MediaPlayPause,
            "media_play" => MediaCommandKind::MediaPlay,
            "media_pause" => MediaCommandKind::MediaPause,
            "media_stop" => MediaCommandKind::MediaStop,
            "media_next_track" => MediaCommandKind::MediaNextTrack,
            "media_previous_track" => MediaCommandKind::MediaPreviousTrack,
            "media_seek" => {
                let seek_position = take_required_number(&mut remaining, "seek_position")?;
                if seek_position < 0.0 || !seek_position.is_finite() {
                    return Err(MediaPlayerError::InvalidFormat(
                        "seek_position must be a non-negative number".into(),
                    ));
                }
                MediaCommandKind::MediaSeek { seek_position }
            }
            "play_media" => MediaCommandKind::PlayMedia(parse_play_media(&mut remaining)?),
            "browse_media" => {
                MediaCommandKind::BrowseMedia(BrowseMediaRequest::parse_service(&mut remaining)?)
            }
            "search_media" => {
                MediaCommandKind::SearchMedia(SearchMediaRequest::parse_service(&mut remaining)?)
            }
            "select_source" => MediaCommandKind::SelectSource {
                source: take_required_string(&mut remaining, "source")?,
            },
            "select_sound_mode" => MediaCommandKind::SelectSoundMode {
                sound_mode: take_required_string(&mut remaining, "sound_mode")?,
            },
            "clear_playlist" => MediaCommandKind::ClearPlaylist,
            "shuffle_set" => MediaCommandKind::ShuffleSet {
                shuffle: take_required_bool(&mut remaining, "shuffle")?,
            },
            "repeat_set" => MediaCommandKind::RepeatSet {
                repeat: take_required_repeat(&mut remaining, "repeat")?,
            },
            "join" => MediaCommandKind::Join {
                group_members: take_required_string_list(&mut remaining, "group_members")?,
            },
            "unjoin" => MediaCommandKind::Unjoin,
            unknown => return Err(MediaPlayerError::UnknownService(unknown.into())),
        };
        reject_remaining_fields(&remaining)?;
        Ok(Self {
            kind,
            extra: remaining,
        })
    }

    /// Canonical upstream service name.
    #[must_use]
    pub const fn service_name(&self) -> &'static str {
        match &self.kind {
            MediaCommandKind::TurnOn => "turn_on",
            MediaCommandKind::TurnOff => "turn_off",
            MediaCommandKind::Toggle => "toggle",
            MediaCommandKind::VolumeUp => "volume_up",
            MediaCommandKind::VolumeDown => "volume_down",
            MediaCommandKind::VolumeMute { .. } => "volume_mute",
            MediaCommandKind::VolumeSet { .. } => "volume_set",
            MediaCommandKind::MediaPlayPause => "media_play_pause",
            MediaCommandKind::MediaPlay => "media_play",
            MediaCommandKind::MediaPause => "media_pause",
            MediaCommandKind::MediaStop => "media_stop",
            MediaCommandKind::MediaNextTrack => "media_next_track",
            MediaCommandKind::MediaPreviousTrack => "media_previous_track",
            MediaCommandKind::MediaSeek { .. } => "media_seek",
            MediaCommandKind::PlayMedia(_) => "play_media",
            MediaCommandKind::BrowseMedia(_) => "browse_media",
            MediaCommandKind::SearchMedia(_) => "search_media",
            MediaCommandKind::SelectSource { .. } => "select_source",
            MediaCommandKind::SelectSoundMode { .. } => "select_sound_mode",
            MediaCommandKind::ClearPlaylist => "clear_playlist",
            MediaCommandKind::ShuffleSet { .. } => "shuffle_set",
            MediaCommandKind::RepeatSet { .. } => "repeat_set",
            MediaCommandKind::Join { .. } => "join",
            MediaCommandKind::Unjoin => "unjoin",
        }
    }

    /// Runtime service feature requirements. Multiple masks are alternatives;
    /// every bit inside one mask is required. Browse service registration has
    /// no runtime gate in the pinned source even though its YAML metadata does.
    #[must_use]
    pub const fn feature_requirements(&self) -> FeatureRequirements {
        match &self.kind {
            MediaCommandKind::TurnOn => FeatureRequirements::one(MediaPlayerFeatures::TURN_ON),
            MediaCommandKind::TurnOff => FeatureRequirements::one(MediaPlayerFeatures::TURN_OFF),
            MediaCommandKind::Toggle => FeatureRequirements::one(
                MediaPlayerFeatures::TURN_ON.union(MediaPlayerFeatures::TURN_OFF),
            ),
            MediaCommandKind::VolumeUp | MediaCommandKind::VolumeDown => {
                FeatureRequirements::either(
                    MediaPlayerFeatures::VOLUME_SET,
                    MediaPlayerFeatures::VOLUME_STEP,
                )
            }
            MediaCommandKind::VolumeMute { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::VOLUME_MUTE)
            }
            MediaCommandKind::VolumeSet { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::VOLUME_SET)
            }
            MediaCommandKind::MediaPlayPause => FeatureRequirements::one(
                MediaPlayerFeatures::PLAY.union(MediaPlayerFeatures::PAUSE),
            ),
            MediaCommandKind::MediaPlay => FeatureRequirements::one(MediaPlayerFeatures::PLAY),
            MediaCommandKind::MediaPause => FeatureRequirements::one(MediaPlayerFeatures::PAUSE),
            MediaCommandKind::MediaStop => FeatureRequirements::one(MediaPlayerFeatures::STOP),
            MediaCommandKind::MediaNextTrack => {
                FeatureRequirements::one(MediaPlayerFeatures::NEXT_TRACK)
            }
            MediaCommandKind::MediaPreviousTrack => {
                FeatureRequirements::one(MediaPlayerFeatures::PREVIOUS_TRACK)
            }
            MediaCommandKind::MediaSeek { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::SEEK)
            }
            MediaCommandKind::PlayMedia(_) => {
                FeatureRequirements::one(MediaPlayerFeatures::PLAY_MEDIA)
            }
            MediaCommandKind::BrowseMedia(_) => FeatureRequirements::none(),
            MediaCommandKind::SearchMedia(_) => {
                FeatureRequirements::one(MediaPlayerFeatures::SEARCH_MEDIA)
            }
            MediaCommandKind::SelectSource { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::SELECT_SOURCE)
            }
            MediaCommandKind::SelectSoundMode { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::SELECT_SOUND_MODE)
            }
            MediaCommandKind::ClearPlaylist => {
                FeatureRequirements::one(MediaPlayerFeatures::CLEAR_PLAYLIST)
            }
            MediaCommandKind::ShuffleSet { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::SHUFFLE_SET)
            }
            MediaCommandKind::RepeatSet { .. } => {
                FeatureRequirements::one(MediaPlayerFeatures::REPEAT_SET)
            }
            MediaCommandKind::Join { .. } | MediaCommandKind::Unjoin => {
                FeatureRequirements::one(MediaPlayerFeatures::GROUPING)
            }
        }
    }

    /// YAML target metadata requirements. This differs from runtime checks for
    /// `browse_media`; direct browse commands also enforce `BROWSE_MEDIA`.
    #[must_use]
    pub const fn target_metadata_requirements(&self) -> FeatureRequirements {
        match &self.kind {
            MediaCommandKind::BrowseMedia(_) => {
                FeatureRequirements::one(MediaPlayerFeatures::BROWSE_MEDIA)
            }
            _ => self.feature_requirements(),
        }
    }

    /// Whether this command is accepted by a provider's current feature mask.
    #[must_use]
    pub fn is_supported_by(&self, features: MediaPlayerFeatures) -> bool {
        self.feature_requirements().is_satisfied_by(features)
    }
}

fn parse_play_media(data: &mut Map<String, Value>) -> Result<PlayMediaCommand, MediaPlayerError> {
    let media_object = data.remove("media");
    let (media_content_type, media_content_id) = if let Some(media) = media_object {
        if data.contains_key("media_content_type") || data.contains_key("media_content_id") {
            return Err(MediaPlayerError::InvalidFormat(
                "media cannot be combined with top-level media_content_type or media_content_id"
                    .into(),
            ));
        }
        let Value::Object(mut media) = media else {
            return Err(MediaPlayerError::InvalidFormat(
                "media must be an object".into(),
            ));
        };
        let media_content_type = take_required_string(&mut media, "media_content_type")?;
        let media_content_id = take_required_string(&mut media, "media_content_id")?;
        reject_remaining_fields(&media)?;
        (media_content_type, media_content_id)
    } else {
        (
            take_required_string(data, "media_content_type")?,
            take_required_string(data, "media_content_id")?,
        )
    };

    let enqueue_value = data.remove("enqueue");
    let announce = take_optional_bool(data, "announce")?;
    if enqueue_value.is_some() && announce.is_some() {
        return Err(MediaPlayerError::InvalidFormat(
            "enqueue and announce are mutually exclusive".into(),
        ));
    }
    let enqueue = match enqueue_value {
        None => None,
        Some(Value::String(value)) => Some(match value.as_str() {
            "play" => MediaEnqueue::Play,
            "next" => MediaEnqueue::Next,
            "add" => MediaEnqueue::Add,
            "replace" => MediaEnqueue::Replace,
            _ => {
                return Err(MediaPlayerError::InvalidFormat(
                    "enqueue must be one of play, next, add, or replace".into(),
                ));
            }
        }),
        // The pinned compatibility schema promotes the deprecated bool to
        // `add` or `play`; warning output belongs to the host integration.
        Some(Value::Bool(value)) => Some(if value {
            MediaEnqueue::Add
        } else {
            MediaEnqueue::Play
        }),
        Some(_) => {
            return Err(MediaPlayerError::InvalidFormat(
                "enqueue must be a string or legacy boolean".into(),
            ));
        }
    };
    let extra = match data.remove("extra") {
        None => Map::new(),
        Some(Value::Object(extra)) => extra,
        Some(_) => {
            return Err(MediaPlayerError::InvalidFormat(
                "extra must be an object".into(),
            ));
        }
    };
    Ok(PlayMediaCommand {
        media_content_type,
        media_content_id,
        enqueue,
        announce,
        extra,
    })
}

fn take_required_string(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<String, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a string"
        ))),
        None => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} is required"
        ))),
    }
}

fn take_optional_string(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<Option<String>, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a string"
        ))),
        None => Ok(None),
    }
}

fn take_required_bool(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<bool, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::Bool(value)) => Ok(value),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a boolean"
        ))),
        None => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} is required"
        ))),
    }
}

fn take_optional_bool(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<Option<bool>, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a boolean"
        ))),
        None => Ok(None),
    }
}

fn take_required_number(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<f64, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::Number(value)) => value.as_f64().ok_or_else(|| {
            MediaPlayerError::InvalidFormat(format!("{field} must be a finite number"))
        }),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a number"
        ))),
        None => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} is required"
        ))),
    }
}

fn take_required_repeat(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<RepeatMode, MediaPlayerError> {
    match take_required_string(data, field)?.as_str() {
        "off" => Ok(RepeatMode::Off),
        "all" => Ok(RepeatMode::All),
        "one" => Ok(RepeatMode::One),
        _ => Err(MediaPlayerError::InvalidFormat(
            "repeat must be one of off, all, or one".into(),
        )),
    }
}

fn take_required_string_list(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<Vec<String>, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) if is_valid_entity_id(&value) => Ok(value),
                Value::String(value) => Err(MediaPlayerError::InvalidFormat(format!(
                    "{field} contains invalid entity ID `{value}`"
                ))),
                _ => Err(MediaPlayerError::InvalidFormat(format!(
                    "{field} items must be strings"
                ))),
            })
            .collect(),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a list"
        ))),
        None => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} is required"
        ))),
    }
}

fn is_valid_entity_id(value: &str) -> bool {
    let Some((domain, object_id)) = value.split_once('.') else {
        return false;
    };
    !object_id.contains('.')
        && is_valid_entity_component(domain)
        && is_valid_entity_component(object_id)
}

fn is_valid_entity_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn take_optional_media_classes(
    data: &mut Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<MediaClass>>, MediaPlayerError> {
    match data.remove(field) {
        Some(Value::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => MediaClass::try_from(value.as_str()),
                _ => Err(MediaPlayerError::InvalidFormat(format!(
                    "{field} items must be media class strings"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(MediaPlayerError::InvalidFormat(format!(
            "{field} must be a list"
        ))),
        None => Ok(None),
    }
}

fn reject_entity_id_argument(data: &Map<String, Value>) -> Result<(), MediaPlayerError> {
    if data.contains_key("entity_id") {
        return Err(MediaPlayerError::InvalidFormat(
            "entity_id must be resolved as a service target before command parsing".into(),
        ));
    }
    Ok(())
}

fn reject_remaining_fields(data: &Map<String, Value>) -> Result<(), MediaPlayerError> {
    if let Some(field) = data.keys().next() {
        return Err(MediaPlayerError::InvalidFormat(format!(
            "unexpected field `{field}`"
        )));
    }
    Ok(())
}

/// Stable error categories for API adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPlayerErrorCode {
    InvalidFormat,
    ServiceNotFound,
    EntityNotFound,
    NotSupported,
    UnknownError,
    Cancelled,
}

impl MediaPlayerErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidFormat => "invalid_format",
            Self::ServiceNotFound => "service_not_found",
            Self::EntityNotFound => "entity_not_found",
            Self::NotSupported => "not_supported",
            Self::UnknownError => "unknown_error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Errors produced by common schema, feature, lifecycle, and provider paths.
#[derive(Debug, Error)]
pub enum MediaPlayerError {
    #[error("invalid media-player data: {0}")]
    InvalidFormat(String),
    #[error("service not found: {0}")]
    UnknownService(String),
    #[error("Entity not found")]
    EntityNotFound,
    #[error("{0}")]
    NotSupported(String),
    // Preserve provider exception text for browse/search `unknown_error`
    // payloads, matching the pinned direct-command surface.
    #[error("{0}")]
    Provider(String),
    #[error("provider does not implement {0}")]
    NotImplemented(&'static str),
    #[error("media-player provider was unloaded")]
    Cancelled,
    #[error(transparent)]
    Lifecycle(#[from] CoreError),
}

impl MediaPlayerError {
    #[must_use]
    pub const fn code(&self) -> MediaPlayerErrorCode {
        match self {
            Self::InvalidFormat(_) => MediaPlayerErrorCode::InvalidFormat,
            Self::UnknownService(_) => MediaPlayerErrorCode::ServiceNotFound,
            Self::EntityNotFound => MediaPlayerErrorCode::EntityNotFound,
            Self::NotSupported(_) | Self::NotImplemented(_) => MediaPlayerErrorCode::NotSupported,
            Self::Cancelled => MediaPlayerErrorCode::Cancelled,
            Self::Provider(_) | Self::Lifecycle(_) => MediaPlayerErrorCode::UnknownError,
        }
    }
}

/// Result type exported by the media-player SDK.
pub type MediaPlayerResult<T> = Result<T, MediaPlayerError>;

/// Provider-owned identity and initial features for one media player.
///
/// `identity.provider_key` is the stable per-provider key. Registration
/// replaces its domain and provider namespace with `media_player` and the
/// supplied owner, then adds the media-player history exclusions.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlayerEntitySpec {
    pub identity: EntitySpec,
    pub supported_features: MediaPlayerFeatures,
}

impl MediaPlayerEntitySpec {
    #[must_use]
    pub fn new(identity: EntitySpec, supported_features: MediaPlayerFeatures) -> Self {
        Self {
            identity,
            supported_features,
        }
    }
}

/// One provider command completed without further policy decisions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderCommandOutcome {
    #[default]
    Executed,
    /// The provider intentionally treated a valid command as a no-op, such as
    /// a source name that is no longer present in its cache.
    NoOp,
}

/// Call context passed to provider futures. `entity_id` is resolved from the
/// current lifecycle handle at dispatch time and is never cached by the SDK.
#[derive(Clone, Debug)]
pub struct MediaPlayerProviderContext {
    owner_id: String,
    provider_key: String,
    entity_id: String,
    context: Context,
    cancellation: watch::Receiver<bool>,
}

impl MediaPlayerProviderContext {
    #[must_use]
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    #[must_use]
    pub fn provider_key(&self) -> &str {
        &self.provider_key
    }

    #[must_use]
    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }

    /// Caller context for a service/API callback. Snapshot reads use the
    /// default context unless the provider returns a more specific snapshot
    /// context for publication.
    #[must_use]
    pub fn context(&self) -> &Context {
        &self.context
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow() || self.cancellation.has_changed().is_err()
    }

    /// Wait for the owner registration to unload or be replaced.
    pub async fn cancelled(&self) {
        let mut receiver = self.cancellation.clone();
        wait_for_cancellation(&mut receiver).await;
    }
}

/// Cloneable owner-cancellation signal that does not retain the RAII
/// registration itself. Providers can move it into their owned poll task and
/// still let dropping the final `MediaPlayerRegistration` trigger cleanup.
#[derive(Clone, Debug)]
pub struct MediaPlayerCancellation {
    receiver: watch::Receiver<bool>,
}

impl MediaPlayerCancellation {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        wait_for_cancellation(&mut receiver).await;
    }
}

/// Asynchronous provider surface for cached entity state and media operations.
///
/// The common SDK validates schema and feature requirements. Provider policy
/// such as source-name resolution, unsupported media handling, and remote API
/// behavior belongs in these methods.
#[async_trait]
pub trait MediaPlayerProvider: Send + Sync + 'static {
    /// Return the provider's current cached snapshot. This must not perform an
    /// unbounded refresh; integrations own their cache/polling lifecycle.
    async fn snapshot(
        &self,
        context: &MediaPlayerProviderContext,
    ) -> MediaPlayerResult<MediaPlayerSnapshot>;

    /// Execute a validated common service command.
    async fn execute(
        &self,
        context: &MediaPlayerProviderContext,
        command: MediaCommand,
    ) -> MediaPlayerResult<ProviderCommandOutcome>;

    /// Browse a provider's media tree. Unsupported providers can use the
    /// default implementation and the dispatcher maps it to `not_supported`.
    async fn browse(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: BrowseMediaRequest,
    ) -> MediaPlayerResult<BrowseMedia> {
        Err(MediaPlayerError::NotImplemented("browse_media"))
    }

    /// Search a provider's media tree.
    async fn search(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: SearchMediaRequest,
    ) -> MediaPlayerResult<SearchMedia> {
        Err(MediaPlayerError::NotImplemented("search_media"))
    }
}

#[derive(Clone)]
struct RegisteredEntity {
    handle: EntityHandle,
    features: Arc<AtomicU64>,
    available: Arc<AtomicBool>,
}

struct ProviderRegistrationState {
    owner_id: String,
    generation: u64,
    provider: Arc<dyn MediaPlayerProvider>,
    cancellation: watch::Sender<bool>,
    entities: BTreeMap<String, RegisteredEntity>,
}

impl ProviderRegistrationState {
    fn cancel(&self) {
        self.cancellation.send_replace(true);
    }

    fn context(
        &self,
        provider_key: &str,
        entity_id: String,
        context: Context,
    ) -> MediaPlayerProviderContext {
        MediaPlayerProviderContext {
            owner_id: self.owner_id.clone(),
            provider_key: provider_key.to_owned(),
            entity_id,
            context,
            cancellation: self.cancellation.subscribe(),
        }
    }
}

struct DispatcherInner {
    lifecycle: EntityLifecycle,
    registrations: RwLock<BTreeMap<String, Arc<ProviderRegistrationState>>>,
    registration_gate: Mutex<()>,
    next_generation: AtomicU64,
}

/// Async dispatcher over shared entity lifecycle leases.
///
/// The dispatcher only holds its registration map while cloning lightweight
/// handles. Provider futures, entity publication, and lease release all run
/// after those locks have been dropped.
#[derive(Clone)]
pub struct MediaPlayerDispatcher {
    inner: Arc<DispatcherInner>,
}

impl fmt::Debug for MediaPlayerDispatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let owners = self
            .inner
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        formatter
            .debug_struct("MediaPlayerDispatcher")
            .field("owners", &owners)
            .finish()
    }
}

impl MediaPlayerDispatcher {
    /// Match a current, enabled entity without invoking provider code.
    #[must_use]
    pub fn owns_entity(&self, entity_id: &str) -> bool {
        self.candidate_for_entity(entity_id).is_some()
    }

    /// Bind an async dispatcher to the application's shared entity lifecycle.
    #[must_use]
    pub fn new(lifecycle: EntityLifecycle) -> Self {
        Self {
            inner: Arc::new(DispatcherInner {
                lifecycle,
                registrations: RwLock::new(BTreeMap::new()),
                registration_gate: Mutex::new(()),
                next_generation: AtomicU64::new(1),
            }),
        }
    }

    /// Register or replace every media-player entity for one provider owner.
    /// Keep the returned lease for the provider lifetime; dropping its final
    /// clone unloads only this exact generation.
    pub fn register_provider(
        &self,
        owner_id: impl Into<String>,
        provider: Arc<dyn MediaPlayerProvider>,
        entities: Vec<MediaPlayerEntitySpec>,
    ) -> MediaPlayerResult<MediaPlayerRegistration> {
        let owner_id = owner_id.into();
        let _gate = self
            .inner
            .registration_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut seen_keys = BTreeSet::new();
        let mut keys = Vec::with_capacity(entities.len());
        let mut specs = Vec::with_capacity(entities.len());
        for entity in &entities {
            let key = entity.identity.provider_key.clone();
            if !seen_keys.insert(key.clone()) {
                return Err(MediaPlayerError::InvalidFormat(format!(
                    "duplicate media-player provider key `{key}`"
                )));
            }
            let mut identity = entity.identity.clone();
            identity.domain = MEDIA_PLAYER_DOMAIN.to_owned();
            identity.provider_id = owner_id.clone();
            identity.info.supported_features = entity.supported_features.bits();
            specs.push(identity.with_unrecorded_attributes([
                "entity_picture",
                "entity_picture_local",
                "media_position",
                "media_position_updated_at",
                "source_list",
                "sound_mode_list",
            ]));
            keys.push(key);
        }

        // EntityLifecycle owns registry IDs and provider identity receipts.
        // This call is intentionally outside the dispatcher's registration-map
        // lock; the gate only serializes concurrent registrations.
        let handles = self
            .inner
            .lifecycle
            .register_provider_batch(&owner_id, specs)?;
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        let (cancellation, cancellation_receiver) = watch::channel(false);
        let mut registered_entities = BTreeMap::new();
        for ((key, entity), handle) in keys.into_iter().zip(entities).zip(handles) {
            registered_entities.insert(
                key,
                RegisteredEntity {
                    handle,
                    features: Arc::new(AtomicU64::new(entity.supported_features.bits())),
                    available: Arc::new(AtomicBool::new(true)),
                },
            );
        }
        let registration = Arc::new(ProviderRegistrationState {
            owner_id: owner_id.clone(),
            generation,
            provider,
            cancellation,
            entities: registered_entities,
        });
        let previous = self
            .inner
            .registrations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(owner_id.clone(), registration);
        drop(_gate);
        if let Some(previous) = previous {
            previous.cancel();
        }

        Ok(MediaPlayerRegistration {
            lease: Arc::new(RegistrationLease {
                dispatcher: Arc::downgrade(&self.inner),
                owner_id,
                generation,
                cancellation: cancellation_receiver,
            }),
        })
    }

    /// Unload a provider owner and release only the entities registered by its
    /// current generation.
    pub fn unregister_provider(&self, owner_id: &str) -> MediaPlayerResult<usize> {
        self.inner.unregister(owner_id, None)
    }

    /// Ask a provider for its current cached snapshot and publish it through
    /// the current weak-backed entity handle. A disabled or released entity is
    /// skipped; an unload during the future cancels the operation.
    pub async fn refresh_snapshot(
        &self,
        owner_id: &str,
        provider_key: &str,
    ) -> MediaPlayerResult<Option<StateChange>> {
        self.refresh_snapshot_with_context(owner_id, provider_key, Context::default())
            .await
    }

    /// Publish a cached provider snapshot with its original causal context.
    pub async fn refresh_snapshot_with_context(
        &self,
        owner_id: &str,
        provider_key: &str,
        context: Context,
    ) -> MediaPlayerResult<Option<StateChange>> {
        let registration = self.registration(owner_id)?;
        let entity = registration
            .entities
            .get(provider_key)
            .cloned()
            .ok_or(MediaPlayerError::EntityNotFound)?;
        let Some(entity_id) = entity.handle.current_entity_id() else {
            return Ok(None);
        };
        let context = registration.context(provider_key, entity_id, context);
        let mut cancellation = context.cancellation.clone();
        let snapshot =
            run_cancellable(&mut cancellation, registration.provider.snapshot(&context)).await?;
        if !self.is_current(&registration) {
            return Err(MediaPlayerError::Cancelled);
        }
        if entity.handle.current_entity_id().is_none() {
            return Ok(None);
        }
        entity
            .features
            .store(snapshot.supported_features.bits(), Ordering::Release);
        entity
            .available
            .store(snapshot.available, Ordering::Release);
        entity
            .handle
            .publish(snapshot.to_entity_snapshot())
            .map_err(Into::into)
    }

    /// Validate and dispatch a generic `media_player` service to already
    /// resolved entity IDs. Unavailable entities are skipped and an empty
    /// target set succeeds without provider callbacks.
    pub async fn dispatch_service(
        &self,
        service: &str,
        data: &Map<String, Value>,
        target_entity_ids: &[String],
    ) -> MediaPlayerResult<MediaPlayerServiceOutcome> {
        self.dispatch_service_with_context(service, data, target_entity_ids, Context::default())
            .await
    }

    /// Context-preserving variant for host adapters handling a generic service
    /// call. The caller context is passed to each provider callback unchanged.
    pub async fn dispatch_service_with_context(
        &self,
        service: &str,
        data: &Map<String, Value>,
        target_entity_ids: &[String],
        context: Context,
    ) -> MediaPlayerResult<MediaPlayerServiceOutcome> {
        let command = MediaCommand::parse_service(service, data)?;
        let (candidates, skipped_unavailable) = self.candidates(target_entity_ids);
        for candidate in &candidates {
            let features =
                MediaPlayerFeatures::from_bits(candidate.entity.features.load(Ordering::Acquire));
            if !command.is_supported_by(features) {
                return Err(MediaPlayerError::NotSupported(format!(
                    "Service {} is not supported by this player",
                    command.service_name()
                )));
            }
        }

        let mut outcome = MediaPlayerServiceOutcome {
            skipped_unavailable,
            ..MediaPlayerServiceOutcome::default()
        };
        for candidate in candidates {
            let Some(entity_id) = candidate.entity.handle.current_entity_id() else {
                outcome.skipped_unavailable += 1;
                continue;
            };
            let context =
                candidate
                    .registration
                    .context(&candidate.provider_key, entity_id, context.clone());
            let mut cancellation = context.cancellation.clone();
            match &command.kind {
                MediaCommandKind::BrowseMedia(request) => {
                    let response = run_cancellable(
                        &mut cancellation,
                        candidate
                            .registration
                            .provider
                            .browse(&context, request.clone()),
                    )
                    .await
                    .map_err(map_browse_error)?;
                    self.ensure_current(&candidate.registration, &candidate.entity)?;
                    let current_id = candidate
                        .entity
                        .handle
                        .current_entity_id()
                        .unwrap_or(context.entity_id.clone());
                    outcome.responses.push(MediaPlayerEntityResponse::Browse {
                        entity_id: current_id.clone(),
                        result: response,
                    });
                    outcome.dispatched_entity_ids.push(current_id);
                }
                MediaCommandKind::SearchMedia(request) => {
                    let response = run_cancellable(
                        &mut cancellation,
                        candidate
                            .registration
                            .provider
                            .search(&context, request.clone()),
                    )
                    .await
                    .map_err(map_search_error)?;
                    self.ensure_current(&candidate.registration, &candidate.entity)?;
                    let current_id = candidate
                        .entity
                        .handle
                        .current_entity_id()
                        .unwrap_or(context.entity_id.clone());
                    outcome.responses.push(MediaPlayerEntityResponse::Search {
                        entity_id: current_id.clone(),
                        result: response,
                    });
                    outcome.dispatched_entity_ids.push(current_id);
                }
                _ => {
                    let provider_outcome = run_cancellable(
                        &mut cancellation,
                        candidate
                            .registration
                            .provider
                            .execute(&context, command.clone()),
                    )
                    .await?;
                    self.ensure_current(&candidate.registration, &candidate.entity)?;
                    let current_id = candidate
                        .entity
                        .handle
                        .current_entity_id()
                        .unwrap_or(context.entity_id.clone());
                    match provider_outcome {
                        ProviderCommandOutcome::Executed => {
                            outcome.dispatched_entity_ids.push(current_id);
                        }
                        ProviderCommandOutcome::NoOp => {
                            outcome.no_op_entity_ids.push(current_id);
                        }
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Execute the direct browse command for one active entity. Unlike the
    /// generic browse service, this WS-style entrypoint enforces
    /// `BROWSE_MEDIA` and requires content type and ID together.
    pub async fn browse_media(
        &self,
        entity_id: &str,
        data: &Map<String, Value>,
    ) -> MediaPlayerResult<BrowseMedia> {
        self.browse_media_with_context(entity_id, data, Context::default())
            .await
    }

    /// Context-preserving direct browse variant for a host API adapter.
    pub async fn browse_media_with_context(
        &self,
        entity_id: &str,
        data: &Map<String, Value>,
        call_context: Context,
    ) -> MediaPlayerResult<BrowseMedia> {
        let request = BrowseMediaRequest::parse_command(data)?;
        let candidate = self
            .candidate_for_entity(entity_id)
            .ok_or(MediaPlayerError::EntityNotFound)?;
        let features =
            MediaPlayerFeatures::from_bits(candidate.entity.features.load(Ordering::Acquire));
        if !features.contains_all(MediaPlayerFeatures::BROWSE_MEDIA) {
            return Err(MediaPlayerError::NotSupported(
                "Player does not support browsing media".into(),
            ));
        }
        let context = candidate.registration.context(
            &candidate.provider_key,
            entity_id.to_owned(),
            call_context,
        );
        let mut cancellation = context.cancellation.clone();
        let response = run_cancellable(
            &mut cancellation,
            candidate.registration.provider.browse(&context, request),
        )
        .await
        .map_err(map_browse_error)?;
        self.ensure_current(&candidate.registration, &candidate.entity)?;
        Ok(response)
    }

    /// Execute the direct search command for one active entity. Direct
    /// requests require content type and ID together; service schemas permit
    /// those two fields independently.
    pub async fn search_media(
        &self,
        entity_id: &str,
        data: &Map<String, Value>,
    ) -> MediaPlayerResult<SearchMedia> {
        self.search_media_with_context(entity_id, data, Context::default())
            .await
    }

    /// Context-preserving direct search variant for a host API adapter.
    pub async fn search_media_with_context(
        &self,
        entity_id: &str,
        data: &Map<String, Value>,
        call_context: Context,
    ) -> MediaPlayerResult<SearchMedia> {
        let request = SearchMediaRequest::parse_command(data)?;
        let candidate = self
            .candidate_for_entity(entity_id)
            .ok_or(MediaPlayerError::EntityNotFound)?;
        let features =
            MediaPlayerFeatures::from_bits(candidate.entity.features.load(Ordering::Acquire));
        if !features.contains_all(MediaPlayerFeatures::SEARCH_MEDIA) {
            return Err(MediaPlayerError::NotSupported(
                "Player does not support searching media".into(),
            ));
        }
        let context = candidate.registration.context(
            &candidate.provider_key,
            entity_id.to_owned(),
            call_context,
        );
        let mut cancellation = context.cancellation.clone();
        let response = run_cancellable(
            &mut cancellation,
            candidate.registration.provider.search(&context, request),
        )
        .await
        .map_err(map_search_error)?;
        self.ensure_current(&candidate.registration, &candidate.entity)?;
        Ok(response)
    }

    fn registration(&self, owner_id: &str) -> MediaPlayerResult<Arc<ProviderRegistrationState>> {
        self.inner
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner_id)
            .cloned()
            .ok_or(MediaPlayerError::EntityNotFound)
    }

    fn is_current(&self, registration: &ProviderRegistrationState) -> bool {
        self.inner
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&registration.owner_id)
            .is_some_and(|current| current.generation == registration.generation)
    }

    fn ensure_current(
        &self,
        registration: &ProviderRegistrationState,
        entity: &RegisteredEntity,
    ) -> MediaPlayerResult<()> {
        if !self.is_current(registration) || entity.handle.current_entity_id().is_none() {
            return Err(MediaPlayerError::Cancelled);
        }
        Ok(())
    }

    fn candidates(&self, target_entity_ids: &[String]) -> (Vec<DispatchCandidate>, usize) {
        if target_entity_ids.is_empty() {
            return (Vec::new(), 0);
        }
        let targets = target_entity_ids.iter().cloned().collect::<BTreeSet<_>>();
        let registrations = self
            .inner
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut candidates = Vec::new();
        let mut skipped_unavailable = 0;
        for registration in registrations {
            for (provider_key, entity) in &registration.entities {
                match entity.handle.status() {
                    EntityStatus::Active { entity_id } if targets.contains(&entity_id) => {
                        if *registration.cancellation.borrow()
                            || !entity.available.load(Ordering::Acquire)
                        {
                            skipped_unavailable += 1;
                        } else {
                            candidates.push(DispatchCandidate {
                                registration: registration.clone(),
                                provider_key: provider_key.clone(),
                                entity: entity.clone(),
                            });
                        }
                    }
                    EntityStatus::Disabled { entity_id } | EntityStatus::Removed { entity_id }
                        if targets.contains(&entity_id) =>
                    {
                        skipped_unavailable += 1;
                    }
                    EntityStatus::Released { entity_id } | EntityStatus::Shutdown { entity_id }
                        if targets.contains(&entity_id) =>
                    {
                        skipped_unavailable += 1;
                    }
                    _ => {}
                }
            }
        }
        (candidates, skipped_unavailable)
    }

    fn candidate_for_entity(&self, target_entity_id: &str) -> Option<DispatchCandidate> {
        let registrations = self
            .inner
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for registration in registrations {
            if *registration.cancellation.borrow() {
                continue;
            }
            for (provider_key, entity) in &registration.entities {
                if entity.handle.current_entity_id().as_deref() == Some(target_entity_id) {
                    return Some(DispatchCandidate {
                        registration: registration.clone(),
                        provider_key: provider_key.clone(),
                        entity: entity.clone(),
                    });
                }
            }
        }
        None
    }
}

struct DispatchCandidate {
    registration: Arc<ProviderRegistrationState>,
    provider_key: String,
    entity: RegisteredEntity,
}

impl DispatcherInner {
    fn unregister(
        &self,
        owner_id: &str,
        expected_generation: Option<u64>,
    ) -> MediaPlayerResult<usize> {
        let gate = self
            .registration_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registration = {
            let mut registrations = self
                .registrations
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(existing) = registrations.get(owner_id) else {
                return Ok(0);
            };
            if expected_generation.is_some_and(|generation| generation != existing.generation) {
                return Ok(0);
            }
            registrations.remove(owner_id)
        };
        drop(gate);
        let Some(registration) = registration else {
            return Ok(0);
        };

        // Signal cancellation and release lifecycle handles only after the
        // registration-map lock is gone. Active callback clones see the same
        // generation and cannot publish through a released handle.
        registration.cancel();
        let mut first_error = None;
        for entity in registration.entities.values() {
            if let Err(error) = entity.handle.release()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(MediaPlayerError::Lifecycle(error));
        }
        Ok(registration.entities.len())
    }
}

/// RAII lease for exactly one provider registration generation.
#[derive(Clone)]
pub struct MediaPlayerRegistration {
    lease: Arc<RegistrationLease>,
}

impl fmt::Debug for MediaPlayerRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaPlayerRegistration")
            .field("owner_id", &self.lease.owner_id)
            .field("generation", &self.lease.generation)
            .finish()
    }
}

struct RegistrationLease {
    dispatcher: Weak<DispatcherInner>,
    owner_id: String,
    generation: u64,
    cancellation: watch::Receiver<bool>,
}

impl Drop for RegistrationLease {
    fn drop(&mut self) {
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            let _ = dispatcher.unregister(&self.owner_id, Some(self.generation));
        }
    }
}

impl MediaPlayerRegistration {
    #[must_use]
    pub fn owner_id(&self) -> &str {
        &self.lease.owner_id
    }

    /// Whether this registration was unloaded, replaced, or its dispatcher
    /// was dropped.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation().is_cancelled()
    }

    /// Wait for this exact registration generation to unload or be replaced.
    /// Providers can select their cache-refresh task against this future.
    pub async fn cancelled(&self) {
        self.cancellation().cancelled().await;
    }

    /// Obtain a cancellation signal that can be moved into provider-owned
    /// refresh tasks without keeping this registration lease alive.
    #[must_use]
    pub fn cancellation(&self) -> MediaPlayerCancellation {
        MediaPlayerCancellation {
            receiver: self.lease.cancellation.clone(),
        }
    }

    /// Return an entity's current ID through its lifecycle handle.
    #[must_use]
    pub fn entity_id(&self, provider_key: &str) -> Option<String> {
        let dispatcher = self.lease.dispatcher.upgrade()?;
        let registration = dispatcher
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.lease.owner_id)
            .filter(|registration| registration.generation == self.lease.generation)
            .cloned()?;
        registration
            .entities
            .get(provider_key)
            .and_then(|entity| entity.handle.current_entity_id())
    }

    /// Return the current provider feature mask for one active registration.
    #[must_use]
    pub fn supported_features(&self, provider_key: &str) -> Option<MediaPlayerFeatures> {
        let dispatcher = self.lease.dispatcher.upgrade()?;
        let registration = dispatcher
            .registrations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.lease.owner_id)
            .filter(|registration| registration.generation == self.lease.generation)
            .cloned()?;
        registration
            .entities
            .get(provider_key)
            .map(|entity| MediaPlayerFeatures::from_bits(entity.features.load(Ordering::Acquire)))
    }

    /// Explicitly release this registration. Dropping the last clone performs
    /// the same cleanup if this method is not called.
    pub fn unregister(&self) -> MediaPlayerResult<usize> {
        self.lease
            .dispatcher
            .upgrade()
            .map(|dispatcher| {
                dispatcher.unregister(&self.lease.owner_id, Some(self.lease.generation))
            })
            .unwrap_or(Ok(0))
    }
}

/// One browse/search response associated with the current HA entity ID.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaPlayerEntityResponse {
    Browse {
        entity_id: String,
        result: BrowseMedia,
    },
    Search {
        entity_id: String,
        result: SearchMedia,
    },
}

impl MediaPlayerEntityResponse {
    #[must_use]
    pub fn entity_id(&self) -> &str {
        match self {
            Self::Browse { entity_id, .. } | Self::Search { entity_id, .. } => entity_id,
        }
    }

    /// Serialize the provider payload using the pinned service result shape.
    #[must_use]
    pub fn to_service_response(&self) -> Value {
        match self {
            Self::Browse { result, .. } => result.to_service_response(),
            Self::Search { result, .. } => result.to_service_response(),
        }
    }
}

/// Result of a service dispatch after per-target feature and availability
/// checks. Browse/search response-only values remain typed and per entity so
/// the host transport can apply its response envelope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaPlayerServiceOutcome {
    pub dispatched_entity_ids: Vec<String>,
    pub no_op_entity_ids: Vec<String>,
    pub skipped_unavailable: usize,
    pub responses: Vec<MediaPlayerEntityResponse>,
}

fn map_browse_error(error: MediaPlayerError) -> MediaPlayerError {
    match error {
        MediaPlayerError::NotImplemented(_) => {
            MediaPlayerError::NotSupported("Integration does not support browsing media".into())
        }
        error => error,
    }
}

fn map_search_error(error: MediaPlayerError) -> MediaPlayerError {
    match error {
        MediaPlayerError::NotImplemented(_) => {
            MediaPlayerError::NotSupported("Integration does not support searching media".into())
        }
        error => error,
    }
}

async fn run_cancellable<T, F>(
    cancellation: &mut watch::Receiver<bool>,
    future: F,
) -> MediaPlayerResult<T>
where
    F: std::future::Future<Output = MediaPlayerResult<T>>,
{
    if *cancellation.borrow() {
        return Err(MediaPlayerError::Cancelled);
    }
    tokio::select! {
        biased;
        result = future => result,
        () = wait_for_cancellation(cancellation) => Err(MediaPlayerError::Cancelled),
    }
}

async fn wait_for_cancellation(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
