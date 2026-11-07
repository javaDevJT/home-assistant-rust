# Native `system_health` port contract

## Scope and version identity

This contract is based on Home Assistant Core source commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, with the source component in
`homeassistant/components/system_health/` and tests in
`tests/components/system_health/`. That source package declares
`2026.12.0.dev0`; the source project requires Python `>=3.14.2` and pins
`.python-version` to `3.14.10` for development. The native runtime declares
`hass_core::VERSION` as `0.1.0-rust`. The source Python requirement and dev
pin are metadata, not observations of a running interpreter. The native port
has no Python runtime to report.

This is a backend API and provider contract. The Python frontend and its SDK
are external to this Rust worktree and remain unported. Do not claim a rendered
System Health page or a native frontend version.

## Pinned source contract

### Component and registration

- `homeassistant/components/system_health/__init__.py` declares
  `CONFIG_SCHEMA = cv.empty_config_schema(DOMAIN)`. This is an empty config
  schema; it is not a `str` schema or a provider configuration format.
- `async_setup` registers the WebSocket handler and creates
  `LazyIntegrationPlatforms(hass, "system_health", ...)`.
- `SystemHealthProtocol.async_register(hass, register)` is synchronous and
  returns `None`. A provider calls
  `SystemHealthRegistration.async_register_info(callback, manage_url=None)`.
  The registration holds one callback of type
  `Callable[[HomeAssistant], Awaitable[dict]]` and an optional `str` manage
  URL; another registration for the same domain replaces the previous one.
- `LazyIntegrationPlatforms.async_get_platforms()` discovers the platform
  only for currently loaded top-level integrations that declare a
  `system_health` platform. The platform is imported and processed on demand
  and cached. `_registered_domain_data` merges these results with the
  deprecated direct-registration map; a platform registration wins for a
  duplicate domain, and domains are sorted before collection.
- The legacy `system_health.async_register_info(hass, domain, callback)` path
  logs a deprecation that breaks in 2027.1. Native code should expose an
  explicit Rust provider-registration API, not invent a matching HA service.
- `homeassistant/components/homeassistant/system_health.py` registers one
  core callback. Its source fields include version, installation/container
  flags, user, Python version, OS and architecture, timezone, and
  `config_dir`. These are provider values, not fields guaranteed by the
  manager.

### `system_health/info` WebSocket behavior

The command is decorated with
`@websocket_command({probatio.Required("type"): "system_health/info"})` and
`@async_response`. The command schema uses `probatio.Required`, while the
component config schema uses `cv.empty_config_schema`. The type-only command
decorator sets its command schema to `False`; the connection path then requires
an exact built-in `int` ID greater than zero, an exact built-in `str` type,
and rejects `len(msg) > 2`. Thus the wire command has exactly `id` and `type`
and no extra fields; IDs must also increase on that connection. A separate
`MINIMAL_MESSAGE_SCHEMA` declaration uses `cv.positive_int`, `cv.string`, and
`ALLOW_EXTRA`, but is not the validation path for this command. Native code
should not copy those unrelated CV/extra rules. The handler has no
`require_admin` decorator or administrator check. It runs through the
authenticated WebSocket API, so authentication is required, but source code
does not require an administrator for this read-only command.

The response is a one-shot subscription with this order:

1. Each registered provider callback is started concurrently and awaited under
   a five-second callback timeout. Collection completes before the command
   acknowledgement and initial event.
2. The command acknowledgement is sent as a successful result with
   `result: null`.
3. One `initial` event contains every domain, its `info` object, and
   `manage_url` when set. Immediate values are present. Deferred coroutine or
   `Task` values are represented as `{"type":"pending"}`.
4. If there are deferred values, each completion produces an `update` event
   containing `domain`, `key`, and either `success: true` with `data`, or
   `success: false` with a generic failed error object. Updates are emitted
   for completed tasks without sorting; simultaneous completions have no
   stable order.
5. A `finish` event follows after all outstanding values complete. With no
   deferred values the sequence is acknowledgement, `initial`, `finish`.

The source envelope is a successful result with `result: null`, then ordinary
WebSocket event envelopes (`{"id":N,"type":"event","event":...}`). The
component itself does not add an HTTP route. The Cloud integration has a
separate administrator-only support-package download that calls `get_info`
and renders Markdown; that does not make `system_health/info` administrator
only.

For WebSocket values, a Python `datetime` is encoded as
`{"value":"<ISO-8601>","type":"date"}`. Other callback values are returned
without a manager-level type or secret-field filter. An async callback timeout
places `{"error":{"type":"failed","error":"timeout"}}` in that domain's
initial info. A callback exception uses the same shape with `error: "unknown"`;
the exception detail is logged, not sent. A deferred-value exception instead
produces a failed `update` with `error: "unknown"`. On unsubscribe while values
are pending, remaining tasks are cancelled and the handler returns without a
`finish` event.

`get_info(hass)` is a separate Python helper used by internal callers; it is
not the WebSocket transport contract. It awaits coroutine values in place,
stringifies datetimes, converts `{type: "failed"}` values to `"Failed: ..."`,
and includes exception text as `"Exception: ..."`. Do not reuse that
exception-detail behavior in the native WebSocket adapter.

`async_check_can_reach_url` performs a GET with a five-second total timeout.
Any HTTP response is considered `"ok"` because the response status is not
checked. `aiohttp.ClientError` maps to `{"type":"failed","error":"unreachable"}`;
timeout maps to `{"type":"failed","error":"timeout"}`. Optional
`more_info` is copied into the failed result. The response body is unused.

The separate source LLM tool, `system_health__get_system_info`, is registered
only for Home Assistant's LLM API and is marked read-only, non-destructive,
idempotent, and closed-world. It calls the Python system-info helper and the
external frontend version helper. Its `python_version` and `frontend_version`
must not be fabricated by the native port or confused with
`system_health/info`.

## Native integration boundary

- Add the native manager as an optional `ApiFeatures` field and route only the
  existing `system_health/info` WebSocket command through the extension
  dispatcher. Match the source command's `id`/`type` only schema and reject
  extra keys. The current API has no System Health feature, HTTP endpoint,
  service, or frontend panel; do not invent any of those surfaces. When the
  manager is absent, return the API's normal unconfigured-feature error rather
  than a synthetic empty health report.
- The only current `IntegrationCatalog` is the logger's map from integration
  domain to tracing-target aliases. It is not an installed-component catalog
  and cannot discover System Health providers. Native providers must be
  registered by native application/integration startup code; Python platform
  imports and source integrations remain external/unported.
- Follow the existing owner/handle lifecycle: the app retains the integration
  owner and passes its weak manager handle into `ApiFeatures`. In the current
  tree `Runtime` is a cloneable `Arc<RuntimeInner>` wrapper and there is no
  public `RuntimeWeak` type or `Runtime::downgrade()` API. `SystemLogIntegration`
  is the concrete pattern: the app retains its owner while
  `SystemLogManager` holds `Weak<ManagerInner>`. Do not claim or build around a
  nonexistent runtime-weak API. If a provider needs runtime data, pass the
  smallest read-only native context it actually uses; do not retain an
  unrelated strong runtime clone in the API feature.
- Proposed native SDK (new `hass-system-health` crate):
  `SystemHealthIntegration` owns a registry and exposes a weak
  `SystemHealthManager`; `register_info(domain, callback, manage_url)` stores
  one callback and optional URL per domain, with later registration replacing
  that domain. Providers register explicitly during native startup.
  `SystemHealthInfo` is a map of field names to values, with deferred values
  represented explicitly so the WebSocket adapter can produce `pending` and
  later `update` events. One suitable Rust boundary is:

  ```rust
  type HealthFuture<T> =
      Pin<Box<dyn Future<Output = hass_core::Result<T>> + Send + 'static>>;
  type SystemHealthInfo = BTreeMap<String, SystemHealthValue>;
  type SystemHealthCallback =
      Arc<dyn Fn() -> HealthFuture<SystemHealthInfo> + Send + Sync + 'static>;

  enum SystemHealthData {
      Json(serde_json::Value),
      DateTime(chrono::DateTime<chrono::Utc>),
  }
  enum SystemHealthValue {
      Immediate(SystemHealthData),
      Deferred(HealthFuture<SystemHealthData>),
  }
  ```

  This is the isolated draft SDK shape, outside the accepted workspace until
  enrollment. Both immediate and deferred dates retain typed encoding.
  Provider closures capture only the native subsystem handles they need; do
  not capture futures borrowed from a WebSocket request. Keep callback
  registrations in manager-owned state, not in a public runtime event
  subscription.
- The native WebSocket dispatcher currently allows authenticated identities
  to use commands unless the command is in its administrator list or a
  specific read/control guard applies. `system_health/info` is not an
  administrator command and is not one of the state-read commands guarded by
  `can_read_states`; it is also not a mutation blocked for read-only
  identities. Keep it out of `command_requires_admin`; test admin, ordinary,
  and read-only users through `AuthManager` because the static API token is
  intentionally treated as admin. Missing/invalid authentication still fails
  before command dispatch.
- The native API's `ApiState.config` is generic application configuration,
  not a system-info record. The root app currently has no Python runtime,
  frontend version provider, supervisor, installation type, or persisted
  `config_dir` field. Report only data with a real native source: use
  `hass_core::VERSION` for native core version and the configured timezone
  when the root passes it. Omit unknown Python, frontend, OS-release,
  installation, supervisor, and path values. Do not use the source package
  version `2026.12.0.dev0` as the native runtime version.
- The source provider does expose `config_dir` and `user`, and arbitrary
  provider values are returned to authenticated ordinary users. This makes
  callback redaction a real boundary: never serialize the whole config, token
  file contents, auth/session data, secret values, resolved `!secret` values,
  database paths, or credential-bearing URLs. A path field is emitted only if
  the root app has an explicit, approved native value for it. Do not log
  callback values. Generic transport errors must not include panic text,
  credentials, or provider exception strings.
- The source root event list has 169 public types and excludes only `close`
  and `state_reported`; an `internal` or typed label is not a privacy or
  recording exemption. System Health itself has no source event-bus behavior.
  On the native side, `Runtime::event_listeners()` counts real public
  broadcast receivers under `"*"`. Do not use that bus for callback
  bookkeeping or silently exclude a health listener from diagnostics.

## Focused native falsifiers

Use in-process fixtures only. The existing native API protocol tests establish
the loopback pattern with `router_with_features`, a `TcpListener` bound to
`127.0.0.1:0`, and a real `tokio_tungstenite` client. The System Health tests
should follow that pattern without contacting an external provider or host.

1. Assert the command accepts a positive integer ID and string type only;
   reject a Boolean/string ID, non-string type, duplicate/decreasing ID, or
   any third message field. Assert the acknowledgement is `result: null`.
2. Register two deterministic providers. Assert sorted domain order,
   optional `manage_url`, a single callback per domain, a fast value in the
   initial event, a pending marker for one delayed field, its update, then
   `finish`. Keep the delayed future behind a test gate so event order is
   reproducible.
3. Return an immediately failed callback, a callback that exceeds five
   seconds, and a deferred field that fails. Assert the callback timeout and
   exception results are generic, and that no panic/error text reaches the
   client. Exercise the value/detail shape of the native reachability helper
   against loopback success, loopback 5xx, a closed loopback port, and a
   locally delayed response. A 5xx still yields `ok` if source parity is
   preserved, since source checks reachability rather than HTTP status.
4. Unsubscribe while a deferred field is blocked. Assert its future is
   cancelled, no later update is emitted, and no completion event is promised
   after cancellation. Assert completing requests release their subscription
   and do not leave manager tasks behind.
5. Exercise ordinary, read-only, admin, invalid-token, and unauthenticated
   connections. Ordinary and read-only users must be able to read health;
   only authentication is required. Verify no new HTTP route or service is
   introduced.
6. Leave the feature unset and assert the configured-feature error. Drop the
   app-owned manager owner while keeping an API handle and assert requests
   fail cleanly through the weak handle.
7. Use sentinel secret values and paths in fixture configuration and callback
   errors; assert none appear in initial data, updates, logs returned to the
   client, or persisted runtime state. Assert invoking System Health does not
   add a public runtime event-bus receiver.
8. Assert the core provider reports `core-0.1.0-rust` and any explicitly
   passed native timezone. Assert it omits Python/frontend versions,
   `config_dir`, OS release, installation type, and Supervisor fields when no
   native source supplies them; never synthesize successful external
   reachability results.

## Source and native references

- Pinned source component: `homeassistant/components/system_health/__init__.py`
- Pinned source schema, connection, and lazy discovery:
  `homeassistant/components/websocket_api/decorators.py`,
  `homeassistant/components/websocket_api/connection.py`,
  `homeassistant/components/websocket_api/messages.py`,
  `homeassistant/helpers/integration_platform.py`
- Pinned core provider and separate LLM tool:
  `homeassistant/components/homeassistant/system_health.py`,
  `homeassistant/components/system_health/llm.py`
- Pinned component tests: `tests/components/system_health/test_init.py` and
  `tests/components/system_health/test_llm.py`
- Separate support-package HTTP caller and test:
  `homeassistant/components/cloud/http_api.py`,
  `tests/components/cloud/test_http_api.py`
- Native API feature and command policy:
  `crates/hass-api/src/lib.rs`, `crates/hass-api/src/extensions.rs`,
  `crates/hass-api/src/websocket.rs`
- Native owner/handle example and loopback protocol harness:
  `crates/hass-system-log/src/lib.rs`,
  `crates/hass-api/tests/system_log_protocol.rs`,
  `crates/hass/src/main.rs`
- Native runtime version and event receiver accounting:
  `crates/hass-core/src/lib.rs`
