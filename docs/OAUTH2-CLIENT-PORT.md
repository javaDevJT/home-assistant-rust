# External OAuth 2.0 Client Port

## Scope and boundaries

This is a source contract for a Rust external OAuth 2.0 client/flow engine,
pinned to Home Assistant source revision
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. It records the helper behavior,
flow protocol, likely port seam, focused falsifiers, and explicit source
deviations. Source was read with `git show`; no Home Assistant imports, package
execution, real provider traffic, browser action, or credentials were used.

This engine is distinct from `hass-auth`. The latter implements native user
login, JWT sessions, refresh, and MFA. Its `TokenSet` at
`crates/hass-auth/src/lib.rs:173-195` is the native login/refresh pair; do not
reuse it for provider OAuth. External tokens are provider-owned data, persisted
with the integration config entry, and may contain provider-specific fields.

The adjacent [Application Credentials Port](APPLICATION-CREDENTIALS-PORT.md)
owns private credential collection, platform loading, typed
`AuthorizationServer`/default descriptors, and custom opaque implementation
descriptors. This client consumes that resolver contract. It must not invent a
catalog of working integrations or infer endpoints from an opaque custom
implementation. Neither this document nor that SDK contract establishes that
any real provider has been ported.

## Source behavior

### Flow and redirect binding

- `AbstractOAuth2FlowHandler` identifies an integration by `DOMAIN`; the
  selected implementation is persisted as `auth_implementation`. A provider
  implementation has stable `domain` and display `name`, builds its authorize
  URL for a flow ID, resolves callback data into a token map, and refreshes a
  token (`homeassistant/helpers/config_entry_oauth2_flow.py:118-194`).
- Redirect selection is source-defined: when the `my` component is loaded, use
  `https://my.home-assistant.io/redirect/oauth`; otherwise a current request
  and `HA-Frontend-Base` header are required, and the callback path
  `/auth/external/callback` is appended
  (`config_entry_oauth2_flow.py:76-78,104-115`). The header is concatenated
  without local origin validation. The host adapter must supply the intended
  public callback origin and make its trust policy explicit; do not substitute
  the native login listener address.
- The local authorization URL contains `response_type=code`, `client_id`, the
  exact `redirect_uri`, and state. `yarl.URL.update_query` retains an existing
  endpoint query; implementation extras are added by the local helper, then
  flow-handler extras are applied (`config_entry_oauth2_flow.py:245-257,435-438`;
  `helpers/oauth2.py:78-99`). Existing tests pin an endpoint query and
  space-encoded scope (`tests/helpers/test_oauth2.py:64-78`,
  `tests/helpers/test_config_entry_oauth2_flow.py:734-789`). Preserve query
  values and encode each parameter once.
- Local state is an HS256 JWT over `flow_id` and the selected `redirect_uri`.
  Its signing key is randomly generated in process memory and is not a native
  login JWT (`config_entry_oauth2_flow.py:249-257,820-840`). The code exchange
  takes `redirect_uri` from this verified state rather than rebuilding it from
  the callback request. The JWT payload is signed, not encrypted; the source
  does not add an `exp` claim.
- The unauthenticated GET callback requires state, verifies its signature,
  accepts either `code` or `error`, and resumes the flow ID from state. It
  submits the callback data to the config-flow manager and returns a small
  `window.close()` HTML response
  (`config_entry_oauth2_flow.py:711-752`). Source checks `code` before `error`
  if both are present. The callback does not itself exchange the code; it
  advances the flow to `creation` or `authorize_rejected`.
- The current config-flow protocol requires `flow_id` and `handler`; external
  results carry `type="external"`, `url`, and `step_id`, then completion carries
  `type="external_done"` and the next `step_id`
  (`homeassistant/data_entry_flow.py:169-193,832-862`). OAuth step IDs are
  `auth`, `creation`, and `authorize_rejected`. A successful flow stores
  `auth_implementation` and `token` in entry data. Do not model this as a
  native-login token exchange.

### Code exchange, PKCE, HTTP, and errors

- The code grant sends `grant_type=authorization_code`, the callback `code`,
  the redirect URI bound into state, and implementation-specific token fields.
  Client authentication defaults to `client_secret_post`; supported methods
  are `client_secret_post` and `client_secret_basic`. Basic auth form-encodes
  each credential before Base64; a public/empty-secret client sends its ID in
  the form body (`helpers/oauth2.py:29-51,`;
  `config_entry_oauth2_flow.py:259-268,289-303`).
- PKCE uses a 43–128 character verifier (default 128), S256 challenge, and
  unpadded base64url digest; the verifier is included only in token resolution
  (`helpers/oauth2.py:54-75`; `config_entry_oauth2_flow.py:306-384`). Keep the
  verifier private and isolated to its authorization flow.
- Authorization URL generation and token resolution each have a 30 second
  timeout (`config_entry_oauth2_flow.py:81-84,498-529`). HTTP status mapping is
  429/5xx → transient, 4xx → reauth, other response errors → generic OAuth
  error, and no-response client failures → connection error
  (`helpers/oauth2.py:101-120`). Initial flow errors become
  `oauth_unauthorized` for reauth errors, `oauth_failed` for other mapped HTTP
  errors, and `oauth_timeout` on timeout
  (`config_entry_oauth2_flow.py:522-537`).
- Token JSON is a provider map. The initial exchange coerces `expires_in` to
  integer and stores `expires_at = now + expires_in` with all provider fields
  (`config_entry_oauth2_flow.py:539-555`). Refresh merges the new map over the
  old map, preserving a prior refresh token if no replacement is returned;
  it rejects an absent/blank access token and recomputes expiry
  (`config_entry_oauth2_flow.py:170-186,270-287`). Reject malformed/non-object
  responses as typed token errors; do not manufacture a zero expiry or let a
  conversion panic escape.
- `OAuth2Session` considers a token usable only when it expires more than 20
  seconds in the future. It rechecks under a per-session async lock, refreshes,
  validates access token and expiry before persistence, updates the entry, then
  injects a single bearer Authorization header for the API request
  (`config_entry_oauth2_flow.py:81-82,755-817`; `helpers/oauth2.py:174-181`).
  Native token storage must be private, atomic, and shared for one account so
  independently created request handles cannot race refreshes. Preserve
  unrelated provider token fields. Never expose access/refresh tokens, client
  secrets, authorization codes, state, or PKCE verifier through `Debug`,
  `Display`, errors, URLs in logs, or diagnostic output.

### Credential/provider registration seam

- Local registrations are grouped by config-flow integration domain and keyed
  by implementation domain. Providers are registered under a provider domain,
  all are called for the requested integration domain, and returned
  implementations are keyed by their `domain`; a later provider can replace
  the same key (`config_entry_oauth2_flow.py:634-710`).
- If no implementations are available, the flow distinguishes missing
  application credentials from missing configuration. A provider failure with
  no other implementation is `oauth_implementation_unavailable`; partial
  availability remains usable. Looking up an entry with a missing selected
  key yields provider-unavailable/retry if any provider failed, otherwise an
  unknown-implementation/reauth path (`config_entry_oauth2_flow.py:440-486,
  643-691`). Keep these states distinct; do not convert provider outages into
  missing credentials or successful discovery.
- The clean Rust seam is a resolved implementation from the application
  credentials SDK: a typed default authorization-server descriptor plus its
  private credential handle, or an opaque custom implementation handle. The
  OAuth engine may implement the source-compatible local exchange for the
  default descriptor. Custom implementations keep their own authorize,
  callback-resolution, and refresh behavior behind their opaque adapter. The
  flow engine consumes stable implementation IDs and service/error domains; it
  does not register source integrations or fabricate endpoints.

Candidate boundary (names illustrative; this is not an implementation plan):

```text
OAuthFlowEngine
  start(integration_domain, flow_context) -> Form | ExternalStep | Abort
  callback(flow_id, verified_state, code_or_error) -> ExternalStepDone | Abort
  resolve(flow_id) -> ExternalStepDone | CreateEntry | Abort
  refresh(account_id, token_snapshot) -> TokenSnapshot | typed OAuthError

ResolvedImplementation
  Local(AuthorizationServerDescriptor, PrivateCredentialHandle)
  Custom(OpaqueImplementationHandle)

ExternalOAuthToken
  provider_fields: private JSON object
  expires_at: absolute timestamp
```

Keep flow state, callback state, provider tokens, and native login JWTs as
separate types and persistence domains.

## Source edges and port decisions

1. **Token endpoint redirect forwarding.**
   `async_token_request` defaults to `allow_redirects=True`
   (`homeassistant/helpers/oauth2.py:123-140`). The local code/refresh path
   calls it without overriding that default
   (`config_entry_oauth2_flow.py:289-303`). Such POSTs can carry an auth code,
   PKCE verifier, client secret/Basic header, or refresh token to a redirect
   target. The source test proves refusal only when explicitly passed false
   (`tests/helpers/test_oauth2.py:101-123`). **Root decision:** the client
   engine refuses redirects for token endpoint POSTs. This is an intentional
   security correction to the local helper omission; browser authorization
   redirects are separate and remain required.
2. **Initial `expires_in` conversion asymmetry.** The flow catches
   `ValueError` but not `TypeError` at
   `config_entry_oauth2_flow.py:539-549`; the refresh helper catches
   `KeyError`, `TypeError`, and `ValueError` at lines 180-185. Existing initial
   flow tests cover missing and non-numeric string values, not `null`
   (`tests/helpers/test_config_entry_oauth2_flow.py:308-369`), while refresh
   tests do cover null (`:1207-1254`). **Root decision:** reject invalid types
   as a typed error in the port; do not copy the uncaught failure or coerce
   booleans/null into fabricated expiries.
3. **Redirect-origin authority.** `HA-Frontend-Base` is concatenated without
   validation at `config_entry_oauth2_flow.py:104-115`. Preserve the documented
   public callback behavior through a host adapter with an explicit trusted
   origin source. Any stricter allowlist or compatibility policy belongs to
   the host-integration decision; it must not be silently replaced with a
   localhost/native-login URL.
4. **Callback response status.** Missing state and missing code/error return a
   plain response without explicit status (`config_entry_oauth2_flow.py:718-743`),
   which means the framework default is success status. Invalid signed state is
   explicitly 400. A native callback should make malformed requests a typed
   client error (400) without advancing a flow. This is a protocol robustness
   correction, not evidence that the current callback accepts invalid state.
5. **Sensitive diagnostics.** The source logs the token endpoint URL and may
   log an OAuth error description or up to 200 characters of a non-JSON error
   body (`helpers/oauth2.py:135-157`). Provider bodies are not guaranteed to be
   secret-free. Native errors/logs should keep status and safe error class,
   redact request URL query data, and omit response bodies and token material.

## Focused loopback falsifiers

Use a local-only HTTP server with controllable endpoints; do not call providers
or use real credentials. Keep the ordinary mocked source tests as parity
oracles, but add the socket boundary cases that mocks can miss.

- Authorization URL: endpoint with an existing query, reserved characters in
  client ID/redirect URI/state/scope, duplicate extra keys, plus signs and
  spaces. Parse back and assert exact values, no dropped endpoint query, and
  single encoding. Assert PKCE challenge/verifier pair and 43/128 boundaries;
  parallel flows must not share verifier or state.
- Callback: missing/invalid state, state for a different flow, unknown/expired
  flow, missing code and error, denial, code success, both `code` and `error`,
  and callback replay. Assert the exact external-step/done/abort shape, one
  flow advances once, and malformed requests do not exchange tokens.
- Code exchange: assert method, form fields, redirect URI from flow state,
  exactly one client-auth method, RFC form-encoded Basic credentials, PKCE
  verifier, and timeout. Serve 301/302/307/308 to a second loopback listener;
  assert the token POST is rejected and the second listener receives no body
  or Authorization header.
- Token bodies: valid object with extra provider fields; malformed JSON,
  scalar/list JSON, missing/null/string/bool `expires_in`, and blank access
  token. Assert typed classification, no token/entry mutation on bad response,
  no panic, and no success-looking zero expiry. Pin zero/negative lifetime
  handling as an explicit contract boundary instead of silently coercing it.
- Status/network: 400 invalid grant, 401, 429, 500/503, redirect, connection
  close, DNS/connection failure, and timeout. Assert reauth/transient/generic
  mapping and that error/log formatting contains no code, secret, verifier,
  bearer token, refresh token, URL query, or response-body content.
- Refresh/cache: preserve prior refresh token and unknown provider fields;
  accept rotated refresh token; never extend expiry without a new usable access
  token. Test the 20-second skew boundary and concurrent requests through two
  handles for one account; one refresh result is committed atomically and all
  requests use the new bearer token.
- Registry/flow: zero/one/many implementations, stale selected ID, partial
  provider failure, all-provider failure, unknown implementation, and custom
  opaque implementation dispatch. Verify stable `auth_implementation` IDs,
  domain isolation, and source-compatible retry versus reauth outcomes. No
  known-provider registry or real provider is required for engine acceptance.

## Pinned source and dependency anchors

- `homeassistant/helpers/config_entry_oauth2_flow.py:104-115` redirect;
  `118-384` implementation, local exchange, and PKCE;
  `387-565` config-flow state machine; `634-710` provider lookup;
  `711-752` callback; `755-840` session, refresh lock, and state JWT.
- `homeassistant/helpers/oauth2.py:29-99` client auth, PKCE, URL construction;
  `101-171` error mapping and token HTTP; `174-181` bearer API request.
- `homeassistant/data_entry_flow.py:169-193,832-862` result fields and
  external-step protocol; `homeassistant/config_entries.py:321-330` config
  entry result extension.
- `tests/helpers/test_oauth2.py:20-166` PKCE, header replacement, query,
  token headers, redirect refusal, and client auth. Key flow tests:
  `tests/helpers/test_config_entry_oauth2_flow.py:166-305` missing providers,
  aborts, timeout; `:308-423` bad token and rejection;
  `:579-705` token error mapping; `:742-858` full flow and refresh;
  `:889-1033` expiry/session behavior; `:1113-1302` refresh errors and
  malformed tokens; `:1625-1755` PKCE; `:1757-2170` provider failures and
  removed implementations. These tests use Home Assistant test fixtures and
  aiohttp mocks; they were inspected, not run in this audit.
- Pinned Python is `>=3.14.2` (`pyproject.toml:24`). Runtime source pins
  `aiohttp==3.14.5` (`pyproject.toml:31`), `PyJWT==2.15.1` (`:58`),
  `probatio==0.13.0` (`:85`), and `yarl==1.25.1` (`:86`); package constraint
  is `multidict>=6.4.2` (`homeassistant/package_constraints.txt`). The focused
  test stack pins `pytest==9.2.0`, `pytest-asyncio==1.4.1`, and
  `pytest-aiohttp==1.1.1` (`requirements_test.txt:26-38`). These anchor the
  source audit; they are not instructions to add Rust dependencies.

## Native host config-flow bridge contract

This supplement records the host boundary from Home Assistant Core commit
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
It extends the OAuth client contract above; it does not replace its SDK-level
proof, token, or transport requirements.

### External flow protocol and state

Home Assistant's external-step methods produce these JSON objects (object key
order is immaterial):

```json
{
  "flow_id": "<native flow id>",
  "handler": "<integration domain>",
  "type": "external",
  "url": "<authorization URL>",
  "description_placeholders": null,
  "step_id": "auth"
}
```

```json
{
  "flow_id": "<native flow id>",
  "handler": "<integration domain>",
  "type": "external_done",
  "step_id": "creation"
}
```

The external-step builder includes `description_placeholders: null` when none
are supplied; `external_done` includes the next step in `step_id` and has no
URL. Neither result includes callback code, state proof, provider tokens, or
flow-private context. The API serializer leaves these results intact when they
have no form schema. Core's generic flow test confirms that callback completion
notifies progress and the frontend then refreshes the flow to run the next
step.

The default local implementation signs a JWT state containing `flow_id` and
`redirect_uri` with HS256. Its random signing secret is created lazily in
process-local `hass.data`; generated state has no `exp` claim. The callback
verifies that signature, uses the signed flow ID to resume the flow, and the
token request uses the signed redirect URI. This is the source behavior, not a
reason to weaken the SDK's per-flow proof epoch, expiry, or expected-flow-ID
checks. The pinned local PKCE helper stores its verifier on the implementation
instance, so a native bridge should use the SDK's per-authorization verifier
and never share that proof across native flows.

### Callback, browser, and cancellation behavior

Core registers `/auth/external/callback` as a global unauthenticated GET view
from the auth component. It requires `state`, rejects an invalid signature
with HTTP 400, accepts `code` before `error` when both are present, and passes
`{"state": <decoded state>, "code": ...}` or `{"state": ..., "error": ...}`
to the current flow. Missing state and missing code/error return plain text
without an explicit non-200 status. A valid callback returns HTML containing
`window.close()` after the flow resumes.

The callback does not exchange the token. The auth step stores callback data
privately and returns `external_done` with `step_id="creation"` when code is
present, or `step_id="authorize_rejected"` when error is present. A subsequent
flow refresh performs token exchange or aborts with `user_rejected_authorize`.
The pinned tests cover successful authorization, provider denial, malformed
token expiry, and this refresh transition. They do not cover callback replay or
concurrent duplicate callbacks. Core's state JWT has no used-state ledger; a
native callback must claim each authorization once and make `flow_id` plus the
expected SDK proof identity agree before any token request. After a flow is
removed, a callback for it resolves to `UnknownFlow`; the callback view has no
local `UnknownFlow` translation.

Core constructs a redirect URI from the current request's
`HA-Frontend-Base` header plus `/auth/external/callback`, or uses the My Home
Assistant redirect when that component is present. The core external-step
builder and JSON adapter pass the generated `url` through without an
authorization-host or scheme allowlist. The pinned core tests establish the
flow-refresh contract, but do not include the matching frontend JavaScript or
prove its URL allowlist behavior. A native host must choose the callback origin
from trusted host configuration, validate the authorization URL policy at its
own boundary, and never accept callback query data as a redirect URI.

Flow cancellation removes the in-progress handler and cancels a progress task;
the OAuth callback itself is an HTTP request, not a flow-owned resolver task.
The native host should make callback resolution flow-owned: abort and host
shutdown cancel the owned resolver, and a late resolver must re-check its
active flow lease before committing any result. A disconnected browser request
must not leave an unowned resolving flow.

### Config-entry association and refresh

The provider token is stored in the integration's config-entry data under
`auth_implementation` and `token`; it is separate from Home Assistant login
tokens. New-flow state holds the selected implementation and callback data
privately. Reauth is associated with the existing entry, not a new account: the
flow context carries `entry_id` and `source="reauth"`. On successful reauth,
Core replaces the existing entry's data, clears `needs_reauth`, and reloads
that same entry. A driver completing reauth therefore has to return the full
entry data (including unrelated integration settings), not only the new token.

The Core refresh session treats a token as valid only when `expires_at` is more
than 20 seconds ahead. It serializes refreshes per session, rejects a refreshed
token missing a nonempty access token or expiry, merges the new token into the
existing entry data, and starts reauth for a refresh-token authorization error.
The current Rust `ConfigEntry.data` is an opaque map, which can hold the same
`auth_implementation`/`token` shape, but `ConfigEntries` has no public narrow
token-data update method for an `OAuthTokenStore` adapter yet. Refresh commits
need an atomic manager operation keyed by the existing `entry_id`; they must
preserve the rest of the map and must not turn a missing/error token into a
successful empty token.

### Rust host seam and safe async boundary

The current Rust seams are intentionally synchronous:

- `ConfigFlowRequest` carries native `flow_id`, domain, step, source,
  `entry_id`, and user input. `ConfigFlowResult` currently has only `Form`,
  `CreateEntry`, and `Abort`; `ConfigEntryDriver::config_flow` is synchronous.
- `hass-api::config_entries::run_manager` dispatches each manager operation
  through `tokio::task::spawn_blocking`. It is suitable for bounded state
  claims and commits, not async provider HTTP.
- `ConfigEntries::flow_result` returns the cached result without running the
  driver. Public `submit_flow` treats its JSON as user input. The HTTP flow
  init/get/post/delete endpoints require an admin identity; there is no public
  OAuth callback endpoint in the current adapter.
- `ConfigEntries::begin_reauth_flow` already creates a private reauth request
  with the original `entry_id` and marks that entry `needs_reauth`, but the
  current HTTP adapter does not expose it. On `CreateEntry`, `finish_flow`
  replaces all `data` for both reauth and reconfigure, clears `needs_reauth`,
  and reloads the original entry.

The smallest safe extension keeps the existing sync driver contract for
ordinary forms and final entry creation, and adds an OAuth-specific async
callback coordinator around it:

1. Add `external` and `external_done` flow results and a private external-flow
   record keyed by native flow ID. Bind the record to the integration domain,
   source, existing `entry_id` when reauthing, and the SDK flow/proof ID. Keep
   state proof, code, resolved token, and cancellation ownership out of
   `ConfigFlowRequest.context` and serialized JSON.
2. Use the SDK's async proof-admission operation outside `run_manager` to
   verify and consume a one-shot claim bound to the expected SDK flow ID; this
   phase must not make a provider request. Then use a narrow manager operation
   to atomically change only the matching active native flow from awaiting
   callback to resolving and return a flow lease. Reject wrong-flow, stale,
   aborted, and replayed claims before any provider token request. Release all
   manager locks before continuing.
3. Run the async code exchange outside `run_manager`, `spawn_blocking`, and
   config-entry/state locks. Make the resolver owned by the flow record; abort
   and host shutdown can cancel it. Do not persist a token until a short commit
   transaction confirms that the same flow lease is still active.
4. On success, retain the resolved token only in private flow state and publish
   `external_done` with `step_id="creation"`; on denial, publish
   `external_done` with `step_id="authorize_rejected"`. Notify flow progress.
   Make the flow GET advance this external-done state exactly once to the
   existing synchronous creation/rejection driver step. Today GET only returns
   cached JSON, so this requires an explicit external-done advancement path.
   Do not route callback data through the public admin `submit_flow` endpoint.
5. Let the existing `CreateEntry` path persist the initial token under the new
   entry ID. For reauth, preserve the existing ID/source association and merge
   the refreshed auth fields with the complete existing integration data before
   `finish_flow` replaces it. Implement refresh storage as a narrow atomic
   token-map update keyed by that entry ID and bound to the SDK's expected
   snapshot. Keep factory shutdown owned by the host application lifecycle and
   close per-entry clients from driver unload.

This uses the existing sync driver for the last, local state transition while
keeping network awaits outside its blocking dispatch. If SDK proof admission
and code exchange are currently one operation, split them into a one-shot
proof/flow claim and an async resolve operation; do not hold a manager lock or
`spawn_blocking` thread across the token endpoint request.

### Loopback acceptance falsifiers

Use only literal loopback listeners and synthetic credentials. The host bridge
is incomplete if any of these fail:

- Starting a native flow returns the exact external-step JSON above and a
  callback URI on the configured loopback listener. The callback URI is not
  derived from a provider response, request `Host`, or callback query.
- A valid callback advances the same native flow once, closes the browser, and
  then a frontend-style flow GET advances `external_done` to creation. Missing,
  malformed, expired, wrong-flow, already-used, aborted-flow, and duplicate
  callbacks do not exchange a code or mutate an entry. An `error` callback
  performs no token POST; if both `code` and `error` are supplied, pin the
  chosen precedence explicitly (Core chooses `code`).
- The loopback token endpoint receives one authorization-code POST with the
  original redirect URI and that flow's verifier. Bad response, timeout,
  cancellation, or redirect cannot create an entry or persist an unbound token.
- Successful initial authorization creates one entry with
  `auth_implementation` and the provider token. Reauth updates the original
  entry ID, preserves unrelated `data`, clears `needs_reauth`, and reloads it.
  The next resource request exercises refresh through the real token store;
  access/refresh rotation is persisted without losing unknown provider fields
  or other entry data.
- Abort during resolution, refresh cancellation, and host shutdown leave no
  stale callback task and no late token commit. Native login `TokenSet` state
  remains untouched.

Source anchors for this supplement: [OAuth flow and callback at the pinned
commit](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/config_entry_oauth2_flow.py#L104-L115),
[external result types and transitions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/data_entry_flow.py#L163-L193),
[callback route registration](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/auth/__init__.py#L210-L220),
[generic external-step test](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/test_data_entry_flow.py#L514-L561),
and the current Rust seams in `crates/hass-config/src/lib.rs` and
`crates/hass-api/src/config_entries.rs`.
