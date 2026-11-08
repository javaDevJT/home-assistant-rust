# Native fan SDK

Typed descriptors, snapshots and feature bits project fan state and capabilities
through the shared entity lifecycle. `FanCommand::resolve` produces an ordered
`FanCommandPlan`; the provider executes its `FanEffect` values. Setting percentage
to zero preserves the source's turn-off followed by percentage-update ordering.

The SDK performs no provider I/O. MQTT consumes the planner and publication API
using the application's existing entity manager. Unknown readings, unavailable
state and retained live capabilities remain distinct.

See [the source contract and remaining gaps](../../docs/FAN-PORT.md).
Focused verification: `cargo test -p hass-fan --locked --offline`.
