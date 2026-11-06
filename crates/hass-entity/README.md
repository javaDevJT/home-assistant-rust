# Native entity lifecycle

`hass-entity` provides the shared lifecycle manager used by Rust providers. One
`EntityLifecycle` owns the shared `Runtime`, `Registries` view, and registry
event worker. Providers register stable identities, keep weak-backed
`EntityHandle` leases, and publish `EntitySnapshot` values. Handles resolve the
current entity ID from the registry; providers do not maintain a second ID
cache.

## Identity and provider ownership

`EntitySpec` describes the Home Assistant domain and registry platform, an
optional registry `unique_id`, config entry/subentry and device/area links, and
provider metadata. A registry identity is `(domain, platform, unique_id)`.
Without a unique ID, the manager allocates and persists an entity ID under the
`(provider_id, provider_key)` receipt.

`with_enabled_by_default(false)` sets a newly created registry entry's
`disabled_by` to `integration`. Existing or restored entries retain user
enablement choices, including across authoritative provider refreshes.

`register_batch` validates a batch before registering it and returns handles in
input order. `register_provider_batch` additionally reconciles one provider
namespace against its complete active configuration. `provider_id` is an
ownership namespace independent of registry `platform`; separate config
entries sharing a platform must use distinct provider IDs. The manager persists
registry identity receipts by provider so an empty configuration only cleans
states that namespace previously owned. Providers should pass the full current
batch on each reconciliation.

Generated-ID receipts and registry receipts are persisted through `Runtime`
metadata. The registry's own provider batch is validated and committed as a
registry transaction. The receipt metadata and entity state writes are separate
Runtime operations: there is no cross-store transaction. A pending union of old
and new registry receipts is saved before registry mutation. A pre-commit
failure restores the old receipt; a post-commit failure retains the union so a
later empty or updated provider batch can reconcile committed identities.

## Provider metadata and user overrides

`ProviderEntityInfo` carries provider-owned original name, icon, device class,
entity category, unit, supported features, translation key, capabilities, and
related metadata. A refresh changes those provider values while preserving the
registry entity ID, aliases, disabled/hidden state, and user overrides such as
name, icon, and device class. State attributes combine provider values with the
current registry overrides; unavailable and restored states keep the
appropriate registry metadata.

## State, release, and shutdown

`EntitySpec::with_unrecorded_attributes` attaches a `StateRecordingPolicy` to
every publication, guarded rename, restore and release write for that lease.
It projects attributes only for state history. Current state and public or
stored state-change events keep their complete attributes. MemoryStore and
SqliteStore implement this projection; another Store must override the additive
`put_state_with_history` hook to honor it.

`publish` checks the current registry status before writing. Disabled or
removed entities do not publish, and a removed entity is never recreated by a
publish call. Lifecycle status changes are available through the handle's
coalescing watch receiver.

The manager tracks the last state it wrote for each active handle. Cleanup uses
`Runtime::remove_state_if_unchanged`, which compares the complete state snapshot
under the Runtime write-order lock. If another writer replaced the state,
cleanup leaves that replacement intact. Rename writes to the new ID only while
it remains absent under the same lock, then removes the old state only if it
still matches the last provider-owned snapshot. A target collision leaves both
states intact; an error removing the old state triggers a compare-and-remove
rollback of the new state.

Releasing a registered, enabled entity saves its last provider snapshot and
writes `unavailable` with `restored: true` only while the current state still
matches the provider's last write. Releasing a generated, unregistered entity
saves its snapshot and removes its state only if unchanged. A later provider
registration can restore that snapshot. Disabled or removed entities clear
their owned state when it still matches.

The last clone of an `EntityHandle` releases its lease. Explicit release
propagates storage errors and can be retried by the caller. If a dropped lease
cannot be released, the manager marks its handle terminal and queues owned
cleanup with exponential backoff. A new lease for the same identity supersedes
that pending cleanup, so an old lease cannot remove the new provider's state.
Persistent errors are reported by `shutdown` after one final retry.

`EntityLifecycle::shutdown` cancels and joins the registry worker, saves
restore metadata for current snapshots, and leaves live Runtime states intact
for application restart. Provider unload should release its handles while the
manager is open; application shutdown is a separate operation.

## Scope and limits

This crate supplies identity, registry observation, state publication, restore
receipts, and lease cleanup. Providers still own their polling or action work
and must stop or join that work when unloading. Runtime state and registry
receipt writes use their normal error paths, but they are not one atomic
transaction. After an abrupt process failure before a release or graceful
shutdown records the last owned observation, stale cleanup can only compare
against the state observed during reconciliation.

Lifecycle removal and unload behavior was checked against Home Assistant Core
revision `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, especially
`homeassistant/helpers/entity.py` and
`homeassistant/helpers/entity_platform.py`.
