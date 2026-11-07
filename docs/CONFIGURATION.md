# Native configuration loading

The executable validates the complete resolved configuration before opening a
database or starting integrations. `hass --config configuration.yaml --check`
runs the same loader and typed validators used for startup.

The loader resolves `!include`, `!include_dir_list`, `!include_dir_named`,
`!include_dir_merge_list`, and `!include_dir_merge_named` relative to the file
containing the tag. Directory loading is deterministic and skips hidden files
and `secrets.yaml`. Cycles, unknown tags, conflicting package keys, malformed
files, and missing dependencies fail explicitly.

```yaml
homeassistant:
  time_zone: America/New_York
  packages: !include_dir_named packages
automation: !include automations.yaml
script: !include scripts.yaml
mqtt:
  host: !env_var MQTT_HOST localhost
  password: !secret mqtt_password
```

`!secret` searches the requesting file's directory and then its parents up to
the configuration root. Nested `secrets.yaml` files override parent values.
Secrets are YAML values; their expected type is checked by the integration's
validator. `!env_var NAME default value` supplies a string default when the
variable is absent. Required missing values are errors.

`homeassistant.packages` merges integration sections into the root before
typed validation. List-based native sections append entries; mapping sections
merge with duplicate conflicts rejected. The resolved package source is kept
by the YAML loader and removed at the typed application boundary to avoid
storing a second secret-bearing configuration object.

Each file is limited to 1 MiB, a load to 8 MiB and 1,024 files, and directory
traversal to 16,384 entries. Include depth is limited to 64 and YAML value
depth to 128. File parse errors report filenames and positions without source
snippets. Typed decode failures retain the field path without quoting resolved
secret values.

Supported integration options are defined by the strict native configuration
types and each crate's README in the [project index](../README.md). Unknown
integrations and options fail validation. Loader support does not imply that
every integration referenced by an upstream configuration has been ported.

The native zone component always creates `zone.home` from the accepted core
name and coordinates. A `zone:` list, one zone mapping, or an empty section
adds YAML zones. Editable zones use the authenticated `zone/*` WebSocket
collection separately from YAML definitions. The administrator-only
`zone.reload` service resolves the current YAML and its includes again and
replaces only the YAML zone collection. It does not reconfigure other domains.
Saved `config/core/update` settings update the home zone and take precedence
over startup YAML on subsequent restarts.

The native tracker provider accepts `device_tracker:` settings with
`consider_home` in seconds, `track_new_devices`, and an optional `devices` list.
It reads an existing `known_devices.yaml` beside the main configuration through
the same secret/include loader and never rewrites that file. Known-device
entries default to `track: false`; reports for newly observed devices follow
`track_new_devices`. Configured and imported device identities must be unique.
Scanner platform loading remains a separate porting gap.

```yaml
person:
  - id: resident_unique_id
    name: Resident
    device_trackers: device_tracker.phone
device_tracker:
  consider_home: 180
  track_new_devices: true
```

A person's `id` is its unique identity, while the shared entity registry assigns
the state ID from its name, such as `person.resident`. Startup restores saved
person fields, starts the providers, then selects the current trackers before
running `homeassistant_started` automations. The administrator-only
`person.reload` service rereads only YAML people; editable people use the
authenticated `person/*` collection. Ordinary users can edit their own name
and set or clear their linked editable person's picture. Picture references
must name an existing native image uploaded through `POST /api/image/upload`;
arbitrary URLs are rejected. JPEG, PNG and GIF uploads are limited to 10 MiB.
Files live under the configuration directory's `image/` folder, with metadata
in the native database. Keep both when moving this installation's data.

Native time/date sensors use the legacy `sensor:` platform form:

```yaml
sensor:
  - platform: time_date
    display_options: [date, time, date_time_iso, time_utc]
    entity_namespace: clock
    scan_interval: "00:01:00"
```

The supported options are `time`, `date`, `date_time`, `date_time_utc`,
`date_time_iso`, `time_date` and `time_utc`; `beat` is rejected. The namespace
prefixes generated entity IDs. `scan_interval` is validated as a platform
setting; these non-polling sensors retain their native minute or local-midnight
schedule. They read the actual clock and follow saved/live Home Assistant
timezone settings. Local ISO state is timezone-naive, as in the pinned source.

The native `time_date` and `uptime` config-entry flows are also available through
the authenticated configuration API. Time/date entries select one option;
uptime allows one entry and accepts no fields. Uptime is the fixed UTC timestamp
captured at provider setup, refreshed on reload/restart. It is not an elapsed
duration or operating-system boot time. See the
[system sensor contract](../crates/hass-system-sensors/README.md).

Native logging controls the actual tracing output layer:

```yaml
logger:
  default: info
  logs:
    homeassistant.components.mqtt: debug
  filters:
    hass_mqtt:
      - '^Repeated fixture message'
```

Configuration and service levels accept any letter case, without trimming.
Supported names are `NOTSET`, `DEBUG`, `INFO`, `WARNING`/`WARN`, `ERROR` and
`CRITICAL`/`FATAL`. Regex filters suppress matching messages from the exact
logger name; they do not propagate to child logger records. Invalid names,
levels and regexes fail configuration validation before opening the database.
The native regex engine supports its documented Rust syntax; Python lookaround
and backreference patterns are rejected. Ordinary Rust error records remain
`ERROR`; explicit native critical records preserve level `50` and the
`CRITICAL` label in filtering, console output and capture. Dynamic records from
the installed LogTracer bridge use their actual logger names and source fields.

Administrator services `logger.set_level` and `logger.set_default_level` apply
session settings. Authenticated users can read `logger/log_info`; administrator
WebSocket controls accept uppercase level names and persistence `none`, `once`
or `permanent`. `once` survives one subsequent start, and `permanent` remains
until replaced or cleared with `NOTSET`. The app maps known integration names
to distinct native targets, including timer, schedule, scene and clock-provider
output. Explicit Rust targets and their `::` hierarchy are also supported.

`sun:` accepts null or an empty mapping and imports the singleton Sun entry;
the empty native Sun configuration flow can add it through the API. The
observer comes from the persisted `homeassistant` latitude, longitude,
elevation and time zone. Core configuration updates recalculate the Sun
without restarting the process. Six timestamp sensors are enabled by default;
the elevation, azimuth and rising sensors start disabled in the entity registry.
Enabling one immediately publishes its current value. The fixed `sun.sun`
attributes remain available live while its nine changing attributes are
excluded from state history. See the [Sun contract](../crates/hass-sun/README.md).

System Log captures actual emitted records alongside console output:

```yaml
system_log:
  max_entries: 50
  fire_event: false
```

The retained warning/error list is process-local; zero entries disables that
list while raw streaming continues. Writes and clear use the ordinary service
authorization policy. The list and raw stream WebSocket commands require an
administrator. Raw subscriptions receive an initial array and later batches,
and are released by the generic unsubscribe command or connection close. With
no configured file/registered reader, initial backlog is empty. An installed
native integration can filter a stream even when it has no configured entry.
`fire_event` optionally publishes each captured warning/error occurrence;
bounded dispatch failures are surfaced rather than treated as persisted output.
See the [source contracts](SYSTEM-LOG-PORT.md) for deduplication, queue bounds
and the remaining LLM consumer scope.

System Health exposes `system_health/info` to authenticated users, including
ordinary and read-only users. Its one-shot report contains the native core
version and current saved timezone, with explicit native providers able to add
fields. `system_health:` is optional. Source-compatible YAML accepts all section
values; truthy parameters are ignored with a generic startup/check diagnostic.
Parameter values are kept out of that diagnostic. See the
[provider and stream contract](SYSTEM-HEALTH-PORT.md).

`tag:` accepts null or an empty mapping. Tags are editable persistent collection
items managed by the authenticated `tag/*` WebSocket commands; names live in
the shared entity registry. Native reader integrations call `TagManager::scan`,
which publishes `tag_scanned` and updates the timestamp entity. The automation
trigger accepts `trigger: tag`, a scalar or list `tag_id`, and an optional
scalar or list `device_id`. No scan HTTP route or scan service is defined by
the pinned integration. See the [tag contract](../crates/hass-tag/README.md).
