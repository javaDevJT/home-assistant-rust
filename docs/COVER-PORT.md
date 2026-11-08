# Cover SDK port

This isolated `hass-cover` SDK follows Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. It was implemented from the pinned
cover component and `homeassistant/helpers/entity.py` as source text; the Home
Assistant Python package and dependency code were not imported or executed.

## Implemented contract

- `CoverDeviceClass` contains the pinned `awning`, `blind`, `curtain`, `damper`,
  `door`, `garage`, `gate`, `shade`, `shutter`, and `window` values.
- `CoverFeatures` retains the pinned `OPEN=1`, `CLOSE=2`, `SET_POSITION=4`,
  `STOP=8`, `OPEN_TILT=16`, `CLOSE_TILT=32`, `STOP_TILT=64`,
  `SET_TILT_POSITION=128`, and `SPEED=256` bits.
- `CoverSpeed` contains the standard `fast`, `medium`, and `slow` labels. The
  provider's supported-speed list remains open to custom strings, as in source.
- State projection gives opening precedence over closing. Otherwise `is_closed`
  maps to `closed`/`open`; an absent value maps to `unknown`. Position alone does
  not decide open/closed state.
- Available snapshots always include `is_closed`; an unknown value is explicit
  JSON `null`. Current position and tilt attributes are omitted when absent.
  The values remain integer reports, as in the source entity.
- Inferred features start with open, close, and stop. A nonempty supported-speed
  list adds speed selection; a current position adds set-position; a current
  tilt position adds open/close/stop/set-tilt. A descriptor's explicit feature
  override replaces inference.
- Unavailable publications become `unavailable` and suppress ordinary state
  attributes through `EntityLifecycle`. Feature bits, device class, and a
  nonempty supported-speed list remain available as capability metadata.
- Service parsing accepts the ten pinned cover service names, standard target
  selectors, `speed` on open/close/set-position, and required percentage
  arguments on set-position and set-tilt-position, represented by the validated
  `CoverPercentage` type. Position coercion follows
  integer conversion before checking the inclusive 0–100 range. Unknown fields,
  malformed strings, duplicate `entity_id` selectors, missing arguments, and
  unsupported feature masks return typed errors.
- Open, close, and set-position silently omit a requested speed when it is not
  listed in the current nonempty `supported_speeds`, matching the source wrapper.
  Toggle requires both open and close bits. While moving, it chooses stop only
  when stop is supported; otherwise it chooses the opposite direction. The
  source's fallback for closed, fully open, and partially open/unknown state is
  retained, including its initial assumption that an ambiguous cover should
  close. Tilt toggle opens only at current tilt position zero and closes for all
  other values, including unknown.

## Deliberate port boundaries

The crate is a native SDK layer, not a Home Assistant integration runtime.
Remaining work includes:

- Further provider setup, reload/unload and device-specific translation beyond
  the native MQTT adapter. MQTT now consumes the SDK for shared lifecycle
  publication, command resolution and persistent per-entity toggle memory.
- Cover device automation triggers and conditions (`cover/trigger.py` and
  `cover/condition.py`) and provider-specific automation registration.
- State reproduction (`cover/reproduce_state.py`) and its state-to-service
  policy.
- Home Assistant's service registration/UI metadata and frontend cover entity
  card, device-class labels, and selector behavior. The SDK validates calls but
  does not render or register UI descriptions.
- Generic Home Assistant significant-change filtering and scheduling. The SDK
  publishes snapshots directly and does not decide when a provider should
  report changes.
- Broader provider and physical-device validation. Current checks use synthetic
  Rust lifecycle fixtures, a numeric-loopback broker and the compiled native
  process, including config-entry unload/reload and SQLite-backed restart.

The wire regression
`shared_cover_toggle_remembers_observed_direction_and_exposes_tilt_toggle`
checks motion-stop behavior, remembered direction after motion ends and both
tilt-toggle directions through actual registered services. No Python package is
executed by these checks.
