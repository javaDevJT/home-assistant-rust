# Spotify provider port contract

This is the source/protocol contract for porting the pinned Spotify integration
to the native Rust host. It is an audit and implementation guide, not a claim
that Spotify has been ported. The generic OAuth client/flow is a separate work
stream. An OAuth code exchange alone covers only the first step below.

## Acceptance boundary

Count the provider as implemented only when a local synthetic flow proves all
of these paths together: Spotify authorization and token exchange; post-authorize
profile lookup and account binding; token refresh and reauth; initial profile,
playback, and device reads; coordinator polling/cache/error behavior; entity
state and feature projection; every exposed media-player command; and setup,
unload, cancellation, and refresh-task ownership. A passing generic OAuth suite
does not cover any of the Spotify-specific steps after token resolution.

## Pinned integration contract

The source is Home Assistant Core commit
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
The worktree base is `6db5a5860dac29541d3bff2f3c2205e3a342f96a`.

### App credentials, OAuth and account binding

- The manifest is a cloud-polling `service` integration, depends on
  `application_credentials`, and pins `spotifyaio==2.1.0`. Its application
  credentials adapter declares `https://accounts.spotify.com/authorize` and
  `https://accounts.spotify.com/api/token`.
- The pinned handler uses the long-running Authorization Code flow. Spotify's
  server-side flow uses the app's Client ID and Client Secret; keep the secret
  in the private application-credentials handle. Spotify recommends PKCE when
  a client cannot safely keep a secret. Authorization must use a registered
  redirect URI with exact matching. HTTPS is required except for explicit
  `127.0.0.1` or `[::1]` loopback literals; `localhost` is not allowed. The
  flow state must be verified on callback.
- The requested scopes, in source order, are:

  | Scope | Pinned use |
  | --- | --- |
  | `user-modify-playback-state` | playback commands |
  | `user-read-playback-state` | current playback and Connect devices |
  | `user-read-private` | profile and the legacy subscription check |
  | `playlist-read-private`, `playlist-read-collaborative` | playlist browsing |
  | `user-library-read` | saved albums, tracks and shows |
  | `user-top-read` | top artists and tracks |
  | `user-read-playback-position` | requested by the pinned flow; no direct call is apparent in the focused integration/client path |
  | `user-read-recently-played` | recently played items |
  | `user-follow-read` | followed artists |

- **Pinned mismatch to correct in the native provider:** the handler joins the
  scope list with commas, and the pinned flow test asserts that comma-delimited
  URL. Spotify's current OAuth docs require a *space-separated* scope value.
  Send the individual scopes as one space-delimited value, encode it once, and
  add a falsifier that rejects the pinned comma form. Runtime setup also splits
  the granted scope on spaces and requires all ten scope names, so the comma
  form cannot satisfy its own later validation. Do not copy the source test's
  expected URL as protocol truth.
- The handler forces `show_dialog=true`. This asks Spotify to show consent again
  for an already-authorized user. Preserve it only if that re-consent behavior
  is an intentional product choice; it is optional in Spotify's protocol.
- The confidential-client code exchange is a form-encoded `POST /api/token`
  with `grant_type=authorization_code`, the one-time `code`, and the exact
  `redirect_uri`, authenticated with HTTP Basic over `client_id:client_secret`.
  The refresh request is form-encoded with `grant_type=refresh_token` and the
  stored refresh token; for this server-side flow it also uses HTTP Basic.
  Responses include `access_token`, `token_type`, `expires_in`, `scope`, and
  initially `refresh_token`. A PKCE client instead sends its `client_id` and
  `code_verifier` in the form without a secret.
- After code exchange, the provider calls `GET /v1/me`. It titles the entry with
  `display_name`, falling back to the Spotify user ID, then sets that user ID as
  the config-entry unique ID. A duplicate account aborts. Reauth must match the
  existing ID or aborts with `reauth_account_mismatch`; reconfigure also refuses
  a different account. Initial creation and reconfigure store `name`; the
  entity's device ID is account-scoped.
- **Current identity difference:** Spotify now documents `account_id` as the
  immutable account-linking identifier and explicitly says not to use `id` for
  account linking. The pinned client/model and tests use `id` (`user_id`). Use
  `account_id` for new links if it is present, but define a migration that keeps
  existing ID-bound entries, reauth matching, and device/entity identity stable
  rather than silently creating duplicates. The pinned `spotifyaio` DTO has no
  `account_id` field and models `product` as optional, so a native profile model
  needs to add the current field rather than deserializing through that shape.
  Spotify's live profile reference still labels `product` deprecated, while its
  February 2026 Web API changelog lists it as removed. Treat `product` as
  optional and non-authoritative; verify actual entitlement behavior for the
  chosen app mode instead of making setup depend on that field.
- The pinned config flow maps any profile `403` to `user_not_premium`. The test
  fixture text also says a user may simply not be registered in the developer
  dashboard. Current Spotify development mode has an authorized-user cap and
  can restrict endpoints, so retain the HTTP error context and distinguish
  app-user access, missing scope, entitlement, and actual account state where
  Spotify gives enough information. Do not infer “free account” from every
  `403`.
- Spotify's February 2026 developer-access update says Development Mode requires
  the developer to have Premium, set a five-authorized-user cap per Client ID,
  and limit new integrations to a smaller supported endpoint set. It initially
  allowed one Development Mode Client ID per developer; the July 2026 changelog
  raised that to 25 and says development-mode quota is counted per developer
  account. No later source reviewed here changes the five-user cap. Spotify
  postponed endpoint-access changes for existing integrations in March, while
  keeping the other limits. Development Mode is described as a non-commercial
  personal project/testing mode. The February supported-endpoint list includes
  the profile, player, device, control, and browse APIs used by this component;
  verify the selected app mode and dashboard allowlist before treating that as
  a deployment guarantee. Do not infer app-owner access policy from an end
  user's profile.
- There is no Spotify options flow or use of entry `options` in the pinned
  component. Entry `data` contains `auth_implementation`, `token`, and `name`;
  the token map includes provider token fields plus the host's expiry field.
  Reauth updates the same entry. Preserve unrelated existing data/options and
  unknown token fields in the native store even though the old handler does not
  explicitly merge them in its reauth branch.

Spotify currently documents one-hour access tokens and six-month refresh-token
lifetimes measured from original authorization; refreshing does not extend that
six-month lifetime. Refresh `invalid_grant` is terminal: start authorization
again, do not retry the same refresh token. Refresh responses may omit a new
refresh token, in which case retain the old one. The existing OAuth port
contract uses a 20-second expiry margin, serial refresh per session, and atomic
token-map updates that preserve unknown fields.

### Setup and data requests

The config-flow post-authorization step and runtime setup both fetch the user
profile. Runtime setup then ensures a valid token, creates one client, installs
a callback that re-validates/refreshes the stored token before each API request,
fetches initial playback, fetches initial devices, stores runtime data, checks
for all requested scopes, then forwards the media-player platform. These first
refreshes are sequential and required for successful entry setup.

The upstream client receives Home Assistant's shared HTTP session, so it does
not own or close that session. Unload only unloads the media-player platform;
entity removal unsubscribes the device-list listener. The native driver must
cancel its owned timers/requests and close any per-entry-owned client on unload,
without closing a host-shared HTTP client.

| Purpose | Request | Contract |
| --- | --- | --- |
| identity | `GET https://api.spotify.com/v1/me` | 200 profile; `user-read-private`; fields used by the old code: `id`, `display_name`, optional `product` |
| playback | `GET /v1/me/player?additional_types=track,episode` | 200 state or 204 no active state; `user-read-playback-state` |
| devices | `GET /v1/me/player/devices` | 200 `{ "devices": [...] }`, including an empty list; `user-read-playback-state` |
| playlist context enrichment | `GET /v1/playlists/{id}` | called only when the playback context changes to a playlist; metadata failure is non-fatal |

For playback, preserve nullable `context`, `progress_ms`, `item`, and device
`id`/`volume_percent`; preserve `is_playing`, `currently_playing_type`,
`repeat_state`, and `shuffle_state`. The client requests `additional_types` for
tracks and episodes, but Spotify says that parameter may be deprecated and
clients must handle future item types. Do not make an unknown item type fail
the whole state refresh.

Devices can be omitted by Spotify when a model is unsupported. Their IDs may be
null and are not guaranteed to remain stable. A restricted device accepts no
Web API commands. Volume is nullable and in `0..=100`; honor `supports_volume`.
The integration sorts device names for a stable `source_list`, while source
selection resolves a name back to the currently cached device ID.

The pinned browser also exposes playlists, followed artists, saved albums,
tracks and shows, recently played, top artists/tracks, playlist contents,
album tracks, artist albums, and show episodes. Preserve those read paths if
preserving current browse behavior; the scope list is not playback-only. The
verified `spotifyaio` 2.1.0 client already uses the current
`GET /v1/playlists/{id}/items` endpoint and maps the new `items[].item` response
field (retaining compatibility with `tracks`); the Rust port should keep the
current route and field shape.

### Coordinator cadence, cache and failures

- Playback uses a 30-second update interval. When playing with a known item and
  progress, if `duration_ms - progress_ms` is less than 30 seconds, the next
  interval becomes remaining time plus one second. Paused/unknown playback uses
  the normal interval.
- Devices refresh every five minutes. A successful media-player command waits
  one second and then refreshes playback immediately. It does not explicitly
  refresh the device coordinator.
- Playlist context metadata is cached by the last checked context URI. A changed
  URI clears the cached playlist; DJ context is represented by title `DJ` and
  skips playlist fetch. A playlist 404 is non-fatal and stays checked. Network,
  rate-limit, or schema errors are logged at debug level, clear the playlist,
  reset the checked URI, and let playback refresh succeed.
- The client timeout is ten seconds. It turns `TimeoutError` into
  `SpotifyConnectionError`. The component maps connection errors from profile,
  playback, and device requests to setup/update retry errors. Profile `403`
  becomes a non-fixable `user_not_premium` setup issue. Config-flow profile
  failures abort as `user_not_premium` or `connection_error`.
- The client recognizes 204, 401, 403, 404 (also `"status": 404` in the body),
  and 429. It maps these to empty response, authentication failure, forbidden,
  not found, and rate-limit exceptions respectively. Other statuses are returned
  as response text for model parsing; there is no explicit 400/5xx mapping.
  Playback's 204 maps to `None`/idle. Spotify's generic API docs define 204 as
  success with no body.
- A 401 from the Web API is raised as an authentication exception; the pinned
  client does not retry the API call after refreshing, and the Spotify
  integration has no explicit provider-level API-401-to-reauth mapping. Keep
  pre-request token refresh, but ensure a terminal 401 reaches the host's
  reauth path without turning it into an empty success or endless retry.
- The dependency does not read `Retry-After`, apply a backoff, or retry an API
  request. Its 429 exception retains response text only. Current Spotify docs
  say 429 is app-wide over a rolling 30-second window (with separate quota
  restrictions possible) and normally supplies `Retry-After` in seconds. The
  July 2026 changelog also defines a Development Mode quota-exceeded 429 body
  with `reason="QUOTA_EXCEEDED"`, counted per developer account. The native
  client should retain status, body, and header and schedule backoff. Avoid blind
  retries of command writes such as next/queue; Player API ordering is not
  guaranteed across concurrent endpoint calls.
- `CoordinatorEntity.available` is `coordinator.last_update_success`. Any failed
  playback refresh marks the media-player unavailable until a later successful
  refresh; previous data may still be cached but must not be presented as
  currently available. Device-list updates write state only when the entity is
  enabled.

### Entity projection and command contract

The integration creates one `media_player` entity per account. It has no entity
name of its own, uses the pinned profile `user_id` (`id`) as unique ID, and
registers the account-level service device (`Spotify AB`, model
`Spotify {product}` when present, configuration URL `https://open.spotify.com`).

| State/property | Mapping |
| --- | --- |
| state | no playback -> `idle`; `is_playing` -> `playing`; otherwise `paused` |
| media ID/type | item URI; episode -> podcast, track -> music |
| title/artist/album/track | item name; episode show name; track artists joined by `, `; album name and track number only for tracks |
| duration/position | milliseconds converted to rounded integer seconds; position timestamp is coordinator fetch time because Spotify's `timestamp` is unreliable |
| image | first episode image, then show image; for tracks the first album image |
| playlist/source | cached playlist name, `DJ`, or none; active device name or none |
| volume/shuffle/repeat | volume percent divided by 100 or none; shuffle passthrough; Spotify `context/off/track` -> HA `all/off/one` |
| sources | alphabetically sorted device names |

Advertised feature set is browse, next, pause, play, play-media, previous,
repeat, seek, select-source, shuffle, and volume. If the user profile says a
non-premium product, advertises zero features. Otherwise, absent playback or a
restricted active device reduces features to `select-source` only; a known
non-restricted playback device advertises the full set. The current API marks
`product` deprecated, so this UI gate needs a replacement or an explicit
“unknown entitlement” policy before that field disappears.

Standard HA `media_player` commands map to Spotify calls; they return no
Spotify JSON response and successful Spotify writes return 204:

| HA command | Spotify request and payload |
| --- | --- |
| play/resume | `PUT /v1/me/player/play`; default body `{ "position_ms": 0 }` |
| play-media | same endpoint; track/episode/music -> `{ "uris": [uri] }`; playlist/album/artist/show -> `{ "context_uri": uri }`; saved-tracks special ID -> `spotify:user:{id}:collection` |
| enqueue `ADD` | `POST /v1/me/player/queue?uri={uri}`; optional `device_id`; only track/episode/music, other types raise `ValueError` |
| pause | `PUT /v1/me/player/pause` |
| next / previous | `POST /v1/me/player/next` / `/previous` |
| seek | `PUT /v1/me/player/seek?position_ms={int(seconds*1000)}` |
| volume | `PUT /v1/me/player/volume?volume_percent={int(level*100)}` |
| repeat | `PUT /v1/me/player/repeat` with `state=off`, `state=context`, or `state=track` |
| shuffle | `PUT /v1/me/player/shuffle` with `state=true` or `state=false` |
| select source | `PUT /v1/me/player` with `{ "device_ids": [id] }` |

Writes require `user-modify-playback-state`; Spotify currently documents player
control as Premium-only. In the pinned handler, a source name that is not in
the cached device list is a silent no-op. `play-media` strips URL query and
fragment, targets the first cached device only when no playback is active, and
silently logs/ignores unsupported media types. Queueing a context item raises.
Keep these edge cases explicit in service tests. There are no custom Spotify
service handlers separate from standard media-player commands and browse.

## Native OAuth seam and Spotify-specific work still required

### Native account and entitlement decisions

New links prefer Spotify's nonempty `account_id`, falling back to legacy `id`
when the response has no account-linking identifier. Persist both supplied
identifiers and the chosen identity kind. Restored entries retain their existing
unique ID; reauthentication must match that entry through its stored identifier,
and must not silently migrate identity or replace a different account. Duplicate
checks compare both known identifiers across existing Spotify entries. Provider
entry data stores only the needed identity/display fields and private OAuth
fields, rather than the entire profile's unknown metadata.

The provider uses the default trusted Spotify API origin. Custom base URLs are
an injected fixture/developer seam and never come from service data, callback
queries or untrusted profile fields. The integration validates enqueue media
types before calling the wire client's generic queue method.

An explicit non-premium product keeps the pinned zero-feature policy. An absent
product is unknown entitlement, and also advertises no control features until
entitlement is known; it must not be labeled a free account. Read-only state
projection remains separate from advertised controls. Actual provider failures
remain typed errors; a missing response does not manufacture idle playback or
successful commands. These decisions are implementation requirements, not
evidence of an accepted runtime provider.

Use the existing contract in [`docs/OAUTH2-CLIENT-PORT.md`](OAUTH2-CLIENT-PORT.md):
the strong `OAuthFlowHost` owns provider registrations and resolver jobs; the
weak `OAuthFlowManager` supplies API routing; the private
`ConfigEntryTokenStore` uses the entry's token map and atomic data patches.
Loaded providers supply their real entry driver, application-credentials
platform, and post-authorization hook. The Spotify post-authorize hook must make
the bearer `GET /v1/me` call, validate usable identity and requested scopes,
choose title/unique ID under the migration rule above, apply any verified
eligibility policy, and return the complete entry data. The loaded entry driver
then owns initial profile/playback/device refresh, entity setup, and unload.

The port doc explicitly marks the Rust bridge as incomplete: current flow
results are only form/create/abort, the flow driver is synchronous, and the
HTTP adapter has no OAuth callback route. The required external/external-done
callback and async resolver must keep callback proof, code, tokens, and PKCE
material private, claim callback state once, resolve outside config locks, and
commit only if the flow lease is still active. Reauth must preserve the existing
entry ID and full unrelated data; refresh needs an atomic token-map update by
entry ID. Report these as host/API work owned by the OAuth stream rather than
inventing a Spotify-only callback API.

## Synthetic loopback fixtures and falsifiers

Use a local fake HTTP server only. These are fixture shapes, not live-account
observations. The pinned fixtures `current_user.json`, `playback.json`,
`playback_episode.json`, `devices.json`, and `playlist.json` are useful starting
points; add current fields and status fixtures below. Do not use real OAuth
credentials or Spotify endpoints in tests.

```json
{
  "account_id": "acct-stable-1", "id": "legacy-user-1",
  "display_name": "Living Room", "type": "user", "uri": "spotify:user:legacy-user-1",
  "href": "https://api.spotify.com/v1/users/legacy-user-1",
  "external_urls": {"spotify":"https://open.spotify.com/user/legacy-user-1"},
  "images": [], "product": "premium"
}
```

For the 200 playback parser fixture, start with the complete checked-in
`playback.json` and `playback_episode.json` responses; the track/album object
shown below is abbreviated for readability, not a complete
`spotifyaio`-2.1.0 DTO fixture.

```json
{
  "device": {"id":"dev-1", "is_active":true, "is_private_session":false,
    "is_restricted":false, "name":"Speaker", "type":"Speaker",
    "volume_percent":35, "supports_volume":true},
  "shuffle_state":false, "repeat_state":"off", "context":null,
  "progress_ms":12000, "is_playing":true, "currently_playing_type":"track",
  "item":{"type":"track", "uri":"spotify:track:t1", "name":"Song",
    "id":"t1", "duration_ms":180000, "explicit":false,
    "external_urls":{}, "href":"/t1", "disc_number":1,
    "track_number":1, "is_local":false,
    "album":{"album_type":"album", "total_tracks":1, "id":"a1",
      "images":[], "name":"Album", "release_date":"2024-01-01",
      "release_date_precision":"day", "uri":"spotify:album:a1",
      "artists":[{"id":"ar1", "name":"Artist", "uri":"spotify:artist:ar1"}]},
    "artists":[{"id":"ar1", "name":"Artist", "uri":"spotify:artist:ar1"}]}
}
```

```json
{"devices":[{"id":"dev-1", "is_active":true, "is_private_session":false,
  "is_restricted":false, "name":"Speaker", "type":"Speaker",
  "volume_percent":35, "supports_volume":true}]}
```

Also send a lowercase device `type` such as `speaker` in the loopback suite:
Spotify's current reference examples use lowercase strings, whereas the pinned
client enum and fixtures use title-case values. The native parser should accept
the documented string values without turning a new device label into a setup
failure.

Falsifiers to run against that loopback server:

1. Register the local Spotify provider and assert authorize URL has the exact
   allowlisted redirect, one-time state, `response_type=code`, client ID, and
   space-delimited scope; test denial, state mismatch, and redirect refusal.
   The pinned comma-scope test must fail against the corrected native URL.
2. Return token JSON with all standard fields and an unknown extra field. Verify
   the provider stores the full map, profile lookup uses the bearer token,
   `account_id` binds a new entry, null display name falls back cleanly, and a
   second authorization of the same account aborts. Reauth same account must
   update the same entry and preserve unrelated data/options; another account
   must leave it untouched. Refresh with and without a replacement refresh
   token; `invalid_grant` must start reauth once and must not loop.
3. Assert exact `GET /v1/me`, `/v1/me/player?additional_types=track,episode`,
   and `/v1/me/player/devices` requests and parse the fixtures above. Cover
   profile `account_id` with missing `product`, legacy `id` only, empty devices,
   nullable device ID/volume, restricted device, playback `204`, null item,
   episode, unknown future item type, and optional playlist 404/schema failure.
4. For every command in the table, assert method/path/query/body and bearer
   scope, return 204 with no body, and verify the media-player call returns
   successfully. Add 401, 403, 429 plus `Retry-After`, 5xx, malformed JSON, and
   ten-second timeout cases. Verify a command error does not trigger a success
   refresh; success waits one second then refreshes playback.
5. With a fake clock, assert 30-second playback cadence, the near-track-end
   `remaining + 1s` adjustment, five-minute device cadence, stable sorted source
   names, and a failed poll setting entity unavailable until a successful poll.
6. Cancel authorization resolution, setup during each initial read, an in-flight
   command, and unload with timers/listeners active. Assert no late token commit,
   no platform/entity left after failed setup, no callbacks/polls after unload,
   listeners are removed, and the owned API client is closed without closing a
   host-shared HTTP client.

## Pinned dependency and provenance

- Integration pin: `spotifyaio==2.1.0` in the pinned manifest and
  `requirements_all.txt`.
- Official PyPI release metadata:
  [`spotifyaio 2.1.0 JSON`](https://pypi.org/pypi/spotifyaio/2.1.0/json).
  Downloaded the published sdist from its metadata URL to
  `/tmp/codex-ha-spotifyaio-2.1.0-20261107-v123-audit/spotifyaio-2.1.0.tar.gz`;
  local SHA-256 equals PyPI's published sdist digest:
  `d2d3a802f081b5a41f74812f84d9376c40a151b9beff2fe960a2450c310ed77f`.
  The wheel SHA-256 in the same metadata is
  `8a7945634a05850cf55455aca18785399ea8dbfb5a94eac2c6c3b675048772cb`.
- Sdist metadata says Python `>=3.13,<4.0`; runtime dependencies are
  `aiohttp>=3.0.0`, `mashumaro>=3.11,<4.0`, `orjson>=3.9.10,<4.0.0`, and
  `yarl>=1.6.0`. Repository/homepage is
  [`joostlek/python-spotify`](https://github.com/joostlek/python-spotify).
- The sdist includes `LICENSE.md`, MIT, copyright Joost Lekkerkerker
  2023-2026. Preserve its copyright and license notice if copying or adapting
  any dependency code/model. The Rust port can implement the protocol from this
  contract without importing Python; do not imply source reuse unless it occurs.
- Source was inspected as text/AST only. No dependency code was imported or
  executed; no Spotify request, OAuth, browser/account action, package test, or
  playback/device action was made.

## Source anchors

- [Pinned manifest](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/manifest.json#L1-L13), [all requirements pin](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/requirements_all.txt#L3249), [scopes and media types](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/const.py#L11-L40), and [Spotify application credentials endpoints](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/application_credentials.py#L7-L12).
- [Config flow and reauth](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/config_flow.py#L35-L107), [setup/unload](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/__init__.py#L42-L89), and [coordinators](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/coordinator.py#L47-L232).
- [Entity/device metadata](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/entity.py#L10-L26), [media-player features/setup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/media_player.py#L48-L147), [state/properties/services/listener](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/media_player.py#L151-L452), and [browse reads](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/spotify/browse_media.py#L277-L399).
- Focused source tests: [flow URL and entry data](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/spotify/test_config_flow.py#L38-L106), [reauth/reconfigure](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/spotify/test_config_flow.py#L203-L355), [setup and refresh failures](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/spotify/test_init.py#L25-L160), [media/player service tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/spotify/test_media_player.py#L398-L695), and [polling tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/spotify/test_media_player.py#L765-L907).
- Verified package source in the downloaded sdist: `src/spotifyaio/spotify.py:114-173,336-437,456-462,486-499,646-649`; `src/spotifyaio/models.py:34-52,353-376,421-467,551-573`; `src/spotifyaio/exceptions.py:4-25`; `PKG-INFO:1-30`; `pyproject.toml:1-33`; `LICENSE.md:1-13`.
- Pinned inherited failure/availability behavior: [`update_coordinator.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/update_coordinator.py#L452-L603) catches coordinator errors and reschedules refresh; [`CoordinatorEntity.available`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/update_coordinator.py#L706-L729) mirrors `last_update_success`. OAuth host seams and pending bridge are recorded in `docs/OAUTH2-CLIENT-PORT.md:11-24,152-174,394-476`.
- Current primary protocol sources: [Authorization Code](https://developer.spotify.com/documentation/web-api/tutorials/code-flow), [PKCE](https://developer.spotify.com/documentation/web-api/tutorials/code-pkce-flow), [redirect URI rules](https://developer.spotify.com/documentation/web-api/concepts/redirect_uri), [refresh tokens](https://developer.spotify.com/documentation/web-api/tutorials/refreshing-tokens), [scopes](https://developer.spotify.com/documentation/web-api/concepts/scopes), [current profile](https://developer.spotify.com/documentation/web-api/reference/get-current-users-profile), [playback state](https://developer.spotify.com/documentation/web-api/reference/get-information-about-the-users-current-playback), [devices](https://developer.spotify.com/documentation/web-api/reference/get-a-users-available-devices), [rate limits](https://developer.spotify.com/documentation/web-api/concepts/rate-limits), [API statuses](https://developer.spotify.com/documentation/web-api/concepts/api-calls), and [developer-access update](https://developer.spotify.com/blog/2026-02-06-update-on-developer-access-and-platform-security).
- Current policy/API change sources: [February 2026 Web API changelog and supported endpoint list](https://developer.spotify.com/documentation/web-api/references/changes/february-2026) and [July 2026 developer-app limits and quota response](https://developer.spotify.com/documentation/web-api/references/changes/july-2026).
