# Native light SDK

Typed light descriptors, snapshots, color modes and feature bits project through
the shared entity lifecycle. Service normalization and color conversion helpers
follow the pinned Home Assistant source. The MQTT adapter consumes this SDK for
real discovery, state publication and command dispatch.

The SDK performs no provider I/O. The host owns registration, target admission,
provider execution and lifecycle shutdown. Registry overrides and live capability
attributes remain separate from ordinary provider attributes while unavailable.

See [the source contract and remaining gaps](../../docs/LIGHT-PORT.md).
Focused verification: `cargo test -p hass-light --locked --offline`.
