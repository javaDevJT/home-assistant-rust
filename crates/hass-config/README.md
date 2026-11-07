# hass-config

This crate owns persistent Home Assistant config entries, native driver
lifecycle calls, config and options flows, and the core location/unit/URL
settings used by API adapters. It requires registered native driver code for
integration setup; an absent driver is stored as `setup_error`, never `loaded`.

## Driver contract

Create one cloneable manager from the shared Runtime and register drivers before
loading entries:

```rust,ignore
let entries = ConfigEntries::new(runtime)?;
entries.register_driver(Arc::new(MyIntegrationDriver::new()))?;
entries.setup_entry(&entry_id)?;
```

Implement `ConfigEntryDriver` for each native integration. Its synchronous
`setup`, `unload`, `config_flow`, `options_flow`, `migrate`, and `remove`
callbacks execute outside the manager's metadata locks. Drivers that need async
IO can capture a Tokio handle and block on startup or shutdown from the root
adapter's `spawn_blocking` task. `SetupOutcome::RetryAfter` records a retry
deadline; the owner calls `retry_due_entries(now)` to process due retries.

Hosts that register drivers temporarily retain their `Arc<dyn ConfigEntryDriver>`
and call `unregister_driver(&driver)` after stopping work and unloading entries.
Removal checks pointer identity, so an old host cannot remove a replacement
driver. This operation does not invoke unload callbacks. It also allows startup
rollback to remove only callbacks installed by the failing host.

The manager exposes typed create, get/list, setup, unload, reload, retry,
disable, removal, options update, reauth, config-flow, and core-config methods.
Config-flow HTTP adapters can call `init_config_flow_with_input` and
`submit_flow`; options adapters use `init_options_flow_with_input` and
`submit_flow`. Flow form/create/abort values use Home Assistant's data-entry
flow result fields. The WebSocket dispatcher accepts the upstream entry
commands (`config_entries/get`, `get_single`, `update`, `disable`, `subscribe`,
flow progress/subscribe) and the core config get/update commands.

`ConfigEntries::active_flow(flow_type, flow_id)` returns a typed
`ConfigFlowInfo` snapshot only when the requested flow type matches the active
session. It includes the cached public result and owning integration domain.
Active form/abort results retain config context (including a linked
`context.entry_id` when present); options flow results use the owning entry ID
as `handler` and in `context.entry_id`. Subentry results use
`handler: [entry_id, subentry_type]`.

Native subentries use persistent typed `ConfigSubentry` records and the
`create_subentry`, `get_subentry`, `list_subentries`, `update_subentry`, and
`remove_subentry` manager methods. Drivers register supported subentry types
and implement `supported_subentry_types` plus `subentry_flow` for native flow
steps. Adapters start creation/reconfigure with `init_subentry_flow` and
continue using `submit_flow`; unsupported types fail explicitly.
`ConfigEntries::subentry_exists` validates an exact persisted parent/child
identity. Pointer-owned `ConfigLifecycleObserver`s receive committed parent
removal, parent disable/enable, and child removal events after manager locks
are released. `observe_lifecycle` returns a `ConfigLifecycleObserverGuard` that
unregisters only its exact observer when dropped. Parent removal events carry
the removed entry's original `domain`, which remains available after the
entry is gone. Parent and child cleanup precede their removal/update
announcement; disable visibility refresh follows the entry update. See
[the pinned-source and Repairs contract](../../docs/CONFIG-SUBENTRIES-PORT.md).

## Persistence and secret handling

Entries, private `data`/`options`, lifecycle/retry/reauth/reload state, and core
settings are stored under the Runtime metadata key `hass.config_entries.v1`.
Writes persist before the manager publishes its in-memory update. Entry IDs and
per-domain unique IDs are checked for collisions. Schema upgrades run through
the driver's migration callback and cannot downgrade an entry.

Ordinary API fragments from `handle_ws`, config-flow completion results, and
Runtime config-entry events omit entry `data` and `options`. `ConfigEntry`
debug output also redacts those maps. Trusted driver callbacks and typed
`ConfigEntries::get_entry`/`list_entries` results retain the private values.

## Current limits

- Flow sessions and registered driver objects are process-local. On restart,
  previously loaded entries return to `not_loaded` until the root registers
  drivers and calls setup.
- Retry deadlines are persisted, but retry scheduling is owned by the caller
  through `retry_due_entries`; this crate does not create detached timer tasks.
- `config_entries/subscribe` and `config_entries/flow/subscribe` return the
  upstream `null` command result. Entry changes are persisted and published as
  `config_entry_changed` Runtime events; the WebSocket transport owns delivery
  of subscription events.
- Core configuration validates the location, IANA time zone, unit system, and
  optional HTTP(S) URLs. It seeds from YAML only until a persisted API update
  exists, and applies the saved time zone to Runtime.
- Config subentry `data` is private and omitted from ordinary listing fragments.
  Older config records that predate subentries load with an empty subentry set.
- Lifecycle observer errors are reported after the config mutation is durable;
  observers run outside manager locks and cannot roll back that committed
  mutation. Parent and child removals atomically store a pending cleanup record
  with the config deletion. Records contain only a stable UUID and the typed
  parent/child identity needed by cleanup observers; they never copy entry,
  subentry, or option data. Parent cleanup supersedes queued child records for
  that entry. After installing the real cleanup observers, the host calls
  `ConfigEntries::replay_pending_cleanup` to retry records left by a callback
  failure or restart. Registration and manager construction do not replay.
  Successful cleanup durably acknowledges only its exact UUID; an ack-write
  failure leaves the record for another idempotent retry. Replay delivers only
  lifecycle cleanup callbacks and does not replay public
  `config_entry_changed` Runtime events.
- Child cleanup may reenter the manager and remove its parent. The following
  update reads the current parent and checks that snapshot under the state
  lock before publishing; a removed or superseded snapshot is skipped. Driver
  capability callbacks stay outside that lock. This keeps a stale `updated`
  event from following the parent's `removed` event.
- Successful core updates emit `core_config_updated` after persistence, with
  accepted request fields and no WebSocket envelope fields. Invalid or
  unsupported fields produce no update event. The app uses this event to
  refresh the home zone and clock sensors from the latest saved core settings.
