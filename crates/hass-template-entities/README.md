# Native template entities

This crate ports a bounded subset of Home Assistant's modern root `template:`
YAML integration. It reads live state through `hass_core::Runtime`, renders
expressions with `hass-template`, stores explicit `unique_id` values through
the shared `hass-registry` manager, and runs control actions with
`hass-script::run_sequence`.

The application deserializes the root `template:` list into
`Vec<TemplateBlock>`, calls `validate(&blocks)`, then starts the integration
with its shared registry manager:

```rust,ignore
let templates = TemplateEntities::start_with_registries(
    runtime.clone(),
    config.template,
    registries.clone(),
)
.await?;
// ...
templates.shutdown().await?;
```

`TemplateEntities::start(runtime, blocks)` is a standalone convenience for
tests or callers that do not already own a `Registries` instance. The strict
Serde configuration types reject unknown entity fields. The root `template:`
value is a list of blocks; a supported domain key may contain one entity map or
a list of maps.

## Supported entities and fields

State-based entities:

- `sensor`: required `state`; `device_class`, `state_class`, and
  `unit_of_measurement`.
- `binary_sensor`: required `state`; `device_class` and
  `unit_of_measurement`.
- `switch`: optional `state`, `turn_on`, `turn_off`, and `optimistic`.
- `number`: required `set_value`; optional `state`, `min`, `max`, `step`,
  `device_class`, `unit_of_measurement`, and `optimistic`.
- `select`: required templated `options`; optional `state`, `select_option`,
  and `optimistic`.
- `button`: required `press`; optional `device_class`.

These entities also support the YAML common fields `name`, `unique_id`,
`default_entity_id`, `availability`, `icon`, `picture`, `variables`, and
`attributes`. Attribute templates may be a map of per-attribute templates or a
template that returns a JSON-compatible object. Dynamic `name`, `icon`,
picture, and custom attributes are reflected in runtime state. Sensor units
and device classes are static metadata; select options and number range values
may be templates. Explicit unique IDs use the shared entity registry; entities
without one persist their generated entity ID mapping in Runtime metadata.

Switch, number, select, and button actions are real `hass-script` sequences.
The registered `switch.turn_on`, `switch.turn_off`, `switch.toggle`,
`number.set_value`, `select.select_option`, and `button.press` services execute
the configured actions against Runtime services. A switch, number, or select
state changes optimistically only when `optimistic: true`; a normal action
never stands in for a device's reported state. A successful button press stores
its last-pressed timestamp, matching the button's local action state.

## Reactive state and errors

The integration subscribes atomically to a Runtime state snapshot and later
state events. It coalesces each bounded event batch and re-evaluates entities in
topological order when one template references another configured template
entity by its static entity ID. Dependency cycles are rejected during startup.
Own state events refresh only downstream dependents, which prevents an entity
from continuously refreshing itself. Templates using `now()` or `utcnow()` are
refreshed once per minute.

A false availability template writes `unavailable`. A failed state template
writes `unavailable` when there is no separate availability template; with a
successful availability template, the entity remains available and its empty
native state is reported as `unknown`, matching Home Assistant's template
entity behavior. An availability template error retains availability, also
matching the pinned upstream behavior. Missing source states remain `unknown`;
invalid numeric and boolean states become unavailable rather than zero or
false.

Shutdown cancels and joins the reactive task and active script runs, unregisters
only this integration's service owner, and releases its Runtime clones. Drop
performs synchronous cancellation, provider cleanup, and reactive-task abort as
a fallback; call `shutdown().await` for joined cleanup.

## Unsupported configuration

Trigger-backed blocks (`triggers`, `actions`, `conditions`, `trigger_variables`,
block `unique_id`, and block `variables`) are rejected. Entity-level
`conditions` are also rejected. Legacy singular root keys (`trigger`, `action`,
and `condition`) and template domains outside the six listed above fail strict
Serde decoding. Other upstream options such as sensor `last_reset`, binary
sensor delays and `auto_off`, light, cover, and alarm-control-panel entities
are not implemented. Numeric and device-class strings are type/length checked;
the full Home Assistant class and unit enumerations are not replicated yet.

The schema and expected behavior were cross-checked against Home Assistant
Core commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and its template sensor,
binary sensor, switch, number, select, and button tests.
