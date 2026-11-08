//! Bounded native Spotify Web API client and wire models.

mod authorization;
mod client;
mod error;
mod models;

pub use authorization::{AuthorizationError, OAuthSessionAuthorizer, RequestAuthorizer};
pub use client::{
    DEFAULT_API_BASE_URL, PlayMediaTarget, RepeatMode, SpotifyClient, SpotifyClientOptions,
    StartPlaybackRequest,
};
pub use error::{ForbiddenReason, RateLimitReason, SpotifyError};
pub use models::{
    AlbumTrack, Artist, Device, Image, InlinePage, PlaybackContext, PlaybackState, PlayedTrack,
    Playlist, PlaylistItem, Profile, SavedAlbum, SavedShow, SavedTrack, Show, SpotifyObject,
    UserProfile,
};
