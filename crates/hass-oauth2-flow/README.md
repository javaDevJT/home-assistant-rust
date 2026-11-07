# hass-oauth2-flow

Native host orchestration for Home Assistant style config-entry OAuth flows.
The crate connects `hass-oauth2-client`, `hass-application-credentials`, and
`hass-config`; it does not provide a real integration catalog or provider
credentials.

## Ownership

`OAuthFlowHost` is the strong lifecycle owner. Its `OAuthFlowManager` and
provider registration guard are weak handles. A loaded integration registers
its actual application-credentials platform and an `OAuthFlowProvider` that
supplies scopes, a local client configuration hook, and a post-authorization
identity hook. Opaque custom OAuth implementations remain unsupported by this
local token engine.

The host keeps authorization state, callback proof, code, PKCE verifier, and
provider token values private. The callback verifies and stages proof, then
publishes `external_done`; a later flow GET claims the transition and performs
the token exchange. It persists `auth_implementation` and the provider token
inside the config entry. Reauthentication uses the current entry and preserves
unrelated data, options, preferences, and identity.

Use `OAuthFlowHostOptions` with a callback URL selected and validated by the
application. Incoming request headers cannot change this URL. Call the host's
async `shutdown()` during teardown; application shutdown separately drains the
global `ConfigEntries` flow tasks and the `OAuthClientFactory`.

Provider unregister and host shutdown retain one cleanup job per operation.
Canceling a waiter does not cancel cleanup: it cancels and joins pre-commit
resolvers, waits for admitted commits and deferred activation, then removes the
exact Core driver, evicts its clients, and releases the platform registration.
Flow expiry is enforced on GET, submit, callback, and config-flow admission;
committing flows remain protected by the durable-commit barrier. Completed
per-flow cleanup records are reaped after signaling their shared waiters.

## Acceptance tests

`tests/native_flow.rs` uses a controlled numeric-loopback token endpoint and
fake loaded providers. It covers the external and external-done protocol,
private persistence and refresh, reauthentication and stale revision conflict,
callback denial/replay, concurrent GET, TTL cleanup, resolver cancellation,
idle GET and picker-submit expiry, provider unregister/re-register after a
canceled waiter, weak-owner Drop cleanup, and canceled/concurrent shutdown
waiters during a gated durable commit. It also repeats expiry cycles against one
provider and checks that the provider remains usable. No real provider or
credential is used.
