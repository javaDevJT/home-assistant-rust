# `hass-media-player`

<!-- Ownership: media_player_sdk_v134 owns this README and src/** plus tests/** only. -->

Reusable native media-player contracts for providers and async runtime adapters.

This crate owns the pinned state vocabulary, feature bits, service schemas,
feature requirements, browse/search payload types, and an async dispatcher that
uses `hass-entity` leases. Providers supply truthful state and feature masks;
the common layer does not infer provider capabilities or device policy.

The public provider entrypoints are `MediaPlayerProvider::{snapshot,
execute,browse,search}`. `MediaPlayerDispatcher::register_provider` returns a
generation-scoped `MediaPlayerRegistration`; retain it for the provider
lifetime and move its cloneable `cancellation()` token into provider-owned
refresh tasks. `refresh_snapshot` obtains cached state, updates the live feature mask,
and publishes through the lifecycle handle. `dispatch_service` takes
already-resolved entity IDs, validates service data and AND/OR feature gates,
skips unavailable entities, then calls the provider without holding registry
locks. Context-preserving dispatch variants carry the host `Context` through to
provider callbacks. Direct `browse_media` and `search_media` entrypoints
enforce the direct command feature and pair rules and return typed
`BrowseMedia`/`SearchMedia` responses.

Important compatibility details are explicit in the API: service schemas allow
`media_content_type` and `media_content_id` independently while direct
browse/search commands require both or neither; `browse_media` service calls
have no runtime feature gate in the pinned registration, while direct browse
requires `BROWSE_MEDIA`; `search_media` requires `SEARCH_MEDIA`; play-media
enqueue/announce do not add runtime feature gates; and providers can return
`ProviderCommandOutcome::NoOp` for provider-specific silent no-op behavior.

`MediaPlayerSnapshot::to_entity_snapshot` omits `None` values, omits regular
media properties while off, and emits source/sound-mode lists only when their
feature is set and their list is non-empty. Position timestamps are provider
supplied and retain fractional precision. The entity lifecycle records live
position, position timestamp, picture, and capability lists without storing
those attributes in state history. Providers supply `entity_picture` and
`entity_picture_local` independently; a remote picture does not invent a local
proxy URL. Both keys are omitted for an off player and excluded from state
history. Grouping-capable players publish `group_members: null` when no group
list was supplied, including while off. Unavailable snapshots prevent service
commands until a subsequent available snapshot refreshes the registration.

`MediaPlayerDispatcher` is intentionally async-only. It does not block a Tokio
worker or install a sync adapter; the host's async service registration layer
can call `dispatch_service`, `browse_media`, and `search_media` directly.

The compatibility reference is Home Assistant Core source commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, read as source text. This crate
does not import or execute upstream Python code.

Tests use injected in-memory providers and the native in-memory Core store.
They do not contact services, accounts, playback devices, or Spotify.
