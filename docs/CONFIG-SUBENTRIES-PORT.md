# Config Subentries Port

## Checkpoint

This work adds Home Assistant's native config-subentry model and lifecycle to
`hass-config`. The source of truth is Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` (`homeassistant/config_entries.py`,
`homeassistant/components/config/config_entries.py`, and the focused sections
of `tests/test_config_entries.py`).

The implementation scope is `crates/hass-config/**` and this document. The
transport layer remains the owner of routes and permissions; native flows and
typed manager methods are the contract for its adapters.

## Pinned behavior and implementation contract

- A subentry has its own persisted ID, type, title, optional unique ID, and
  private `data` map. Parent entry API fragments expose only `num_subentries`;
  the native subentry list exposes ID, type, title, and unique ID, never data.
- A non-null subentry unique ID is unique among the parent's subentries. A
  subentry ID is assigned when a new subentry is created and remains stable
  through update, removal, and Runtime reopen.
- Add, update, and remove first persist the parent with its new subentry set.
  An unchanged update returns `false` and emits no update notification.
  Removal also emits the exact parent/subentry identity after the persisted
  removal. No subentry-specific integration remove callback exists upstream.
- The parent config-entry store migration adds `subentries` with an empty
  default to older entries. The Rust decoder follows that additive migration
  and reads the source list representation while writing stable subentry
  records.
- Native subentry flows are registered by the parent integration driver and
  keyed by a supported subentry type. Creation is routed through that callback;
  reconfiguration includes the real parent and subentry IDs. Unsupported types
  and missing/deleted parents fail instead of inventing subentry identities.
  Reconfigure callbacks can replace title/data or merge `data_updates`, change
  or clear a unique ID, and request a parent reload before the flow aborts.
- Parent setup and unload remain parent-driver operations. Parent removal runs
  the existing unload and remove callbacks before its store deletion. Lifecycle
  observers run only after the resulting mutation is persisted and outside both
  manager locks: parent removal, changed parent disable state, and subentry
  removal are separate events. Parent-removal observers finish registry and
  Repairs ownership cleanup before the `config_entry_changed` removal event is
  dispatched. Disable/enable does not clear issue ownership.

The relevant pinned tests include `test_remove_subentry`,
`test_updating_subentry_data`, `test_update_subentry_and_trigger_listener`,
`test_entry_subentry_duplicate`, `test_subentry_flow`,
`test_update_subentry_and_abort`, `test_update_subentry_reload_and_abort`,
and `test_remove_subentry_cleans_registries_before_update_listener` in
`tests/test_config_entries.py`. The latter verifies child registry cleanup
before update listeners observe the parent.

## Frozen Repairs integration

`ConfigEntries::subentry_exists(entry_id, subentry_id)` validates persisted
ownership by the exact parent/subentry pair. A pointer-owned
`ConfigLifecycleObserver` receives:

- `EntryRemoved { entry_id }` after parent deletion is committed, for
  `RepairsManager::clear_config_entry`;
- `EntryDisabledByChanged { entry_id, disabled_by }` after either disable or
  enable commits, for issue visibility refresh only;
- `SubentryRemoved { entry_id, subentry_id }` after exact child deletion is
  committed, for `RepairsManager::clear_config_subentry`.

The observer events carry no config entry or subentry data. Callers register
their own `Arc`; `ConfigEntries::observe_lifecycle` returns a
`ConfigLifecycleObserverGuard` that unregisters only that exact observer when
dropped. `EntryRemoved` carries the original integration `domain` as a snapshot
because the parent entry is already absent when post-commit registry cleanup
runs. Parent-removal observers run before the removed-entry Runtime event, so
registry and Repairs cleanup is complete when event subscribers see the
removal. Observer failures are reported after the config mutation commits; they
do not roll back persisted config state.

Native flow adapters can query `ConfigEntries::active_flow(flow_type, flow_id)`
for a typed `ConfigFlowInfo` snapshot. The method rejects unknown, completed,
or wrong-kind IDs and returns the actual cached result and owning domain.
Form/abort result metadata retains config context; options flow `handler` is
the owning entry ID, and subentry `handler` is `[entry_id, subentry_type]`.
These fields let Repairs resolve the entry linked to an existing child flow
without inventing an ID or treating an integration domain as an entry ID.

## External API contract

The public transport is limited to the pinned subentry flow and WebSocket
commands. There are no REST CRUD routes for child records.

- `POST /api/config/config_entries/subentries/flow` starts a child flow. Its
  required `handler` is `[entry_id, subentry_type]`; optional `subentry_id`
  selects reconfiguration. The index accepts extra fields but only consumes
  those values. Flow completion and changes run through the registered native
  integration driver's `subentry_flow` callback. The pinned edit/admin gate
  rejects an authenticated non-admin with 401 `Unauthorized`.
- `GET`, `POST`, and `DELETE
  /api/config/config_entries/subentries/flow/{flow_id}` read the current form,
  submit an object (an empty body is accepted), or abort. Creating a flow and
  GET/POST continuation require administrator/edit authority. The inherited
  resource DELETE requires an authenticated identity, including read-only
  identities; the HTTP middleware scopes that exception to this path. Unknown
  flow IDs return 404 `Invalid flow specified`.
- `config_entries/subentries/list` takes `entry_id` and returns
  `subentry_id`, `subentry_type`, `title`, and `unique_id`. It never returns
  private `data`.
- `config_entries/subentries/update` takes `entry_id`, `subentry_id`, and an
  optional string `title`; it returns `null`. Empty titles are accepted, while
  unknown command fields are rejected. Updating only changes title through
  this source endpoint.
- `config_entries/subentries/delete` takes `entry_id` and `subentry_id`,
  removes that exact child, and returns `null`.
- All three WebSocket commands require administrator authority. Missing parent
  IDs return `not_found` / `Config entry not found`; missing child IDs return
  `not_found` / `Config subentry not found`.

The HTTP adapter omits internal flow `context` from the returned form and abort
DTOs, and supplies the source abort metadata. The native manager retains its
typed context in `active_flow` for internal continuation ownership such as
Repairs linkage.

The unsupported-reconfigure message still differs: upstream names the Python
handler class and step, while the native capability error names the subentry
type. Native callbacks can return the typed `InvalidData` error with schema
details; the HTTP adapter maps it to the pinned 400 `{ "errors": ... }` shape.

## Verification status

`cargo test -p hass-config` passes, including the typed active-flow checks,
observer guard drop behavior, and committed lifecycle ordering. The root
integration pass owns workspace-wide verification. A real loopback API test
now exercises flow creation and reconfiguration, generated child IDs, private
data redaction, duplicate unique-ID aborts, sibling preservation, WebSocket
list/update/delete, read-only flow abort, malformed requests, and Runtime
reopen persistence. The focused loopback case, strict all-target API Clippy,
and the frozen 617-test/75-target workspace gate pass. Root wired the transport
handlers and verified the optimized build and example CLI check.
