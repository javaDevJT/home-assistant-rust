# `hass-tag`

Native tag collection and scan state for the Rust Home Assistant runtime.

`TagIntegration::start` takes the shared `Runtime`, `Registries`, and
`EntityLifecycle`. It restores the collection, registers `tag` entities through
the registry/lifecycle owners, and publishes their state. `TagManager` is a
weak handle for provider and API callers; shutting down or dropping the
integration releases its entity leases and closes retained handles.

`TagManager::scan` is the native callback for a real tag reader or other
provider. It fires the public `tag_scanned` event first, with the caller's
context and the currently registered name, then persists the scan time and
truthy device ID and updates entity state. A storage failure does not retract
the already-fired public event. The collection's private change signal is
separate from the public event bus. The automation engine matches scalar or
list tag/device filters, expands pre-migration composite device IDs through the
shared registry, and preserves scan context in action execution.

Collection rows preserve source insertion order across restore, list, create,
update, delete, scan, and persistence. Names are owned by the shared entity
registry. Loading migrates version 1 collections by moving legacy names to the
registry and removing the duplicate `tag_id` field. Timestamps retain naive
datetime values and explicit fixed offsets, matching the pinned source's
datetime validator and ISO serialization.

Create requests with a missing or empty `tag_id` generate a UUID. Update and
delete accept any string ID and report a missing item through the collection's
not-found error.

Failed CRUD operations compensate collection, registry and entity changes.
Create preserves a preexisting live identity or tombstone; delete restores the
exact removed identity if later cleanup fails. Name compensation compares the
operation's applied value atomically, preserving a concurrent user edit.
These writes span separate Runtime operations, so observers can see transient
registry/state events. Compensation failures are included in the returned
error; aborted operations emit no private collection delta.

`crates/hass-api/src/tags.rs` handles source collection CRUD commands. The
API wiring supplies ordinary-user list/subscribe access and admin-only
mutation. `crates/hass-api/tests/tags_protocol.rs` exercises actual authenticated
WebSocket CRUD, permission errors, initial/live private deltas and scalar
coercions. Names and update/delete IDs remain strict strings; create IDs,
descriptions and device IDs use the pinned scalar-to-string conversion.

There is no tag QR/HTTP scan endpoint, `tag.scan` service, or `tag/scan`
WebSocket command in the pinned Home Assistant source. This crate adds none; a
provider must call `TagManager::scan` directly. NFC hardware and device setup
are outside this crate.

The entity's recording policy omits `tag_id` from state history while retaining
it in current state and live state-change events. `tests/sqlite.rs` proves that
split through real tag scans and a database reopen. Native stored event rows
currently retain complete state-change events; upstream recorder routes those
changes to state rows rather than its generic event table, so that storage
layout parity remains open. Public `tag_scanned` data is retained in full.

The compiled-process fixture in `crates/hass/tests/end_to_end.rs` verifies tag
CRUD, the actual registry identity, event-triggered automation, hard restart,
name persistence and deletion cleanup. It does not manufacture NFC activity.

Behavior was audited against Home Assistant Core
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, especially
`homeassistant/components/tag/{__init__,entity,trigger}.py`,
`homeassistant/helpers/collection.py`, and `tests/components/tag/`.
