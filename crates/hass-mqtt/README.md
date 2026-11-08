# Native MQTT integration

`hass-mqtt` owns one native MQTT client and bridges MQTT discovery, device
messages, runtime entity state, and outgoing service commands. It uses
`rumqttc` 0.25. `MqttIntegration::start(runtime, config).await` waits at most
five seconds for the broker's first successful CONNACK. Call
`shutdown().await` to send DISCONNECT and join its event loop.

## Configuration

The public `MqttConfig` is strict: unknown keys fail deserialization. The
password is redacted from its `Debug` output. `validate()` checks settings
without contacting the broker, so config-check commands can call it offline.

```yaml
mqtt:
  host: 127.0.0.1
  port: 1883
  client_id: homeassistant-rust
  username: mqtt-user       # optional
  password: replace-me      # optional
  tls: false                # true enables platform-root certificate checking
  keep_alive_secs: 30
  qos: 0                    # 0, 1, or 2
  discovery_prefix: homeassistant
  discovery_enabled: true
  optimistic: false
  max_payload_bytes: 65536
```

Defaults are shown above. `host` is required. `keep_alive_secs` is 5–3600 and
`max_payload_bytes` is 256–65536. Incoming packets are capped by the MQTT
client and each payload is checked against the configured limit before JSON or
template processing. Discovery retains at most 1024 active entities.
When set, `password` requires a non-empty `username`; `username` alone sends an
empty MQTT password.

## Discovery and state

The port handles `sensor`, `binary_sensor`, `switch`, `light`, `cover`, `fan`,
`climate`, `button`, `number`, `select`, and `text` discovery topics in either supported
Home Assistant form:

```text
homeassistant/<platform>/<object_id>/config
homeassistant/<platform>/<node_id>/<object_id>/config
```

Each entity gets a stable ID from its configured name (or discovery object ID)
and a registry identity from `unique_id`. When `unique_id` is omitted, the
discovery node/object pair is used. A `device` object is preserved in
`MqttEntityMetadata`; `MqttIntegration::entity_metadata()` provides a sorted
readback for root registry reconciliation. An empty retained discovery payload
removes the runtime state and active metadata for that discovery topic.

State and command topics may use Home Assistant's `~` base topic shorthand.
Sensors accept `state_topic`, `value_template`, `unit_of_measurement`,
`device_class`, `state_class`, `json_attributes_topic`, and
`json_attributes_template`. Binary sensors map `payload_on`/`payload_off` to
`on`/`off`. Switches and lights map `state_on`/`state_off` (defaulting to
`payload_on`/`payload_off`) to `on`/`off`. `value_template` is used for
sensor/binary sensor/switch state; lights use `state_value_template` and then
`value_template`. State/value templates accept one expression (for example,
`{{ value_json.temperature }}`); command and JSON-attributes templates use the
shared bounded string renderer. Templates receive `value`, `entity_id`, and
`name`; state templates also receive `value_json` when the message parses as
JSON. The `this` object exposes `entity_id`, `name`, `state`, and `attributes`
from the entity's current Runtime state (or `unknown` and discovery attributes
before a state exists). It does not expose Python State properties such as
timestamps or context.

Availability accepts `availability_topic` or an `availability` object/list,
with `availability_mode: all|any`. Defaults are `online` and `offline`.
Broker loss marks discovered entities unavailable; the event loop resubscribes
discovery, state, JSON-attributes, and availability topics after reconnect.

Cover discovery accepts state, position, and tilt topics and their command
counterparts. Positions are checked against configured ranges and exposed as
percentages. Fan discovery accepts on/off, percentage, preset, oscillation, and
direction topics; percentage values use `speed_range_min` and
`speed_range_max`. Climate discovery subscribes to 12 state topics for HVAC
mode/action, current and target temperature/humidity, fan, preset, and swing
modes. Reported temperatures stay in the device's native unit. The MQTT
`temperature_unit` accepts `C`/`c` and `F`/`f`, then normalizes to the typed
degree unit used internally. If `temperature_unit` is omitted, the adapter
captures Core's configured unit system when discovery is accepted; later Core
unit changes reproject the cached native values without sending MQTT commands.
Defaults include modes
`auto/off/cool/heat/dry/fan_only`, fan modes `auto/low/medium/high`, swing modes
`on/off`, humidity bounds 30–99, temperature step 1, and an initial target of
21 °C converted to the native unit. Buttons publish `payload_press`. Numbers validate `min`,
`max`, and `step`; selects validate `options`; text entities enforce `min`,
`max`, and an optional regular-expression `pattern`. Their reported values are
updated only from state topics. An empty retained config message removes the
entity and its metadata for every supported platform.

Example retained discovery and wire messages used by the local broker fixture:

```json
{"name":"Room Temperature","unique_id":"room-temperature-1","~":"lab","state_topic":"~/temperature","availability_topic":"~/available","value_template":"{{ value_json.temperature }}","unit_of_measurement":"°C","device":{"identifiers":["room-1"],"name":"Room"}}
```

Publish that object retained to `homeassistant/sensor/room/temperature/config`,
then publish `online` to `lab/available` and
`{"temperature":21.5}` to `lab/temperature`; the runtime reports
`sensor.room_temperature = 21.5` with the configured unit.

## Shared entity lifecycle

The application config-entry driver starts MQTT with its shared entity
lifecycle and registries. Discovered switches, buttons, lights, covers, fans,
and climate entities use their native descriptors and snapshot projections there, while MQTT
keeps its discovery, state-topic, command queue, reconnect, and removal flow.
Registry names, device-class and icon overrides remain effective; raw MQTT
device metadata is preserved while its supported identity and details are
registered against the config entry. Renamed entity IDs are resolved from the
live lifecycle handle, and disabled or removed entities are not service
targets. Platform capabilities remain visible while unavailable; ordinary
MQTT state attributes are hidden until the entity is available again.

An empty discovery payload permanently removes the owned entity-registry row
before releasing its publisher, so it does not leave a restored placeholder.
Ordinary provider unload retains that row. Registry identity checks prevent a
reused entity ID from deleting another publisher's entry. Orphan-device,
device-trigger and tag cleanup remain separate work.


Standalone callers can continue using `MqttIntegration::start`; that entry
point keeps the direct runtime path for compatibility. Existing constructors
use the built-in light profile catalog. The shared lifecycle config-entry
driver uses `start_with_entity_lifecycle_and_profiles` for the native
switch/button/light/cover/fan/climate SDK path and accepts the host-loaded light
profile catalog. Profile defaults are resolved against each entity's current
registry ID.

## Commands and optimistic state

The integration registers switch and basic light `turn_on/off/toggle`, cover
open/close/stop/position/tilt services, climate `turn_on`, `turn_off`, `toggle`,
HVAC mode, temperature, humidity, fan mode, preset, and swing mode services,
fan turn on/off/percentage/preset/oscillation/direction services, `button.press`, `number.set_value`,
`select.select_option`, and `text.set_value`. Each service requires a known
`entity_id` target. Switch/basic-light discovery requires `command_topic`;
`payload_on` and `payload_off` default to `ON` and `OFF`. `command_template`
receives `value` (the selected payload), `entity_id`, `name`, `this`, and the
service data/target variables.
This supports the usual JSON wrapper, for example:

```json
{"name":"Heater","unique_id":"heater-1","state_topic":"lab/heater/state","command_topic":"lab/heater/set","command_template":"{\"state\":\"{{ value }}\"}","payload_on":"1","payload_off":"0"}
```

`switch.turn_on` publishes `{"state":"1"}` to `lab/heater/set` using the
entity QoS (or global QoS) and retain setting. The integration leaves reported
state untouched until `lab/heater/state` reports `1`. State is updated after a
service call only when the root `optimistic` option or that discovery entity's
`optimistic: true` explicitly enables it. Optimistic switches publish
`assumed_state: true`; the attribute stays visible while unavailable, and a
toggle from `unknown` requests `on`. Basic-schema lights do not model
brightness/color options; callers that need custom basic-light payloads can
use `command_template` and its service variables.

JSON lights opt into the pinned `schema: "json"` contract. Their discovery can
declare brightness scaling, color temperature units and bounds, supported
color modes, effects, flash, and transition support. `light.turn_on` publishes
the Home Assistant JSON shape (`state`, `brightness`, `color_temp`, `color`,
`effect`, `flash`, and `transition`) to `command_topic`. HS, RGB, RGBW, RGBWW,
XY, white, brightness, color-temperature, and effect input state is parsed
from the reported JSON topic. Light options are filtered according to the
entity's declared capabilities; unknown service options are rejected.

The async command queue is bounded. Queue saturation, unknown target IDs, and
unsupported service options return runtime service errors. For switch and light
entities, published commands do not change reported state unless global
`optimistic: true` or the discovery entity explicitly sets `optimistic: true`.
Climate updates a commanded cache field when it has no state topic or optimism
is enabled. Each queued publication carries its own explicit cache operation,
even when the requested value matched an earlier snapshot. Actor admission
applies that operation once; completion projects the live cache. Concurrent
telemetry can therefore supersede a commanded value before its ACK, and other
state callbacks can expose the updated cache while the ACK is pending. The
tests use a local MQTT wire fixture; they do not contact an external broker or
issue physical device calls.

MQTT service handlers await the publish callback. QoS 0 completes after the
outgoing packet is written, QoS 1 at PUBACK, and QoS 2 at PUBCOMP. The callback
has a ten-second soft timeout: it logs and returns normally without promising
broker delivery. A late acknowledgment still clears its packet-ID record, so
it cannot complete a later command. Switch, light, cover and fan optimistic
updates are applied after callback completion or timeout; climate uses the
cache timing described above. Service results include only states that the
runtime actually changed. Repeating a command that leaves the state
and attributes unchanged returns no changed-state entry. See pinned
[`mqtt/entity.py:2011`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/mqtt/entity.py#L2011),
[`mqtt/client.py:750`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/mqtt/client.py#L750),
and [`mqtt/switch.py:150`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/mqtt/switch.py#L150).

Button presses publish the generic UTC timestamp before the MQTT provider
action. A provider error leaves that timestamp in state, and a successful
service result includes its actual state change. If the connection or entity
becomes unavailable while a publish is pending, completion keeps the latest
reported attributes and the desired state for restore while the live state
remains `unavailable`. When availability returns after reconnect, the remembered
desired state is projected again.

Canceling a service caller does not retract an already queued MQTT command; the
event loop retains its packet tracking and completes the admitted operation,
using the state timing described above. For lifecycle-managed publishers,
completion remains bound
to the original entity handle: a released publisher's late ACK cannot update a
replacement entity using the same discovery topic. Shutdown closes command
admission, resolves pending replies,
sends DISCONNECT when possible, and joins the event-loop worker. If startup is
canceled after the worker connects, its guard signals and reaps that worker.

`subscribe_topic(filter, qos).await` adds a validated MQTT filter and retains
it for reconnect resubscription, for example when native automations register
topic triggers. Every accepted inbound PUBLISH within the configured payload
limit dispatches one private `mqtt_message` signal with `topic`, UTF-8 `payload`
or null, `payload_bytes`, `qos`, and `retain`. Oversized payloads are discarded
before signal dispatch or parsing.
