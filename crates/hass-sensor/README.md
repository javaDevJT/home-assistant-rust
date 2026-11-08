# hass-sensor

`hass-sensor` is a small native sensor descriptor and reading projector for
Home Assistant's `sensor` domain. It builds an `hass_entity::EntitySpec` from a
stable descriptor and projects each typed reading into an `EntitySnapshot`.
`EntityLifecycle` remains responsible for registry identity, rename/disable
handling, publication, and provider unload.

The behavior target is Home Assistant Core
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf),
especially `homeassistant/components/sensor/entity.py` and `const.py`. This
crate is an independent Rust port, not an official Home Assistant package.

## Example

```rust
use hass_entity::EntitySpec;
use hass_sensor::{
    SensorDescriptor, SensorDeviceClass, SensorProjectionOptions, SensorReading,
    SensorStateClass, SensorUnit, SensorValue,
};

let descriptor = SensorDescriptor::new(
    EntitySpec::new("sensor", "weather", "weather-entry", "outside-temperature")
        .with_unique_id("weather-outside-temperature"),
)?
.with_name("Outside temperature")
.with_device_class(SensorDeviceClass::Temperature)
.with_state_class(SensorStateClass::Measurement)
.with_native_unit("°C")
.with_suggested_unit("°F")
.with_suggested_display_precision(1);

let spec = descriptor.entity_spec();
let projection = descriptor.project_with_options(
    SensorReading::new(SensorValue::Number(20.0)),
    SensorProjectionOptions {
        user_unit: None,
        temperature_unit: None,
    },
)?;

// Register `spec` through EntityLifecycle, then publish `projection.snapshot`.
// `projection.warnings` reports source-compatible warning-only metadata issues.
```

## Projection behavior

- Stable metadata lives on `SensorDescriptor`; a `SensorReading` contains the
  current typed value, availability, optional `last_reset`, context, and any
  provider attributes. `None` is `unknown` while available. Unavailable readings
  project to `unavailable` without turning a missing value into zero.
- Plain text stays text when numeric metadata does not require a number. Numeric
  metadata parses numeric strings and rejects nonnumeric or non-finite values.
  Signed/unsigned integers and integer text retain exact digits when no unit
  conversion is needed, including counters beyond i64. Booleans retain Python's
  `True`/`False` spelling. Floating-point state strings use the pinned scientific
  notation conventions, with signed, two-digit exponents where needed.
  `Date` values publish ISO dates. `Timestamp` and `Uptime` require an aware
  datetime, normalize to UTC, and publish ISO-8601 with seconds precision.
- `enum` options are emitted as attributes and nonempty lists validate the
  reading. `last_reset` is emitted only with `total`; other state classes return
  a projection error when a reset value is supplied.
- A registered user unit takes priority over the descriptor's suggested unit,
  then the automatic temperature unit, then the native unit. The automatic unit
  is considered only for the `temperature` device class with a recognized
  temperature native unit.
- Temperature, pressure, atmospheric pressure, distance, precipitation, energy,
  energy storage, and power use class-gated converters for the pinned common
  unit sets. A requested unit change outside the matching converter or without
  a native unit is an error; the projector never relabels an unchanged value.
- Invalid device-class/state-class pairs and invalid native units for the
  complete pinned device-class unit tables are warnings, matching Core's current
  warning paths. An invalid state-class unit, invalid enum use, malformed value,
  non-finite numeric value, or invalid `last_reset` is an error. All 57 native-unit
  rules and the five classes without such a rule are source-derived; see
  [unit-table provenance](PROVENANCE/UNIT-TABLES.md). Explicit no-unit rules are
  distinct from classes for which Core does not validate units.
- Provider-native units apply the pinned 13 ambiguous-spelling aliases before
  display, conversion and validation. The native-unit getter retains the raw
  supplied value; suggested and user-selected units are not rewritten.

This is a focused shared projection layer. It does not implement Core's complete
registry persistence/readback of selected units,
uptime drift suppression, display rounding, restore data, recorder statistics,
sensor WebSocket operations, frontend behavior, or automation/device triggers.
Applications pass an effective user unit through `SensorProjectionOptions` and
use `EntityLifecycle` for lifecycle changes; this crate does not poll physical
sensors or own providers.
