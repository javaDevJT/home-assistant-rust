# Native `sensor` and `binary_sensor` port contract

**Status:** source contract only. No native sensor or binary-sensor platform
parity is claimed by this document.

Pinned source is Home Assistant Core
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
This contract covers the shared entity projection seam. Integration collectors,
configuration flows, recorder storage, and the complete automation/WebSocket
surface remain separate work.

## Platform contract

The source separates relatively stable entity metadata from the value a provider
updates. `SensorEntityDescription` carries device class, state class, native and
suggested units, enum options, and suggested display precision; `last_reset` is
a separately validated state attribute. The entity base supplies the current
value and validates/projects it when Core reads state. See the pinned
[`SensorEntityDescription`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L46-L56)
and [`SensorEntity.state`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L528-L732).

| Domain | Provider value | Published state | Domain metadata |
| --- | --- | --- | --- |
| `sensor` | Native scalar, `date`, timezone-aware `datetime`, `Decimal`, or no value | A state string after the applicable formatting, validation, and conversion; no value is `unknown` while available | Device class, optional state class, native/display unit, enum options, suggested precision/unit, and valid `last_reset` |
| `binary_sensor` | `Option<bool>` (`is_on`) | `on`, `off`, or `unknown` | Optional binary device class; its on/off meaning depends on the class |

In the Rust runtime, `EntitySnapshot` already distinguishes an absent state from
unavailability: an available snapshot with `state: None` writes `unknown`,
while `available: false` writes `unavailable`. It composes attributes separately
from the state. Therefore a sensor projection must return the final state string
and standard attributes before calling `EntityHandle::publish`; the lifecycle
layer does not convert or validate sensor values. See
[`EntitySnapshot`](../crates/hass-entity/src/lib.rs#L180-L200),
[`write_snapshot_to_id`](../crates/hass-entity/src/lib.rs#L1750-L1820), and
[`EntityHandle::publish`](../crates/hass-entity/src/lib.rs#L456-L508).

### Sensor value and metadata rules

- `None` remains `unknown`; it must not be replaced by zero. The generic
  availability flag independently produces `unavailable`.
- Date values use ISO date text. `timestamp` and `uptime` values require a
  timezone, normalize to UTC, and serialize to seconds. `uptime` also suppresses
  small clock drift. Enum options may be absent or empty; a nonempty option
  list validates membership, and providing options on a non-enum class is an
  error. These branches are in the pinned
  [state projector](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L574-L639)
  and the [device-class definitions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/const.py#L105-L145).
- Core expects a numeric value when numeric metadata calls for one, including a
  numeric device class, state class, unit, or suggested precision. Numeric
  strings are parsed; nonnumeric and non-finite values fail validation. Values
  without those numeric signals can remain text. The exact decision and checks
  are in [`SensorEntity.state`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L641-L680)
  and [`_numeric_state_expected`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L263-L274).
- Unit is part of the value contract. A registered user's selected unit has
  priority; Core then applies the applicable suggested-unit, temperature,
  translation, or native-unit rules. It converts the value only when a converter
  for the device class supports the units. A port must not relabel an unchanged
  numeric value with a different unit. The pinned rules are in
  [`unit_of_measurement`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L450-L522)
  and [`UNIT_CONVERTERS`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/const.py#L607-L650).
- Sensor device classes constrain units and permitted state classes. State
  classes also constrain units. Core currently logs an invalid device-class /
  state-class pair and an invalid device-class unit, but raises for an invalid
  state-class unit. These are not interchangeable validations. See the pinned
  [`DEVICE_CLASS_UNITS`, `DEVICE_CLASS_STATE_CLASSES`, and `STATE_CLASS_UNITS`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/const.py#L651-L907)
  and [validation paths](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L547-L573).
  Ordering matters: available `None` returns before numeric coercion, device-unit
  warnings and state-class-unit rejection. Those latter checks run only after a
  value successfully enters the numeric path; see
  [the early return](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L574-L580)
  and [unit validation](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L690-L732).
- Supported state classes are `measurement`, `measurement_angle`, `total`,
  and `total_increasing`; they describe recorder semantics, not merely a
  numeric-looking state. `last_reset` is only emitted for `total`; other state
  classes reject it. See the [state-class definitions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/const.py#L577-L605)
  and [state attributes](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L390-L412).

The shared Rust layer should represent readings with enough type information to
distinguish numbers, text, dates, timestamps, and no value until the projection
step. Its descriptor should keep stable metadata separate from the changing
reading. The projection should produce a state plus `device_class`,
`state_class`, `unit_of_measurement`, enum `options`, and other standard entity
attributes as applicable. Preserve Core's distinction between validation
errors and warning-only metadata mismatches; never silently turn bad data into
zero or publish a misleading unit/value pair.

### Binary sensor state and class

The pinned [`BinarySensorEntity`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/binary_sensor/entity.py#L14-L75)
uses `is_on: bool | None`; `None` maps to `unknown`, `true` to `on`, and `false`
to `off`. Availability remains the shared entity lifecycle's separate
`unavailable` state. `BinarySensorDeviceClass` does not change these stored
states; it defines their meaning. For example, `on` means low battery for
`battery`, connected for `connectivity`, open for `door`, and detected for
`motion`. See the pinned [binary classes](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/binary_sensor/const.py#L11-L105).

## Recorder and statistics boundary

The sensor SDK supplies valid states and metadata; it does not own history or
statistics. The recorder selects sensors that publish a valid `state_class` and
hashable device-class/unit metadata, then handles unit normalization. Its
default statistic types are mean/min/max for `measurement`, circular mean for
`measurement_angle`, and sum for `total` and `total_increasing` ([pinned recorder source](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/recorder.py#L13-L75)).
The SDK must preserve the class and unit so the recorder can make that decision;
it must not assign a state class to every numeric sensor or calculate recorder
aggregates itself.

## Services, conditions, and triggers

The pinned `sensor` and `binary_sensor` setup functions create an
`EntityComponent`; sensor setup also installs its sensor WebSocket API. Both
delegate entity addition and config-entry lifecycle to Core's generic entity
platform ([sensor setup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/__init__.py#L52-L111),
[binary-sensor setup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/binary_sensor/__init__.py#L46-L68),
[generic entity platform](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/entity_platform.py#L745-L980),
[config-entry add/unload](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/entity_component.py#L165-L214)).
The two domain packages have no `services.py` or `services.yaml` at this pin;
the sensor WebSocket API is a separate API surface. Do not invent sensor
commands as part of value projection.

Generic `state` and `numeric_state` conditions/triggers operate on published
entity states, so native sensors can reuse the existing shared automation
engine. The native crate exposes generic state and numeric-state triggers and
conditions in [`hass-automation`](../crates/hass-automation/src/lib.rs#L125-L155)
and [`Condition`](../crates/hass-automation/src/lib.rs#L819-L842). Upstream's
generic triggers are in the [`state`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/homeassistant/triggers/state.py#L131-L185)
and [`numeric_state`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/homeassistant/triggers/numeric_state.py#L122-L190)
modules. The platform contract is therefore to publish correct canonical states
and attributes, not to implement another state-trigger engine.

Binary sensors also have distinct device conditions and triggers selected from
the device class. Those mappings are defined in Core's
[`ENTITY_CONDITIONS`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/binary_sensor/device_condition.py#L141-L263)
and [`ENTITY_TRIGGERS`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/binary_sensor/device_trigger.py#L77-L199).
The current native automation enums cover generic state/numeric-state paths,
not these binary-sensor device mappings. They remain a separate automation gap;
emitting a `binary_sensor` state alone does not claim device-condition or
device-trigger parity.

## Lifecycle seam and native reuse

Keep the lifecycle manager domain-agnostic. `EntitySpec` already carries the
domain, platform, registry unique ID, config entry/subentry, device/area, and
stable provider identity ([`EntitySpec`](../crates/hass-entity/src/lib.rs#L39-L74)).
Register the sensor descriptions as `sensor` or `binary_sensor` specs, reconcile
config-backed sets with `register_provider_batch`, and publish projected
snapshots through their returned handles. Release those handles on provider
unload and call lifecycle shutdown only when the application stops
([registration and shutdown](../crates/hass-entity/src/lib.rs#L319-L402),
[`publish` and `release`](../crates/hass-entity/src/lib.rs#L456-L529)).

The existing native providers show why the common projection is useful:

- [`hass-system-sensors`](../crates/hass-system-sensors/src/lib.rs#L483-L673)
  already describes `time_date` values and an `uptime` timestamp, but builds
  state strings and metadata directly rather than through a sensor SDK.
- [`hass-sun`](../crates/hass-sun/src/integration.rs#L1224-L1345) publishes six
  timestamp sensors, two degree-valued `measurement` sensors, and a
  `solar_rising` binary sensor through raw entity specs/snapshots. Its degree
  sensors use state class `measurement`; do not reinterpret them as
  `measurement_angle` during migration.
- [`hass-hardware-metrics`](../crates/hass-hardware-metrics/src/lib.rs) is a
  useful later data source, but it is not currently an entity-platform adapter.

These consumers are evidence for a small supported subset, not evidence that
the Core `sensor` or `binary_sensor` platform is ported. A high-leverage first
SDK slice is a typed sensor descriptor/value projector plus the binary
`Option<bool>` projector, adapted to `EntitySpec` and `EntitySnapshot`. Exercise
it with the existing `time_date`/`uptime` and `sun` outputs. This covers plain
text, dates/timestamps, numeric measurement values with units, and binary state
without making every provider duplicate state serialization.

The SDK now maps all 41 pinned sensor `UNIT_CONVERTERS` entries to the 31
converter families in `hass-units`, including source-compatible invalid-unit
errors, identity shortcuts, reciprocal zero results, and the f64 limitation for
negative Beaufort values. Remaining gaps before a broader platform claim
include registry persistence/readback of selected units, every enum/device
class validation edge, display precision, restore of native value/unit,
recorder/statistics behavior, sensor WebSocket operations, and binary-sensor
device conditions/triggers. `RestoreSensor` specifically preserves the native
value and native unit as extra restore data ([pinned restore mixin](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sensor/entity.py#L955-L969));
the current `EntitySnapshot` contains a published string state, so that restore
contract needs an explicit typed extension if a target integration depends on
it.

## Source-pinned falsifiers

These existing Core tests are targeted acceptance cases for the SDK contract;
they were inspected as source and were not run in this documentation task.

- Temperature converts between Fahrenheit and Celsius only for the temperature
  class; the same units on another/no class must not be converted ([conversion
  and wrong-class tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L127-L160)).
- Unit projection respects the configured/registered and suggested-unit rules,
  rather than changing only the attribute ([priority tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L1392-L1540),
  [suggested-unit changes](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L1724-L1880)).
- Numeric-state expectations distinguish `date`/`enum`/`timestamp`/`uptime`
  from measurements; `None` remains unknown, while nonnumeric and non-finite
  values fail when numeric metadata is present ([validation tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L2412-L2695)).
- A state-class unit outside its allowed set and `last_reset` outside `total`
  must not be silently accepted ([unit and reset cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L211-L225),
  [invalid-unit cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sensor/test_init.py#L2412-L2460)).
- Binary values map exactly to unknown/on/off, with device class available for
  class-specific behavior ([state tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/binary_sensor/test_init.py#L30-L57)).
- Binary device conditions and triggers must follow class mappings and actual
  state changes ([condition tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/binary_sensor/test_device_condition.py#L235-L322),
  [trigger tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/binary_sensor/test_device_trigger.py#L236-L331)).
