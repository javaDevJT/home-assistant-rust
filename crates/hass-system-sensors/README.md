# System sensors

This crate ports the pinned Home Assistant `time_date` and `uptime`
integrations from commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.

## `time_date`

The accepted options are `time`, `date`, `date_time`, `date_time_utc`,
`date_time_iso`, `time_date`, and `time_utc`. The pinned
`homeassistant/components/time_date/const.py` excludes `beat`; its lingering
translation in `strings.json` does not make it valid. The strict YAML and flow
parsers in this crate reject `beat` and unknown integration-specific fields.
`TimeDateYamlConfig::parse` takes the integration-specific fields after the
surrounding YAML loader has validated shared sensor platform fields. YAML
defaults to `["time"]` and accepts either one string or a list, preserving the
supplied list order. The shared `entity_namespace` field is retained for each
platform and prefixes generated entity IDs. The shared `scan_interval` field
accepts Home Assistant time-period values; time/date entities are non-polling,
so they continue to update on their own minute or local-midnight schedule. The
config flow requires one string and a configured Home Assistant timezone.

The root app's `SystemSensorYamlPlatform` adapter in
`crates/hass/src/system_sensors.rs` deserializes a tagged sensor block and
requires `platform: time_date`. The root loader validates shared platform
fields before passing the platform tag and integration-only fields to this
type. Unsupported tags and unknown time/date fields return explicit errors.

`format_time_date_state` formats an actual UTC instant in the supplied Home
Assistant timezone. UTC variants stay UTC. `date_time_iso` is a timezone-naive
local ISO value with seconds set to `00`, matching the upstream parse/format
path. `next_time_date_update` follows the upstream scheduling rule: date sensors
target the next local midnight; every other option targets the next UTC minute
boundary. Local midnight calculations account for DST and do not assume every
local day has 24 hours.

`TimeDateSensor::snapshot` reads `Utc::now()` and the caller supplies the
current `Runtime::time_zone()` result. With the `runtime` feature,
`TimeDateIntegration` registers descriptors through the lifecycle's
provider-scoped batch API, publishes the initial state, schedules minute or
local-midnight updates, and listens for `core_config_updated` to refresh
immediately and recompute deadlines. YAML callers supply a stable provider ID;
config-entry providers are scoped automatically as `time_date:<entry_id>`. An
empty config reconciles that provider's stale entities, and shutdown/drop
release only its namespace. Entity registry IDs are assigned by the shared
lifecycle; this crate supplies the upstream entity metadata and config-entry
unique ID behavior.

The root app's `SystemSensors` host combines YAML time/date blocks in their
source order under the stable `time_date:yaml` provider while preserving each
platform's namespace, registers the
time/date and uptime config-entry flows, restores entries, and retries
temporary setup failures. Its `shutdown` stops retry work, unloads loaded
entries, and joins the owned time/date task.
Drop closes the host, cancels its retry task and removes only its own driver
callbacks. Explicit async shutdown is required to unload entries and join
in-flight work before releasing the shared runtime.

## `uptime`

The user flow accepts no fields, creates an entry titled `Uptime`, and allows a
single config entry. `UptimeSensor::new` captures `Utc::now()` once at provider
setup, following `homeassistant/components/uptime/sensor.py:32`; `at` accepts an
explicit timestamp for deterministic tests and host setup coordination. The
state remains that setup-time UTC timestamp and never advances. It represents
the Home Assistant provider setup time, not operating-system boot time or an
elapsed duration.

The sensor uses device class `uptime`, is non-polling, has no unit, uses the
config entry ID as its unique ID, and identifies a service device by
`("uptime", entry_id)`. The host registers the descriptor's service device
and passes its ID to `UptimeIntegration`; the integration scopes its provider
as `uptime:<entry_id>`. This crate does not allocate an entity or device ID.

## Upstream coverage

Focused tests use the pinned component cases from
`tests/components/time_date/test_sensor.py` and
`tests/components/time_date/test_config_flow.py` for the seven formats,
New York timezone conversion, UTC minute boundaries, local midnight, and DST
start/end, including nonexistent and ambiguous local midnights. Uptime setup
time, flow defaults, state format, entity identity, and
device metadata follow `tests/components/uptime/test_sensor.py`,
`tests/components/uptime/test_config_flow.py`, and the pinned
`tests/components/uptime/snapshots/test_sensor.ambr`.

The crate inherits package metadata and dependencies from the root workspace.
The app owns workspace membership and provider/config-flow wiring.
