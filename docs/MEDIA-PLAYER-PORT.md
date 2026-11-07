# Native `media_player` port contract

**Status:** source contract only; no native media-player parity is claimed.
Pinned source is Home Assistant Core
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
First consumer is Spotify, whose entity and endpoint mappings are in
[`SPOTIFY-PORT.md`](SPOTIFY-PORT.md#entity-projection-and-command-contract).

## Entity contract

Core supplies the `media_player` entity/state vocabulary, attribute projection,
feature bits, common services, target checking, and API surfaces. A provider
supplies stable entity identity, availability/current state, truthful feature
mask, and the provider operations it advertises.

States: `off`, `on`, `idle`, `playing`, `paused`, `buffering`; availability is
separate. Standard optional state attributes include volume/mute, media
content ID/type/duration/position/position timestamp, title/artist/album/track,
series/season/episode/channel/playlist, app ID/name, source, sound mode,
shuffle, repeat, and current media image (`entity_picture`). Null attributes are omitted. Position and duration are
seconds; volume is a float from 0 to 1; repeat is `off`, `all`, or `one`. When
off, regular media attributes are omitted. `source_list` and `sound_mode_list`
are capability attributes emitted only when that feature is set and the list is
non-empty. The pinned property-to-attribute map and projection are in
[`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/entity.py#L46-L70)
and [the entity base](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/entity.py#L139-L188).

`supported_features` is an unsigned bitmask, defaults to zero, and must retain
these pinned values:

| Feature | Bit | Feature | Bit |
| --- | ---: | --- | ---: |
| `PAUSE` | 1 | `SEEK` | 2 |
| `VOLUME_SET` | 4 | `VOLUME_MUTE` | 8 |
| `PREVIOUS_TRACK` | 16 | `NEXT_TRACK` | 32 |
| `TURN_ON` | 128 | `TURN_OFF` | 256 |
| `PLAY_MEDIA` | 512 | `VOLUME_STEP` | 1024 |
| `SELECT_SOURCE` | 2048 | `STOP` | 4096 |
| `CLEAR_PLAYLIST` | 8192 | `PLAY` | 16384 |
| `SHUFFLE_SET` | 32768 | `SELECT_SOUND_MODE` | 65536 |
| `BROWSE_MEDIA` | 131072 | `REPEAT_SET` | 262144 |
| `GROUPING` | 524288 | `MEDIA_ANNOUNCE` | 1048576 |
| `MEDIA_ENQUEUE` | 2097152 | `SEARCH_MEDIA` | 4194304 |

Values and state/repeat enums: [pinned `const.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/const.py#L74-L219).

## Service contract

The common service catalog and feature requirements are:

| Services | Feature requirement |
| --- | --- |
| `turn_on`, `turn_off`, `toggle` | `TURN_ON`; `TURN_OFF`; both for `toggle` |
| `volume_up`, `volume_down`, `volume_set`, `volume_mute` | `VOLUME_SET` or `VOLUME_STEP`; `VOLUME_SET`; `VOLUME_MUTE` |
| `media_play_pause`, `media_play`, `media_pause`, `media_stop` | both `PLAY` and `PAUSE`; `PLAY`; `PAUSE`; `STOP` |
| `media_next_track`, `media_previous_track`, `media_seek` | `NEXT_TRACK`; `PREVIOUS_TRACK`; `SEEK` |
| `play_media` | `PLAY_MEDIA` |
| `select_source`, `select_sound_mode`, `clear_playlist` | `SELECT_SOURCE`; `SELECT_SOUND_MODE`; `CLEAR_PLAYLIST` |
| `shuffle_set`, `repeat_set`, `join`, `unjoin` | `SHUFFLE_SET`; `REPEAT_SET`; `GROUPING` |
| `browse_media`, `search_media` | target metadata says `BROWSE_MEDIA`, `SEARCH_MEDIA`; runtime caveat below |

For feature checks, masks in a single requirement are AND; multiple
requirements are OR. Thus `media_play_pause` requires both bits, while
`volume_up/down` accept either absolute or step volume. This is enforced by the
upstream entity-service resolver, not only the UI. Service definitions and
registration: [`services.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/services.yaml#L1-L293)
and [`services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/services.py#L91-L300).

Spotify's feature set is browse, next, pause, play, play-media, previous,
repeat, seek, select-source, shuffle, and volume. The pinned integration
advertises zero for a non-premium profile; otherwise `SELECT_SOURCE` only when
there is no playback or the active device is restricted, and its full set for a
known unrestricted active device. Its profile `product` field is deprecated;
define unknown entitlement behavior before relying on it. A device can report
null volume and `supports_volume=false`; do not assume absolute-volume control.
See [Spotify's pinned gate](SPOTIFY-PORT.md#entity-projection-and-command-contract).

| Service | Validated arguments and provider operation |
| --- | --- |
| `volume_set` | Required `volume_level` float `0..=1` -> set volume. |
| `volume_up/down` | No args. Base fallback adds/subtracts `volume_step` (default `0.1`) and clamps, but only when current volume is known and `VOLUME_SET` is set. A step-only provider implements the operation itself. |
| `media_seek` | Required `seek_position` numeric seconds (`cv.positive_float`; UI minimum 0) -> seek. |
| `repeat_set` / `shuffle_set` | Required repeat `off`, `all`, or `one`; required boolean `shuffle`. |
| `select_source` | Required source string; provider resolves to its current device ID. |
| `play_media` | Required `media` with string `media_content_type` and `media_content_id`; optional `enqueue` `play|next|add|replace`, `announce` bool, and `extra` object default `{}`. `enqueue` and `announce` are exclusive. Provider receives media type/ID and kwargs. |

The play-media schema also accepts top-level content type/ID for compatibility,
but rejects mixing those with the `media` object. Legacy boolean `enqueue` maps
to `add`/`play` with a deprecation warning. YAML filters enqueue/announce by
`MEDIA_ENQUEUE`/`MEDIA_ANNOUNCE`, but runtime registration gates only
`PLAY_MEDIA`. Spotify maps seek seconds to `int(seconds * 1000)`, volume to
`int(level * 100)`, repeat `all -> context` and `one -> track`, shuffle to a
bool, and source name to cached device ID. Its unknown source is a silent
no-op; context enqueue raises; unsupported media types are ignored. Keep those
provider policies out of common validation. Full mapping: [Spotify service
table](SPOTIFY-PORT.md#entity-projection-and-command-contract).

Upstream entity services target `media_player` entities. For an explicit
existing target with a missing required feature, Core raises
`ServiceNotSupported` (`ServiceValidationError`, translation key
`service_not_supported`); unavailable entities are filtered. Non-admin service
calls use the caller's entity permissions; these
media-player services are not admin-only.

Two upstream quirks need a deliberate compatibility choice: `browse_media`
YAML advertises `BROWSE_MEDIA`, but its runtime service registration omits the
feature requirement; the direct browse WS command does check it. `search_media`
checks `SEARCH_MEDIA` on both paths. Service schemas allow content type and ID
independently; direct browse/search WS commands require both or neither.

## HTTP and WebSocket surface

| Operation | Upstream surface |
| --- | --- |
| Play media | Generic HTTP `POST /api/services/media_player/play_media`; generic WS `call_service` with domain/service, target and service data. There is no dedicated play-media command. |
| Browse | WS `type: "media_player/browse_media"`, required `entity_id`, optional type/ID pair. Provider returns a `BrowseMedia` tree. |
| Search | WS `type: "media_player/search_media"`, required `entity_id` and `search_query`, optional type/ID pair and `media_filter_classes`. Provider returns `SearchMedia`. |
| Browse/search services | Generic `media_player.browse_media` and `.search_media`, response-only services. HTTP needs `?return_response`; generic WS uses `return_response: true`. No browse-tree REST route exists. |
| Images | GET `/api/media_player_proxy/{entity_id}` and `/browse_media/{type}/{id}` serve image bytes only, not a browse tree. Missing image bytes returns 404. |

`BrowseMedia` fields are title, media class/content type/content ID,
children class, play/expand/search flags, searchable classes, thumbnail,
`not_shown`, and children. `SearchMedia` serializes as `{ "result": [...] }`;
child items omit recursive child arrays. The only request enum validation is
`media_filter_classes` against `MediaClass`; content type remains a string for
provider-defined extensions. Types and serializers:
[`browse_media.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/browse_media.py#L99-L199).

Direct browse/search WS errors: unknown entity -> `entity_not_found`,
`Entity not found`; missing feature -> `not_supported`, `Player does not
support browsing media` / `Player does not support searching media`; browse bit
set but method raises `NotImplementedError` -> `not_supported`, `Integration
does not support browsing media`; `BrowseError`/`SearchError` -> `unknown_error`
with the exception text; invalid schema -> `invalid_format`. Error classes:
[`errors.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/errors.py#L6-L15).

For generic service calls, upstream WS uses `service_validation_error` for
unsupported target/schema errors and `home_assistant_error` for provider
`HomeAssistantError`; HTTP maps validation to 400 and provider
`HomeAssistantError` to 500. Browse/search HTTP calls without `return_response`
fail 400 with `Service call requires responses but caller did not ask for
responses. Add ?return_response to query parameters.` Requesting a response
from a no-response service fails 400 with `Service does not support responses.
Remove return_response from request.`
The generic API envelopes are in [HTTP](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/api/__init__.py#L383-L476)
and [WebSocket](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/websocket_api/commands.py#L315-L399).

## Native implementation seams

* **Entity lifecycle exists.** Use `hass-entity::EntityLifecycle` and
  `EntitySpec::with_unrecorded_attributes`; `ProviderEntityInfo` carries
  `supported_features` and capabilities. Register stable provider identity,
  publish snapshots through the returned handle, and release/cancel provider
  work on unload. Do not create another entity ID cache. Register domain
  services/browse/search/image handlers once; entries own entity handles and
  provider tasks. See [`hass-entity` contract](../crates/hass-entity/README.md)
  and [upstream setup/unload](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/media_player/__init__.py#L245-L274).
* **Services exist but are lower-level.** `Runtime::register_entity_service`
  is owner-scoped and dispatches synchronously by entity matcher; it merges
  changed states and optional responses. Its JSON `description` is metadata,
  not a schema validator or feature gate. Add typed media validation and required
  feature checks above this seam. Current service handler and registration:
  [`hass-core`](../crates/hass-core/src/lib.rs#L583),
  [service registration](../crates/hass-core/src/lib.rs#L1395).
* **Targets exist.** `hass-registry::targets::resolve_entity_service_targets`
  resolves entity/device/area/floor/label targets. The API installs it when
  registries are enabled. The legacy flat-selector allowlist does not yet
  include `media_player`; extend it for flat device/area/etc. selectors. Use
  `target` maps as the canonical WS contract; reject `entity_id` in both target
  and data (current error: `service entity_id must be supplied in target or data, not both`). A selector with no matches skips provider dispatch and returns an empty success. See [`targets.rs`](../crates/hass-registry/src/targets.rs#L32)
  and [domain allowlist](../crates/hass-registry/src/targets.rs#L119).
* **Transport gaps remain.** `hass-api` has generic HTTP and WS service calls,
  but no media browse/search command or image proxy. WS `call_service` requires
  `Identity.can_control` (admin/owner or `system-users`); HTTP builds caller
  context but has no matching explicit `can_control` check, and Runtime target
  resolution does not enforce per-entity permissions. Apply one control and
  entity-target policy to both transports. `ServiceCall` also has no
  `return_response` request flag: HTTP accepts `?return_response`, while WS
  always returns a response field. Add response-capability metadata for
  response-only browse/search services. Host unknown services currently map to
  WS `service_not_found` / `Service not found` and HTTP 404 / `Service not found.`;
  no media-specific error codes exist. Host adapters: [HTTP](../crates/hass-api/src/http.rs#L335),
  [WS auth](../crates/hass-api/src/extensions.rs#L220),
  [WS service call](../crates/hass-api/src/websocket.rs#L1970).
* **Async choice remains open.** Runtime handlers are synchronous and adapters
  run them on blocking workers. Spotify control/browse calls need network I/O;
  add an async entity-service path or use an explicitly blocking client with
  bounded timeouts. Provider owns refresh/task cancellation; domain owns
  service/API registration.

## Synthetic falsifiers

Run only with fixtures and a fake loopback HTTP server; no credentials, live
Spotify requests, playback, or physical devices.

1. Project fake track/episode/device payloads. Assert state/attribute types,
   null omission, exact feature bits and non-premium/no-playback/restricted gates.
2. Validate Spotify services via both generic APIs: volume scale, seek seconds,
   repeat/shuffle mapping, source lookup, play URI/context, enqueue validation,
   service schema failures, feature-gate AND/OR, and authorized target expansion.
3. Assert browse/search payloads, result serialization, return-response
   requirements, unknown/missing-feature/provider-error responses, and that the
   image route returns bytes/404 rather than a browse tree.
4. Register two provider owners, target each and both, unload one, then verify
   the other still dispatches and stale work cannot publish after release.

Retain the exact provider fixture names already called out in
[`SPOTIFY-PORT.md`](SPOTIFY-PORT.md#synthetic-loopback-fixtures-and-falsifiers):
`current_user.json`, `playback.json`, `playback_episode.json`, `devices.json`,
and `playlist.json`. The source browse/service checks are
[`test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/media_player/test_init.py#L396-L686)
and [`test_browse_media.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/media_player/test_browse_media.py#L28-L123).
