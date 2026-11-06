# hass-api

This crate exposes the bounded native runtime through an Axum REST API and a
Home Assistant-style WebSocket API. It does not run Python integrations; REST
state writes update the runtime and do not control devices.

## Router and authentication

Construct **ApiState** with the runtime, a non-empty access token, and a JSON
configuration value, then pass it to **router(state)**. HTTP requests under
**/api/** require **Authorization: Bearer <token>**. **/health** is unauthenticated
and returns only **{"status":"ok"}**. WebSocket authentication is performed with
an **auth** message after the server sends **auth_required**; the handshake omits
**ha_version** because this implementation is not a Home Assistant release.

Protected HTTP requests with a missing or wrong token return 401. An empty
configured token rejects all protected HTTP requests and WebSocket upgrades
with 500. Token values are compared with **subtle** constant-time equality and
are never logged. HTTP request bodies are limited to 1 MiB.

For graceful shutdown, use **router_with_shutdown(state, receiver)**. Send
**true** through the watch channel alongside the Axum server shutdown signal.
This closes active WebSockets so they do not hold the server drain open.

## REST routes

| Method and path | Behavior |
| --- | --- |
| GET /api/ | API running message |
| GET /api/config | Supplied configuration JSON |
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

History requires **filter_entity_id** because the runtime can query history for a
known entity but cannot enumerate entity IDs that exist only in retained
history. A comma-separated list is accepted. The default end is the current
time, and the default start is 24 hours before that end. Only **filter_entity_id**
and **end_time** are supported; other history query parameters return 400.
Timestamps must be RFC 3339, and the start must not be later than the end.

REST errors are JSON objects with a message field. Invalid JSON, invalid
inputs, and unsupported query parameters return 400; absent entities/services
return 404; storage failures return a sanitized 500. The event inventory route
returns 501 with a message explaining the missing core capability.

## WebSocket route

Connect to **/api/websocket**, receive **auth_required**, then send
**{"type":"auth","access_token":"..."}**. A valid token receives **auth_ok**; an
invalid token receives **auth_invalid** and the connection closes. Each command
after authentication must have a strictly increasing positive integer id.

Implemented commands are **ping**, **get_states**, **get_config**, **get_services**,
**subscribe_events** (optional event_type, omitted or * subscribes to all),
**unsubscribe_events**, **fire_event**, and **call_service**. Success and failure
responses use Home Assistant-style result envelopes; subscriptions receive
event envelopes. Unknown commands return **unknown_command**. If a receiver
lags, the API sends an **event_stream_lagged** error for each active subscription
and clears those subscriptions.

Authentication times out after 10 seconds. A socket accepts messages up to
1 MiB and frames up to 256 KiB. The server permits at most 128 WebSocket
connections and 32 active subscriptions per connection.
