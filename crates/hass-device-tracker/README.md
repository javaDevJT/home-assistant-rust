# Native device tracker

This crate implements the runtime-owned `device_tracker.see` service and the
state provider needed by the native Home Assistant process. Behavior is based
on the pinned upstream Home Assistant source revision
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, especially
`homeassistant/components/device_tracker/{__init__,entity,legacy,const}.py` and
`tests/components/device_tracker/`.

## Runtime API

Start the provider after the zone integration:

```rust,ignore
let tracker = DeviceTrackerIntegration::start(runtime, zones.manager(), config).await?;
```

`TrackerManager::see(update, context)` accepts typed reports. The owned
`device_tracker.see` service accepts `mac` or `dev_id`, plus `host_name`,
`location_name`, `gps`, `gps_accuracy`, `battery`, `attributes`, `source_type`,
and `consider_home`. Its two upstream iOS compatibility fields, `hostname` and
`battery_status`, are accepted and discarded. The callback preserves the
caller context and uses the runtime's normal service authority; upstream
registers this service without an integration-level admin check.

GPS coordinates use `ZoneManager` membership queries. Every containing zone,
including passive zones, is reported in `in_zones`; state uses the smallest
active zone, or `home` / `not_home` when appropriate. A non-GPS report remains
`home` until its configured `consider_home` period expires; expiry clears stale
coordinates. GPS-source coordinates follow upstream's persistent position
behavior. Metadata and ownership context IDs are validated before changes, and
cleanup removes only states that still match this provider's saved context.

## Configuration boundary

`TrackerConfig` defaults to `consider_home = 180s` and `track_new_devices = true`.
`TrackerDeviceConfig` maps the known-device fields `dev_id`, `name`, `mac`,
`track`, `consider_home`, `icon`, and `picture`. A non-null `gravatar` is
ASCII-lowercased, MD5-hashed locally, and converted to the upstream size-80
`wavatar` picture URL; it takes precedence over `picture`. This makes no
network request. `parse_known_devices(Value)` is
a pure parser for an application-loaded, read-only `known_devices.yaml` map; it
does not access or modify files. It ignores only `vendor` and `hide_if_away`, as
upstream does.

This initial slice does not implement scanner platforms, platform
discovery/loading, registry lifecycle, or writes to `known_devices.yaml`.
Unknown devices created by `see` are persisted in this provider's private
runtime metadata instead of being written to YAML. The app owns file loading,
config/API wiring, registry integration, and service authorization.
