# hass-api

This crate exposes the bounded native runtime through an Axum REST API and a
Home Assistant-style WebSocket API. It does not run Python integrations; REST
state writes update the runtime and do not control devices.

## Router and authentication

Construct **ApiState** with the runtime, optional static administrator token,
and base configuration JSON. **router_with_features** also accepts native auth,
registry, template and config-entry managers, plus a network-origin policy.
Optional native notification, zone, person and image managers supply their private feeds.
An optional webhook manager supplies process-local inbound callbacks.
Protected requests accept the static token or native session JWT. **/health**
returns **{"status":"ok"}**. WebSocket authentication follows **auth_required**
with an **auth** message. Handshakes expose the native **ha_version** value
**0.1.0-rust** rather than impersonating an upstream release.

Protected HTTP requests with a missing or wrong token return 401. An empty
configured token is supported when native auth is configured; without either
auth mechanism protected routes fail. Static tokens use constant-time equality;
native JWTs validate signatures, expiry, active user and persisted session.
HTTP request bodies are limited to 1 MiB, except image uploads described below. Trusted forwarding chains must begin
at a configured proxy; actual client origin enforces local-only users.

Native onboarding uses **/api/onboarding** and **/api/onboarding/users**.
**/auth/providers**, **/auth/login_flow**, **/auth/login_flow/{flow_id}**, and
**/auth/token** implement the local password/TOTP flow, one-use authorization
codes, client-bound refresh rotation and revocation. Secrets are never placed
in user/list responses. MFA enrollment uses user-bound **auth/setup_mfa**
WebSocket flows with QR SVGs. Native signed GET/HEAD paths are bound to the
issuing user/session and signed path/query; normal permissions still apply.

For graceful shutdown, use **router_with_shutdown(state, receiver)**. Send
**true** through the watch channel alongside the Axum server shutdown signal.
This closes active WebSockets so they do not hold the server drain open.

## REST routes

| Method and path | Behavior |
| --- | --- |
| GET /api/ | API running message |
| GET /api/config | Supplied configuration JSON |
| GET /api/config/config_entries/entry | Public config entries without private data/options |
| DELETE /api/config/config_entries/entry/{entry_id} | Admin removal through the native driver |
| POST /api/config/config_entries/entry/{entry_id}/reload | Admin unload/setup through the native driver |
| POST /api/config/config_entries/flow | Admin native config flow creation |
| GET/POST/DELETE /api/config/config_entries/flow/{flow_id} | Admin current form, next step, or abort |
| GET /api/config/config_entries/flow_handlers | Actual registered native driver domains |
| POST /api/config/config_entries/options/flow | Admin native options flow creation |
| GET/POST/DELETE /api/config/config_entries/options/flow/{flow_id} | Admin options form, next step, or abort |
| POST /api/template | Admin native bounded template rendering |
| GET /api/states | Current states sorted by entity ID |
| GET /api/states/{entity_id} | One state, or 404 |
| POST /api/states/{entity_id} | Set a JSON object with state, optional attributes, and optional force_update; returns 201 when created and 200 when updated, with a Location header |
| DELETE /api/states/{entity_id} | Remove current state; returns 404 when absent |
| GET /api/services | Registered services in Home Assistant's domain/services shape |
| POST /api/services/{domain}/{service} | Call a registered service; raw object fields are service data, optional target is separated, and ?return_response returns changed_states and service_response |
| POST /api/events/{event_type} | Fire an event with an optional JSON object body |
| GET /api/history/period | Query history with filter_entity_id and optional end_time |
| GET /api/history/period/{timestamp} | Query history from an RFC 3339 timestamp, with filter_entity_id and optional end_time |
| GET /api/events | Actual public event listener counts in event/listener_count objects |
| POST /api/image/upload | Authenticated multipart file upload, including read-only users |
| GET/HEAD /api/image/serve/{image_id}/{filename} | Public original or cached thumbnail, with range and cache preconditions |
| GET/HEAD/POST/PUT /api/webhook/{webhook_id} | Public capability callback after trusted client-origin resolution |

History requires **filter_entity_id**, matching the pinned HTTP adapter. A
comma-separated list is accepted. The default start is 24 hours before the
current time; default end is one day after start. Timestamps must be RFC 3339.
History payloads omit private runtime contexts and reporting metadata.

`skip_initial_state`, `significant_changes_only`, `minimal_response` and
`no_attributes` follow the pinned HTTP flags. The native public bus currently
uses wildcard receivers with consumer-side filtering; inventory reports that
actual wildcard count. Private integration-dispatcher receivers are excluded,
and dropped public receivers disappear from the inventory.

REST errors are JSON objects with a message field. Invalid JSON, invalid
inputs, and unsupported query parameters return 400; absent entities/services
return 404; storage failures return a sanitized 500.

REST event injection records the pinned `REMOTE` origin while retaining its
verified user context. Internal events and the pinned WebSocket injection path
use `LOCAL`.

## WebSocket route

Connect to **/api/websocket**, receive **auth_required**, then send
**{"type":"auth","access_token":"..."}**. A valid token receives **auth_ok**; an
invalid token receives **auth_invalid** and the connection closes. Each command
after authentication must have a strictly increasing positive integer id.

Implemented commands include **ping**, **get_states**, **get_config**, **get_services**,
**subscribe_events**, **subscribe_entities**, **unsubscribe_events**,
**fire_event**, **call_service**, native registry CRUD, auth/session/provider
operations, config-entry/core-setting operations, config-entry/flow progress
subscriptions, and native **render_template** subscriptions. Success and failure
responses use Home Assistant-style result envelopes; subscriptions receive
event envelopes. Unknown commands return **unknown_command**. If a receiver
lags, the API sends an **event_stream_lagged** error for each active subscription
and clears those subscriptions.

State reads, controls and administration use native built-in groups. Account,
configuration and mutation operations enforce their command-specific policy;
non-admin event subscriptions use the pinned event allowlist. Every incoming
command and streamed response revalidates its session and permissions. A role
change closes the old connection. Private integration dispatcher traffic is
never exposed on public event subscriptions.

Authentication times out after 10 seconds. A socket accepts messages up to
1 MiB and frames up to 256 KiB. The server permits at most 128 WebSocket
connections and 32 active subscriptions per connection.

The internal notification feed exposes **persistent_notification/get** and
**persistent_notification/subscribe** to ordinary authenticated users. A
subscription receives its ACK before a keyed **current** snapshot, followed by
**added**, **updated**, and **removed** frames. Generic **unsubscribe_events**
removes the receiver. Feed traffic stays on the private dispatcher and does
not appear as public event updates or recorded notification state rows.

The editable zone collection exposes **zone/list** and **zone/subscribe** to
authenticated users; **zone/create**, **zone/update** and **zone/delete** require
an administrator. Updates and deletion identify the item with **zone_id**.
The list and initial/live change arrays contain editable storage zones, while
implicit home and YAML zones remain ordinary states. A subscription ACK
precedes its initial **added** array; later items use **added**, **updated** or
**removed** with **zone_id** and **item**. Generic **unsubscribe_events** removes
the private receiver. The app's **zone.reload** service checks administrator
authority for both internal user contexts and HTTP/WebSocket transport calls.

The person collection provides **person/list** and **person/subscribe** to
authenticated users, and administrator create/update/delete commands. A caller's
**person/update_own_profile** can change their own name and linked editable
person's picture, preserving tracker assignments. Non-null pictures must match
an existing native image's relative serve path; arbitrary URLs are rejected.

Image upload requires one multipart **file** field with a filename and JPEG,
PNG or GIF content type. Files are limited to 10 MiB; the route allows another
64 KiB for multipart overhead. Serving accepts **original**, **256x256** and
**512x512**, with optional thumbnail cache-busting suffixes. Thumbnails contain
the image without cropping or upscaling. Responses stream files and support
HEAD, byte ranges, Last-Modified and ETag preconditions. Uploaded file URLs are
public, as in the pinned source.

**image/list** and **image/subscribe** are authenticated; **image/update** and
**image/delete** require an administrator and identify records with **image_id**.
Creation uses the HTTP upload route. Subscription ACK precedes the initial
**added** array; live **added**, **updated** and **removed** arrays remain on the
private dispatcher. Generic **unsubscribe_events** removes the receiver.

The webhook endpoint accepts registered methods and enforces each callback's
local-only policy using the resolved peer/proxy origin. Unknown callbacks,
remote requests rejected by local policy, malformed JSON and callback errors
return an empty 200; disallowed methods return 405 except HEAD, which remains
silent. Bodies are limited to 1 MiB. JSON, URL-encoded fields and first duplicate
query values are normalized for the callback. Multipart decoding remains open.
**webhook/list** is administrator-only because it exposes callback IDs;
**webhook/handle** follows the pinned authenticated capability-dispatch policy,
including read-only users. Callback payloads travel privately to automations
and are absent from public event subscriptions and recorder entries.

**tag/list** and **tag/subscribe** are readable by authenticated users;
**tag/create**, **tag/update** and **tag/delete** require administrator
authority. Update/delete identify the item with **tag_id**. A subscription ACK
precedes the initial **added** array, followed by private **added**, **updated**
and **removed** arrays containing **tag_id** and **item**. Generic
**unsubscribe_events** removes the receiver. Duplicate creates report
**home_assistant_error**; missing items report **not_found**. Names and
update/delete IDs are strict strings. Create IDs, descriptions and device IDs
accept scalar string coercion. The pinned integration has no **tag/scan**
command or HTTP scan endpoint; actual readers call the native scan manager.

**logger/log_info** returns the numeric effective levels for loaded native
integrations and integrations with current custom settings. It is readable by
authenticated users. **logger/log_level** identifies a module with **module**;
**logger/integration_log_level** identifies an integration with **integration**.
Both require administrator authority, uppercase **level** and **persistence**
(`none`, `once` or `permanent`). Unknown fields and levels fail validation.
The administrator-only HTTP/WebSocket logger services accept case-insensitive
levels. These paths change the same filter attached to the application's real
tracing output layer and publish **logging_changed** with an empty payload.

**system_log/list** and **system_log/subscribe_raw** require an administrator.
The list result is a newest-first array of retained warning-and-higher entries;
messages remain arrays, source is a two-item array, and timestamps are Unix
seconds. The raw subscription accepts an optional string **integration**;
non-strings are
invalid and unknown integrations return **not_found** / **Integration not
found**. It acknowledges before its initial array event, including an empty
array when no backlog is available, then sends coalesced arrays of formatted
log lines at 500 ms intervals. Use **unsubscribe_events** with the subscription
command id to stop it. Raw streams share the ordinary per-connection
subscription limit and are released when unsubscribed, when the socket closes,
or during API shutdown. The optional **system_log_event** bus event remains
disabled by default and follows generic event permissions.

Socket read, send and close future polling runs in the named
**hass_raw_stream_delivery** tracing span so transport-level DEBUG and TRACE
records do not echo into the raw stream. Command handling stays outside that
span, preserving real DEBUG records emitted by **system_log.write**. The
protocol fixture exercises this with logger level **NOTSET**, a WebSocket
DEBUG write, and a 1.2-second quiet-stream window.

## Repairs

When `ApiFeatures.repairs` is provided, the authenticated WebSocket API exposes
`repairs/list_issues`, `repairs/get_issue_data`, and `repairs/ignore_issue`.
These commands follow the pinned authenticated-WebSocket boundary without an
extra admin requirement. List items omit issue data and config-entry IDs;
issue data returned by `get_issue_data` is producer-supplied user-visible data.

Fix flows use `POST /api/repairs/issues/fix` to start and
`/api/repairs/issues/fix/{flow_id}` with GET, POST, or DELETE to read progress,
submit a step, or abort. The current auth model maps `POLICY_EDIT` to an
administrator who is not read-only for flow creation and GET/POST. DELETE
keeps the inherited authenticated-route behavior. Failed administrator checks
return source HTTP 401; the generic read-only mutation gate defers to these
flow handlers. Responses contain the typed
flow progress DTO. A completed create or abort may include terminal `next_flow`
metadata; the child then continues through the existing config-entry routes,
not through the Repairs flow ID. Linked config entries are serialized through
their public fragment; private entry data and options are not returned. Repairs updates use
the ordinary `repairs_issue_registry_updated` event with action, domain, and
issue ID only; there is no Repairs-specific subscription command.

`tests/repairs_protocol.rs` exercises ordinary and read-only WebSocket access,
the minimal public update event, hidden linked issues, restart persistence,
admin/edit flow checks, authenticated DELETE, terminal next-flow metadata,
independent config-options continuation, and public-entry redaction. It also verifies that the no-platform confirmation form
has an empty schema and completes with `{}`. The API stores only the manager's
weak handle; requests fail after the owning Repairs integration is dropped.
The native identity model has no separate `POLICY_EDIT` permission, so the
administrator mapping is the closest available policy boundary.

### System Health reports

`system_health/info` accepts exactly `id` and `type` from an authenticated user,
including ordinary and read-only users. Callback collection precedes its null
acknowledgement, followed by `initial`, deferred `update` events and `finish`.
Provider failures use generic client errors. The one-shot subscription releases
its ID before finish is delivered. Generic unsubscribe, socket close and app
shutdown cancel and join the collector; transferred provider tasks are aborted.
The stream has a four-frame queue and counts toward the existing socket limit.

The app retains the integration owner; API features hold a weak handle. Providers
register explicitly through the native SDK. The core provider reports the native
version and current runtime timezone. The real protocol tests cover role access,
schema and error boundaries, deferred dates, task cancellation, shutdown and
finished subscription removal. See [the provider contract](../../docs/SYSTEM-HEALTH-PORT.md).

### Repairs API checkpoint (2026-11-06)

The protocol adapter and fixture implement the Repairs contract pinned to
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The authenticated-only flow DELETE
is permitted for read-only identities by a route-scoped exception in the
authentication middleware; anonymous requests and other read-only writes
retain the existing policy. API validation passed with:

- `cargo fmt --check -p hass-api`
- `cargo test -p hass-api --test repairs_protocol` (2 passed)
- `cargo clippy -p hass-api --all-targets -- -D warnings`

The workspace-wide 28-crate gate and compiled app-host persistence fixture are
owned by the primary integration pass.

### Native discovery streams

The admin-only commands `ssdp/subscribe_discovery`,
`zeroconf/subscribe_discovery` and `dhcp/subscribe_discovery` use exact `id`/`type`
requests. Each acknowledges with a null result before its initial cache feed.
Zeroconf and DHCP install their update listeners before that acknowledgement;
SSDP retains its source callback replay order. Streams share the 32-subscription
socket limit, recheck authorization on delivery, report bounded-queue overflow,
and remove their exact guards and join owned work on unsubscribe/disconnect.
DHCP sends a single `{add: [...]}` snapshot even when empty, then add-only rows
with formatted uppercase MACs and the original cached hostname/IP values.
These protocol callbacks do not introduce synthetic public Runtime events.

`network` and `network/configure` expose admin adapter selection;
`network/url` reports saved URLs to authenticated callers. See the pinned
[SSDP](../../docs/SSDP-PORT.md), [Zeroconf](../../docs/ZEROCONF-PORT.md),
[DHCP](../../docs/DHCP-PORT.md) and [Network](../../docs/NETWORK-PORT.md) contracts
for command payloads, ownership and remaining compatibility limits.

### USB serial discovery

`usb/scan`, `usb/list_serial_ports` and `usb/list_serial_integrations` are
admin-only request-response commands. The first requests the source's manual
scan route and returns null; an active watcher suppresses that request.
Port listing accepts only optional boolean `include_usage` (default false),
reads a fresh provider/scanner snapshot on the blocking pool, and never starts
discovery or changes its cache. Isolated scanner failures are logged; a base
provider error is an `unknown_error` carrying its concrete message.

Usage includes actual config-entry consumers, typed USB discovery flows and
missing configured ports, with alias resolution and URL query redaction.
Core selector integration names preserve the pinned generated order; custom
overrides can be supplied through the app's usage-provider seam. The current
app has no custom-component loader or Supervisor source and does not invent
their consumers. See [USB source and native contracts](../../docs/USB-PORT.md).

### Hardware records and system status

Authenticated callers can use `hardware/info` and
`hardware/subscribe_system_status`; neither adds an administrator requirement.
Info returns the nullable records from actually loaded native providers, with
failed providers omitted. With no loaded provider it returns `{hardware: []}`.
The configured metrics provider does not infer Supervisor or USB board rows.

Status subscriptions acknowledge with null, then share one five-second
sampler. There is no immediate sample. Bounded lag is a terminal error for
that subscription; other subscriptions survive. Delivery and bounded idle
polls revalidate sessions, so failed metrics cannot retain a revoked stream.
Unsubscribe/disconnect releases the exact sampler guard; the final subscriber
stops further reads. Owner shutdown closes receivers and joins admitted
metrics and hardware-info callbacks, including canceled callers. See the
[Hardware contract](../../docs/HARDWARE-PORT.md) for payloads and platform limits.

### Application Credentials

`application_credentials/list`, `create`, `update`, `delete`, `subscribe` and
`config_entry` require an administrator. The source-compatible admin records
include `client_secret`. `application_credentials/config` is available to
authenticated users and returns the secret-free catalog of loaded providers
and their placeholders. No provider is inferred from static manifest metadata.

Command schemas are validated before administrator checks. Updates remain
unsupported; deletion checks actual config-entry use, including imported
authorization aliases. A subscription acknowledges with null, then sends an
array of `added` items, including an empty array for an empty collection. Live
arrays use `change_type`, `application_credentials_id` and `item`. It shares
the 32-subscription limit and rejects bounded-queue overflow explicitly.
Delivery and idle polls revalidate the administrator session. Unsubscribe,
disconnect and shutdown cancel and join the exact private forwarding task.
Unexpected unsubscribe fields are rejected before any receiver is removed.
See the [credential contract](../../docs/APPLICATION-CREDENTIALS-PORT.md).
