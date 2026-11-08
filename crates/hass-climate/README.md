# Native climate SDK

`hass-climate` provides typed Climate descriptors, snapshots, feature bits,
strict service parsing, and pure provider-effect plans for Home Assistant Core
commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The source contract is in
[`const.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/const.py#L18-L240),
[`entity.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/entity.py#L127-L264),
and [`services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/climate/services.py#L35-L246).
It is workspace member 53. Native adapter acceptance is tracked separately
from the SDK checks.

The SDK projects through the shared `hass-entity` lifecycle. It performs no
provider or MQTT I/O, target resolution, availability admission, or climate
device-class guessing. `ClimateUnitContext` carries Core's configured
temperature unit into projection and temperature-command validation; the
adapter owns that global preference. Every descriptor requires an explicit
native temperature unit, which stays separate from the configured display unit.

The adapter still owns Core's global unit configuration, target resolution,
availability admission, and provider I/O. The turn-on/off/toggle effects do
not emulate Python subclass overrides or Core's base-class HVAC-mode fallback.

See [CLIMATE-PORT.md](../../docs/CLIMATE-PORT.md) for source anchors, API
contracts, tests, and the remaining host-owned behavior.
The source transcription follows the repository's Apache-2.0 license.

Focused verification:

```sh
cargo test -p hass-climate --locked --offline
cargo clippy -p hass-climate --all-targets --locked --offline -- -D warnings
cargo fmt -p hass-climate -- --check
```
