# hass-api

This crate exposes the bounded native runtime through an Axum REST API and a
Home Assistant-style WebSocket API. It does not run Python integrations; REST
state writes update the runtime and do not control devices.

## Router and authentication

Construct **ApiState** with the runtime, optional static administrator token,
and base configuration JSON. **router_with_features** also accepts native auth,
registry, template and config-entry managers, plus a network-origin policy.
Protected requests accept the static token or native session JWT. **/health**
returns **{"status":"ok"}**. WebSocket authentication follows **auth_required**
with an **auth** message. Handshakes expose the native **ha_version** value
**0.1.0-rust** rather than impersonating an upstream release.

Protected HTTP requests with a missing or wrong token return 401. An empty
configured token is supported when native auth is configured; without either
auth mechanism protected routes fail. Static tokens use constant-time equality;
native JWTs validate signatures, expiry, active user and persisted session.
HTTP request bodies are limited to 1 MiB. Trusted forwarding chains must begin
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
| GET /api/events | Returns 501 because the core does not expose an event-listener inventory |

History requires **filter_entity_id**, matching the pinned HTTP adapter. A
comma-separated list is accepted. The default start is 24 hours before the
current time; default end is one day after start. Timestamps must be RFC 3339.
History payloads omit private runtime contexts and reporting metadata.

REST errors are JSON objects with a message field. Invalid JSON, invalid
inputs, and unsupported query parameters return 400; absent entities/services
return 404; storage failures return a sanitized 500. The event inventory route
returns 501 with a message explaining the missing core capability.

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
