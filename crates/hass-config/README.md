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

The manager exposes typed create, get/list, setup, unload, reload, retry,
disable, removal, options update, reauth, config-flow, and core-config methods.
Config-flow HTTP adapters can call `init_config_flow_with_input` and
`submit_flow`; options adapters use `init_options_flow_with_input` and
`submit_flow`. Flow form/create/abort values use Home Assistant's data-entry
flow result fields. The WebSocket dispatcher accepts the upstream entry
commands (`config_entries/get`, `get_single`, `update`, `disable`, `subscribe`,
flow progress/subscribe) and the core config get/update commands.

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
