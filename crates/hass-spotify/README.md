# hass-spotify

Bounded native HTTP models and client for the Spotify Web API, integrated in
the root Cargo workspace. Run focused checks with `cargo test -p hass-spotify
--locked --offline`.

## Scope and ownership

This crate owns Spotify wire models, request authorization adapters, transport,
pagination, typed HTTP errors, and loopback protocol fixtures. The Home Assistant
runtime, OAuth provider registration, profile identity migration, entities,
polling, and lifecycle are owned by `crates/hass/src/spotify/`. Tests use only
numeric-loopback HTTP servers and synthetic OAuth token values; they do not
contact Spotify or use real credentials.

## Contract provenance

The endpoint and command contract is audited in `docs/SPOTIFY-PORT.md` against
Home Assistant Core commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and the
published `spotifyaio` 2.1.0 source archive (sdist SHA-256
`d2d3a802f081b5a41f74812f84d9376c40a151b9beff2fe960a2450c310ed77f`). The
native implementation is independently written; the upstream Python package is
not copied, imported, or executed here.

## Native usage

Use `OAuthSessionAuthorizer` for persisted runtime requests. The config-flow
post-authorization profile lookup can call
`SpotifyClient::get_current_user_with_token` with the validated, one-shot
`ExternalOAuthToken` before persistence. The transport owns its reqwest client,
does not follow redirects, has bounded time/body/pagination, and propagates
cancellation by keeping each request in the caller's future rather than
spawning API work.
