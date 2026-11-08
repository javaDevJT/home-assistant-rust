# Fan entity SDK port

This is an isolated native Rust port of the Home Assistant fan entity contract
at commit [`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan).
The SDK is enrolled in the expanded 52-crate workspace and is consumed by the
native MQTT adapter. This work remains outside the historical 48-crate receipt;
the expanded candidate requires its own acceptance gate.

## Source contract

The pinned entity defines feature bits `SET_SPEED=1`, `OSCILLATE=2`,
`DIRECTION=4`, `PRESET_MODE=8`, `TURN_OFF=16`, and `TURN_ON=32`. There is no
fan device-class enum or `set_speed` service in this source revision. The
feature bits and entity defaults are in [`const.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/const.py#L50-L82)
and [`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/entity.py#L55-L70).

`FanSnapshot` preserves the default 0% value, nullable percentage/direction/
oscillation/preset state, and `is_on` rule: a percentage greater than zero or
any non-null preset mode means `on`. A supported state attribute remains
present as JSON null when its provider value is `None`; unsupported attributes
are omitted. Zero remains numeric zero and yields `off` unless a preset mode is
also present. The source projection is in [`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/entity.py#L193-L278).

`FanDescriptor::entity_spec` carries `supported_features` and `preset_modes` in
shared provider metadata. The `preset_modes` capability remains live during an
unavailable state, while percentage, preset mode, direction, oscillation, and
provider extras remain availability-scoped snapshot attributes. `None` preset
modes become JSON null when a speed or preset feature is present; removing both
features removes the live attribute. The source marks `preset_modes`
unrecorded, which the descriptor carries into `EntitySpec` recording policy.
The capability projection and unrecorded marker are in [`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/entity.py#L44-L60)
and [`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/entity.py#L226-L264).

The nine registered services and their feature gates are in [`services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/services.py#L11-L85)
and their selector schemas in [`services.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/services.yaml#L16-L129).
Toggle requires both turn-on and turn-off bits. Preset setting requires both
speed and preset bits. `turn_on` itself requires only the turn-on bit, even
when optional percentage or preset arguments are supplied.

## Rust interface

The native crate is [`hass-fan`](../crates/hass-fan/). It exposes
`FanFeatures`, `FanDescriptor`, `FanSnapshot`, `FanState`, `FanDirection`,
`FanCommand`, `FanServiceRequest`, and `FanCommandPlan`.
`FanCommand::parse_service_call` validates the fan service and retains target
selectors and context. `FanCommand::resolve` checks feature bits and provider
preset modes, then returns ordered `FanEffect` values representing actual
provider method calls. The planner performs no device or network I/O; the host
must pass these effects to its real adapter and apply its target and
availability policy.

The zero-percent path is deliberately a two-effect plan:

1. `TurnOff`
2. `SetPercentage { percentage: 0 }`

The pinned `async_set_percentage(0)` awaits `async_turn_off()` but does not
return before calling `set_percentage(0)`. A provider that fails on the first
effect must not be described as having applied the second; the plan itself does
not implement rollback. This order is in [`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/fan/entity.py#L71-L108).
Speed adjustment without an explicit step uses the
inclusive range `(1, speed_count)`, rounds the current mapped step upward, moves
one step, converts back with source floor division, and clamps the final percentage
to `0..100`. An explicit step uses integer addition/subtraction and the same
clamp. The helper formulas and examples follow [`percentage.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/util/percentage.py#L11-L91)
and [`test_percentage.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/util/test_percentage.py#L31-L180).
The pinned fan tests cover the base defaults, attribute passthrough, and valid
and invalid preset-mode service behavior in [`test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/fan/test_init.py#L29-L150).

The service parser models `Coerce(int)` as bool-to-0/1, finite-number
truncation, and decimal integer-string parsing before the `0..100` check.
`oscillating` accepts JSON booleans, 0/1, and common textual boolean values.
The pinned service file names `cv.boolean`; its complete accepted token set is
not included in the inspected fan source, so exact parity for every HA
configuration-validation alias remains unverified. The upstream schema marks
`set_direction.direction` optional although its async method requires the
argument; this SDK preserves an omitted value as `None` in the provider effect
for the host to handle explicitly.

## Verification and integration status

The crate includes synthetic tests for feature bits, default and nullable
states, capability versus ordinary attributes, conversion rounding, service
coercion and required arguments, feature gates, preset validation, effect
ordering, registry overrides/removal, and unavailable-state lifecycle
publication. Eight focused unit tests pass with strict all-target Clippy and
formatting. Explicit reported on/off/unknown state can override the base
speed/preset-derived state, matching providers with an independent power topic.

MQTT consumes `FanCommandPlan`, uses the source's inclusive speed-range formulas,
and derives `speed_count` from the configured range, capped at 100. Its wire
regression `shared_fan_uses_reported_power_inclusive_speeds_and_zero_command_order`
checks basic-fan toggle, speed adjustment, independent power state, OFF then
speed zero, zero-speed reception, and ON-only behavior for `turn_on(percentage=0)`.
Each admitted target's provider effects are now owned by the MQTT event loop
and advance only after the preceding QoS completion or normal soft timeout.
Canceled waiters do not abandon chains; real failures stop remaining effects,
late acknowledgments do not repeat them, and shutdown rejects continuations.
Attribute-only effects use current desired power state at completion. The full
expanded gate is recorded in VALIDATION.md; the prior 48-crate artifact does not
include this SDK.
