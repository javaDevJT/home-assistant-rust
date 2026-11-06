# hass-webhook

`hass-webhook` owns the process-local webhook callback registry used by the
native runtime. It has no dependency on `hass-core`; callers register callbacks
that receive a normalized request and return an optional HTTP response.

The registration guard unregisters its callback when dropped. The registry
stores no persisted callback data, and dispatch does not publish a runtime
event. Automation triggers can therefore enqueue request data privately for
the automation engine.

The HTTP adapter is responsible for body bounds, query and form parsing, header
normalization, and trusted client-origin resolution. A direct registration
defaults to `POST` and `PUT`; the trigger integration can choose its own
source-compatible defaults. `GET`, `HEAD`, `POST`, and `PUT` are the only
supported methods. Unknown IDs, remote requests to a local-only callback, and
callback errors return an empty `200`; a disallowed `HEAD` is also an empty
`200`, while other method mismatches return `405`.

`WebhookManager::admin_inventory` includes webhook IDs and must only be exposed
through an admin-authorized API command. It contains registration metadata,
never request headers or payloads.

The native API exposes `GET`, `HEAD`, `POST`, and `PUT` at
`/api/webhook/{webhook_id}` without JWT authentication, after trusted-origin
resolution. HTTP and `webhook/handle` WebSocket requests pass method, first
query/header values, the bounded raw body, parsed JSON or URL-encoded form
fields, and the resolved client origin to the callback. Bodies are limited to
1 MiB. JSON is parsed when the content type contains `json`; malformed JSON is
silently ignored with an empty `200` response. Multipart form decoding is not
implemented.

The automation adapter registers webhook triggers in the same process-local
manager. Multiple triggers that share an ID fan out through the first
registration's method and locality policy. A disabled automation keeps its
configured webhook registered but does not execute an action; a trigger with
`enabled: false` is not registered. Automation unload removes its callbacks.
Webhook requests travel over a bounded private channel and do not become public
runtime events or recorder entries.
