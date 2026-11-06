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
