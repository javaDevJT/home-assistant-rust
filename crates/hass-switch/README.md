# hass-switch

`hass-switch` provides a typed `switch` descriptor, switch-state projection,
standard service metadata, and strict parsing for `turn_on`, `turn_off`, and
`toggle`. Its source contract is pinned to Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and mapped in
[`docs/SWITCH-PORT.md`](../../docs/SWITCH-PORT.md).

```rust
use hass_core::Context;
use hass_switch::{SwitchCommand, SwitchDescriptor, SwitchDeviceClass, SwitchSnapshot};

let spec = SwitchDescriptor::new("mqtt", "config-entry-17", "kitchen-outlet")
    .with_unique_id("device-17-outlet")
    .with_name("Kitchen outlet")
    .with_device_class(SwitchDeviceClass::Outlet)
    .entity_spec();

let snapshot = SwitchSnapshot::new(Some(true))
    .with_assumed_state(false)
    .with_context(Context::default())
    .publication();

let command: SwitchCommand = "toggle".parse()?;
let next_state = command.resolve(None); // unknown toggles to on
```

Register `spec` with the shared `hass_entity::EntityLifecycle` and call
`publish_publication(snapshot)` on its returned handle. The lifecycle keeps entity identity and
registry overrides current through rename, disable, unload, and restore. This
crate does not register global services, open MQTT connections, or read or
command devices.

Switch services define an entity target for domain `switch` and no service
fields. `SwitchCommand::parse_service_call` validates that vocabulary and
leaves target value validation and entity resolution to the shared registry
resolver. `SwitchCommand::resolve` applies Core's toggle rule: known `on`
becomes `off`; `off` or `unknown` becomes `on`.

`publication()` preserves `assumed_state: true` while unavailable and keeps
ordinary extra attributes scoped to availability. It also clears the flag on
later publications. The legacy `entity_snapshot()` projection remains available;
use `publication()` for the source-defined live metadata behavior.
