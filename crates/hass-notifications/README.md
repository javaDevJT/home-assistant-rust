# hass-notifications

Native, in-memory port of Home Assistant Core's `persistent_notification`
integration, pinned to `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.

## Supported behavior

`NotificationIntegration::start(runtime)` registers the three upstream services:

| Service | Accepted data |
| --- | --- |
| `persistent_notification.create` | Required string `message`; optional string `title` and `notification_id` |
| `persistent_notification.dismiss` | Required string `notification_id` |
| `persistent_notification.dismiss_all` | Empty object |

An omitted create ID gets a random 32-character hexadecimal ID. Creating an
existing ID replaces its message, title, and `created_at` while preserving its
position in the list. Dismissing an unknown ID is a no-op. `dismiss_all` emits
one `removed` update even when the list is empty. Empty strings are valid string
values; absent required fields, non-string values, and unknown fields are
rejected before the feed changes.

The public object has `message`, `notification_id`, nullable `title`, and UTC
`created_at` fields. `NotificationManager::list()` returns objects in insertion
order. `snapshot_and_subscribe()` atomically returns that list and a receiver
for private Runtime signals. The signal is named
`persistent_notifications_updated`; its data is:

```json
{
  "type": "added",
  "notifications": {
    "notification-id": {
      "message": "Example",
      "notification_id": "notification-id",
      "title": null,
      "created_at": "<RFC 3339 UTC timestamp>"
    }
  }
}
```

Update types are `current`, `added`, `updated`, and `removed`. Mutation signals
contain only the affected notifications; `dismiss_all` contains all removed
entries. They go through Runtime's private dispatcher and do not enter public
event history or the public event bus. A WebSocket adapter can implement
`persistent_notification/get` by returning `list()` and
`persistent_notification/subscribe` by sending its success result before the
`current` event, then forwarding matching signal events. The source commands
are available to any authenticated WebSocket connection and have no
administrator-only check. Generic `unsubscribe_events` uses the original
subscription command ID.

The service handler preserves strings and coerces JSON boolean/numeric scalars
through the pinned `cv.string` rules; null, arrays and objects are rejected.
It does not render Jinja
templates itself; the automation/script caller renders templated action data
before calling a service.

## Limits and lifecycle

Messages are limited to 64 KiB, titles to 8 KiB, IDs to 512 bytes, the feed to
512 entries, and the encoded list to 900 KiB. Every update payload is checked
against Runtime's 1 MiB private-signal limit before mutation. Rejected calls do
not modify or evict existing notifications.

Notifications are process-local and are not written to Runtime metadata or the
Store. Starting a new integration instance creates an empty feed, including
when the same Store is reused. `shutdown()` and `Drop` deactivate manager
clones, unregister only this instance's services, and release the integration's
Runtime reference. The crate does not own WebSocket connections or subscriptions;
the API layer owns their transport and disconnect cleanup.
