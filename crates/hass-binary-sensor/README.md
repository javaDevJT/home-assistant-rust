# `hass-binary-sensor`

This standalone crate provides a typed binary-sensor descriptor and a small
projection from `Option<bool>` to the shared native entity lifecycle. Its
source contract is Home Assistant Core revision
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).

```rust
use hass_binary_sensor::{BinarySensorDescriptor, BinarySensorDeviceClass, BinarySensorSnapshot};
use serde_json::json;

let entity = BinarySensorDescriptor::new("my_integration", "my_integration:entry-1", "front_door")
    .with_unique_id("front-door-contact")
    .with_device_class(BinarySensorDeviceClass::Door)
    .with_name("Front door")
    .entity_spec();

let snapshot = BinarySensorSnapshot::new(Some(true))
    .with_extra_attribute("battery_level", Some(json!(87)))
    .into_entity_snapshot();
// Pass `entity` to EntityLifecycle::register_provider_batch and publish
// `snapshot` through the returned EntityHandle.
```

The value mapping is fixed: `Some(true)` publishes `on`, `Some(false)` publishes
`off`, and `None` publishes `unknown`. Availability is independent; use
`with_available(false)` or `BinarySensorSnapshot::unavailable` to have the
shared lifecycle publish `unavailable`.

`BinarySensorDeviceClass` contains the pinned source's complete class list,
canonical strings, and plain descriptions of each class's `on`/`off` meaning.
`EntityLifecycle` preserves its metadata order: explicit registry overrides
win, dynamic snapshot attributes remain authoritative when no override exists,
and provider class/icon/name metadata supplies the fallback. `None` removes an
extra attribute; explicit JSON `null` is preserved, matching the pinned entity
helper's direct extra-attribute merge. Causal `Context` values are copied into
`EntitySnapshot` unchanged.

This crate is a value and metadata projection layer. It does not implement a
binary-sensor provider, device-condition/trigger dispatch, automation behavior,
recorder history, or the complete Home Assistant platform surface. Providers
own source polling and use the existing entity lease for registration,
publication, and unload.
