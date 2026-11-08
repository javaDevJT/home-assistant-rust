# Climate entity SDK port

This is a native Rust port of the Climate entity contract at Home Assistant
Core commit [`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate).
The [`hass-climate` crate](../crates/hass-climate/) is workspace member 53.
Its SDK checks are independent of the in-progress MQTT and application
acceptance work; enrollment alone does not establish platform parity.

## Pinned source contract

Climate defines the seven HVAC modes `off`, `heat`, `cool`, `heat_cool`,
`auto`, `dry`, and `fan_only`; the actions are `cooling`, `defrosting`,
`drying`, `fan`, `heating`, `idle`, `off`, and `preheating`. The default
temperature limits are 7–35 °C and humidity limits are 30–99%. Preset, fan,
vertical-swing, and horizontal-swing standard strings are constants only;
providers still advertise their own strings. These values and attribute names
come from [`climate/const.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/const.py#L18-L240).

`ClimateEntityFeature` assigns `TARGET_TEMPERATURE=1`,
`TARGET_TEMPERATURE_RANGE=2`, `TARGET_HUMIDITY=4`, `FAN_MODE=8`,
`PRESET_MODE=16`, `SWING_MODE=32`, `TURN_OFF=128`, `TURN_ON=256`, and
`SWING_HORIZONTAL_MODE=512`. Bit 64 is not defined. The Rust mask retains
unknown bits, as Python's `IntFlag` does.

The entity reports its HVAC mode as the state; no reported mode maps to
`unknown`, while an unavailable provider snapshot is handled by the shared
lifecycle as `unavailable`. `current_temperature` and `temperature_unit` are
always projected. `temperature` (`target_temperature`) and the high/low
temperature pair are feature-gated. Current humidity is omitted when absent;
target humidity, fan/preset/swing modes are present as JSON null when their
feature is advertised but their value is absent. A missing HVAC action is
omitted. These rules are in [`climate/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L127-L264).

The capability map always carries `hvac_modes`, `min_temp`, and `max_temp`;
`target_temp_step` is present only when truthy. Humidity bounds and step,
`fan_modes`, `preset_modes`, `swing_modes`, and `swing_horizontal_modes` are
feature-gated. An unreported `hvac_modes` list remains JSON null; an explicit
empty list remains `[]`. A provider must declare its native temperature unit;
there is no base-entity fallback to Core's display setting. Default 7/35 °C
bounds are converted into the declared native unit.
Display conversion and precision follow Core's [`display_temp`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/temperature.py#L10-L36)
helper, including ties-to-even rounding.
The capability and default-property rules are in
[`climate/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L266-L447)
and [`climate/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L625-L667).

Core marks these capability attributes unrecorded:
`hvac_modes`, `fan_modes`, `swing_modes`, `min_temp`, `max_temp`,
`min_humidity`, `max_humidity`, `target_humidity_step`, `target_temp_step`,
and `preset_modes`. `swing_horizontal_modes` is a capability but is not in
that source set. The Rust spec preserves this list exactly; its publication
also keeps capabilities live through the shared unavailable-state lifecycle.
The source set is in [`climate/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L45-L90).

The runtime service schema is registered in Python. It exposes
`turn_on`, `turn_off`, `toggle`, `set_hvac_mode`, `set_temperature`,
`set_humidity`, `set_fan_mode`, `set_preset_mode`, `set_swing_mode`, and
`set_swing_horizontal_mode` in
[`climate/services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/services.py#L177-L246).
`set_temperature` requires a single `temperature` or the inclusive
`target_temp_low`/`target_temp_high` pair; it can also include `hvac_mode`.
Each numeric target needs its matching feature bit. Values are converted from
Core's configured unit to the entity's native unit, checked against displayed
rounded bounds, then clamped to the native bounds. Humidity is checked against
provider bounds. Mode commands require the exact provider-advertised list
membership; an empty or absent list rejects the value. The parser and numeric
behavior are in [`climate/services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/services.py#L35-L173)
and mode validation is in
[`climate/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L448-L558).

[`climate/services.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/services.yaml#L3-L151)
describes frontend selectors, including a nested `temperature_range` UI
group. Runtime service data still uses the flat `target_temp_low` and
`target_temp_high` keys checked in Python. The domain setup and config-entry
hooks are in [`climate/__init__.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/__init__.py#L189-L213).

The pinned regression cases include feature and mode validation,
humidity/temperature range checks, display rounding, native-unit conversion,
and default bounds in
[`tests/components/climate/test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/climate/test_init.py#L214-L806),
[`test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/climate/test_init.py#L1116-L1342).

## Rust interface

`ClimateDescriptor` carries stable identity, provider metadata, feature bits,
HVAC mode enums, provider-defined mode lists, native temperature unit,
precision, and numeric bounds. There is no climate device-class enum. The
descriptor produces an `EntitySpec`; `entity_spec_with_units` converts its
capability metadata to Core's configured display unit. A missing native unit
is rejected during metadata validation, publication, and service planning.

`ClimateSnapshot` distinguishes unknown (`hvac_mode: None`), `off`, and
unavailable states; state and mode attributes remain nullable as described
above. `ClimateSnapshot::project` or `ClimateDescriptor::publication` returns
an `EntityPublication`: capability attributes use live `Set`/`Remove`
updates. `supported_features` follows the shared lifecycle's live metadata
path, while temperature readings, reported modes, and extra provider
attributes remain availability-scoped. Registry name and icon overrides stay
owned by `EntityLifecycle` and retain priority while unavailable.

`ClimateCommand::parse_service_call` validates the domain, service fields,
target selectors, required arguments, HVAC enum values, temperature field
combinations, and numeric coercions. It preserves the service context.
`ClimateServiceRequest::resolve` applies descriptor feature gates, provider
list validation, humidity/temperature ranges, and unit conversion before
returning an ordered `ClimateCommandPlan`. Temperature service values are
assumed to use the `ClimateUnitContext` configured unit; the root supplies
that value. Plans retain unresolved target selectors and causal context and
contain typed `ClimateEffect` variants for all ten registered services. The
host must resolve and admit targets, execute provider effects, and publish
resulting state through its real adapter.

The SDK uses the shared `hass-units` temperature converter; it does not copy
Fahrenheit/Celsius/Kelvin factors. Feature masks, enum values, and provider
lists are validated at their respective typed/provider/service layers.

Runtime entity-service targets require a positive selector key, as specified
by `config_validation.py:1406,1428-1441`; they do not require a truthy selector
value. Thus an empty area list with a valid exclusion remains a valid
unresolved target whose no-op behavior belongs to the host. The separate
`exclude_requires_target` truthiness rule is applied to script/config targets
at `config_validation.py:1543-1545`, not to the runtime entity-service schema.

## Verification and remaining host work

The crate's integration suite covers each service family and its feature gate,
unknown and mixed arguments, target selectors and context, HVAC/list
validation, inclusive humidity limits, Celsius/Fahrenheit/Kelvin conversion,
rounded temperature bounds and clamping, nullable and unavailable states,
live capability setting/removal versus ordinary attributes, and synthetic
registry rename/disable/re-enable/release behavior. The lifecycle tests use
only an injected in-memory `Runtime` and registry fixtures.

The host still supplies Core's global unit configuration and precision,
resolves targets and applies provider admission/availability, and performs
MQTT/provider I/O. `turn_on`, `turn_off`, and `toggle` plans intentionally
identify the corresponding provider hook; this SDK does not infer whether a
Python subclass overrode those methods or emulate Core's base-class fallback
selection of HVAC modes. A command schema does not prove those provider state
transitions. No end-to-end MQTT climate path is implemented in this crate.

The context-free `entity_spec()` keeps explicit/native-unit capability data;
the host should use `entity_spec_with_units()` for complete registry capability
metadata in the configured display unit. Both paths require a native unit. The
shared lifecycle applies registry overrides, renames, disables, and release;
the climate crate has no separate lifecycle state machine.

## Candidate 53 MQTT and application adoption

The MQTT adapter uses this SDK through the application's shared entity
lifecycle. Explicit protocol `C`/`F` inputs are normalized to typed units;
omitted native units capture Core's configured unit at setup. Later display
changes reproject cached native readings and leave accepted command values in
their native units. Actor admission applies each command's explicit cache
operation once, and acknowledgment completion projects the live cache, so
intervening telemetry and chained equal-value intent remain correct.

The full local candidate passes 1,374 tests across 158 targets. Its MQTT package
passes six unit and 26 loopback wire tests, and two compiled-process cases
cover actual registry/config-entry lifecycle, persisted units and discovery
removal. Empty discovery removes the owned entity-registry entry before
releasing its handle; provider unload retains that entry. Original publisher
handles also guard ACK completion against replacement discovery entities.
Formatting and strict workspace all-target Clippy pass; optimized artifact and
exact-source Linux acceptance are tracked in [VALIDATION.md](VALIDATION.md).

MQTT templates receive `name` and a serialized `this` map containing
`entity_id`, `name`, `state` and `attributes`. This is a supported subset of the
Python State interface. Broader provider, UI and Python-only coercion coverage,
along with MQTT orphan-device/trigger/tag cleanup, remains separate work.
