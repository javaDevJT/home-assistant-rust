# Switch source contract

The source behavior in this port is pinned to Home Assistant Core
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
The native SDK lives in [`hass-switch`](../crates/hass-switch/README.md). It
projects entity metadata and state and parses the standard switch commands; it
does not own provider I/O or global service registration.

## Entity domain and state

The component declares domain `switch`, service constants `turn_on`,
`turn_off`, and `toggle`, and states `on` and `off` in
[`switch/__init__.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/__init__.py#L7-L57).
The standard entity ID format is `switch.{}`. `SwitchEntity` extends
`ToggleEntity`; its state is `unknown` when `is_on` is `None`, `on` when true,
and `off` when false. `async_toggle` turns off only when `is_on` is true; both
false and unknown turn on. The base also dispatches `async_turn_on` and
`async_turn_off` to provider implementations.
([`switch/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/entity.py#L12-L37),
[`helpers/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/entity.py#L1914-L1977)).

The source `SwitchDeviceClass` enum contains exactly `outlet` and `switch`;
the component's `DEVICE_CLASSES_SCHEMA` lowercases and coerces to that enum
([`switch/const.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/const.py#L15-L34)).
Their entity translations preserve the `on`/`off` state names and distinguish
the outlet name
([`switch/strings.json`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/strings.json#L53-L74)).

`assumed_state` reports that the provider cannot access the real state. Core
adds that attribute only when true. Core merges entity state and extra
attributes only while available, then appends `assumed_state` and standard
metadata independently of availability
([`helpers/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/entity.py#L913-L921),
[`helpers/entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/entity.py#L1159-L1188)).
The switch snapshot follows this projection: `Some(true)` is `on`,
`Some(false)` is `off`, `None` is `unknown`, unavailable is a separate flag,
and `assumed_state` is emitted only when true. Extra JSON attributes are
merged as values, so explicit `null` is preserved and omission removes a key.

`SwitchSnapshot::publication()` uses the shared lifecycle's additive live
attribute API: a true assumed-state flag survives unavailability, ordinary
extras do not, and a later false flag clears it. A false flag does not override
an available provider extra with the same name, matching the source merge
order. Explicit JSON null values remain values. Live attributes survive owned
SQLite reopen and lease reentry, while user registry overrides retain priority.
The legacy `entity_snapshot()` API preserves its original availability-scoped
behavior; the actual MQTT switch adapter uses `publication()`.

## Services

Core registers entity services in `turn_off`, `turn_on`, `toggle` order, each
with no extra schema fields and the corresponding `async_*` method
([`switch/services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/services.py#L8-L19)).
`services.yaml` declares a target restricted to entities in the `switch`
domain and no service fields
([`switch/services.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/services.yaml#L1-L16)).
The source translations name and describe the three commands as “Turn on
switch” / “Turns on a switch.”, “Turn off switch” / “Turns off a switch.”,
and “Toggle switch” / “Toggles a switch on/off.”
([`switch/strings.json`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/strings.json#L76-L89)).

`SwitchCommand::parse_service_call` enforces the `switch` domain, accepts only
those three service names, rejects non-target service data and target keys
outside the shared selector vocabulary, and rejects an `entity_id` duplicated
between `data` and `target`. It leaves selector value validation and entity
resolution to `hass_registry`'s shared service-target resolver. The generic
resolver accepts entity, device, area, floor, and label selectors; after
resolution, the MQTT provider can continue using normalized `entity_id` calls.

The source `test_init.py` exercises direct state lookup, individual and
all-entity turn-on/off calls, and service context propagation
([`test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_init.py#L26-L56),
[`test_init.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_init.py#L59-L84)).
The native tests cover strict parsing, target metadata, and the `unknown` toggle
case without performing a physical command.

## State and device conditions

The state condition registry exposes `switch.is_on` and `switch.is_off` and
also accepts `input_boolean` entities
([`switch/condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/condition.py#L11-L24)).
The target supports `any`/`all` behavior (default `any`). Its `for` field is
marked required and defaults to `00:00:00`
([`switch/conditions.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/conditions.yaml#L1-L20)).
Evaluation compares the exact state with `on` or `off`; unknown and unavailable
match neither. Source tests cover both behaviors, both domains, and combined
targets
([`test_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_condition.py#L37-L69),
[`test_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_condition.py#L72-L157),
[`test_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_condition.py#L187-L279)).

Device conditions are `is_off` and `is_on`. Their schema requires the switch
domain and entity ID and allows an optional positive time-period `for` field
([`switch/device_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/switch/device_condition.py#L15-L39),
[`device_automation/toggle_entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/device_automation/toggle_entity.py#L52-L63),
[`device_automation/toggle_entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/device_automation/toggle_entity.py#L87-L93)).
The helper evaluates the same exact `on`/`off` states and forwards `for` to
the state condition
([`device_automation/toggle_entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/device_automation/toggle_entity.py#L136-L155)).
Listed condition metadata uses `secondary: false` for an ordinary entity and
`secondary: true` for hidden or auxiliary entities; capabilities expose
`for` as `positive_time_period_dict`
([`test_device_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_device_condition.py#L25-L55),
[`test_device_condition.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/switch/test_device_condition.py#L57-L137)).

## Port boundary

`hass-switch` supplies typed descriptors, snapshots, service metadata, and a
pure command/condition vocabulary. A host integration still owns transport,
physical device I/O, command queuing, optimistic state policy, and automation
execution. Existing MQTT switch service registration remains the single host
side-effect path; the SDK does not register another set of global handlers.

The 48-crate continuation adds actual switch device conditions and triggers to
`hass-automation`, reusing the state engine and its duration/context handling.
The shared helper resolves a registry UUID once when attaching, keeps literal
IDs literal, and treats required `device_id` as metadata. Six focused switch
device cases and the compiled YAML trigger/condition/action path pass; the
existing binary-sensor mappings remain covered. Device actions, enumeration,
the generic `switch.is_on`/`switch.is_off` state-condition registry,
reproduction, significant-change handling, switch-as-light and full provider
and frontend coverage remain open.
