# hass-automation

This crate implements a strict, bounded subset of Home Assistant YAML
automations. It supports state and event triggers, state and numeric-state
conditions, service actions, and delays up to 30 seconds. Unknown fields and
unsupported options are errors.

```yaml
- id: announce_arrival
  alias: Announce arrival
  triggers:
    - trigger: state
      entity_id: person.josh
      from: not_home
      to: home
  conditions:
    - condition: state
      entity_id: input_boolean.quiet_mode
      state: "off"
    - condition: numeric_state
      entity_id: sensor.outdoor_temperature
      above: 10
      below: 30
  actions:
    - action: notify.phone
      data:
        message: Someone arrived home
    - delay: "00:00:02"

- id: react_to_door_event
  triggers:
    - trigger: event
      event_type: door_opened
      event_data:
        source: front_door
  actions:
    - action: light.turn_on
      target:
        entity_id: light.entry
      data:
        brightness: 180
```

`entity_id` on a state trigger accepts one entity or a list. A state trigger
matches only changes in the state's value; attribute-only updates do not
match. `from` and `to` are exact strings. Event data matching requires every
configured top-level key/value to match, while additional event data is
allowed. All configured conditions must pass. Numeric bounds are exclusive.

Service actions accept `action: domain.service`, and optional object-valued
`data` and `target`. Delays accept a number of seconds, `HH:MM:SS` text, or an
object with `hours`, `minutes`, `seconds`, and/or `milliseconds`. Each delay
is limited to 30 seconds. An automation may have at most 32 actions, 16
triggers, and 16 conditions. The engine runs at most 16 automations at once,
queues at most 128 additional runs, and bounds each run to 60 seconds.

Automation-generated service contexts preserve the triggering user and set
their parent to the triggering context ID. Context lineage prevents an
automation from re-entering itself through chains of other automations. The
engine reports broadcast lag and queue overflow as errors instead of
silently dropping work. Shutdown cancels delays and joins active work. Since
the runtime service handler API is synchronous and has no cancellation
contract, shutdown waits for any in-flight handler to return; the 60-second
run limit cannot forcibly terminate a handler that blocks indefinitely.
