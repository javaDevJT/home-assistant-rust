# hass-automation

This crate runs bounded Home Assistant automations against `hass_core::Runtime`.
It accepts both `triggers`/`actions` and legacy `trigger`/`action` lists, and
both modern `trigger:` and legacy `platform:` trigger names. Unknown fields and
unsupported trigger platforms are rejected.

```yaml
- id: warm_arrival
  alias: Warm arrival
  mode: restart
  variables:
    message: "{{ 'Welcome home' }}"
  triggers:
    - trigger: state
      entity_id: person.josh
      from: away
      to: home
      for: 2
  conditions:
    - condition: template
      value_template: "{{ is_state('input_boolean.quiet_mode', 'off') }}"
  actions:
    - action: light.turn_on
      target:
        entity_id: light.entry
      data:
        brightness: 180
    - action: notify.phone
      data:
        message: "{{ message }}"

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
```

Triggers include state, numeric state, template, local `time`, `time_pattern`,
Home Assistant startup/shutdown, event, and MQTT message signals. State,
numeric-state, and template triggers accept `for` durations up to 24 hours.
State triggers can observe an attribute; numeric thresholds are exclusive.
Template triggers fire on false-to-true transitions. The time trigger accepts
`HH:MM` or `HH:MM:SS`; time-pattern fields accept an integer, `*`, or `/step`.
The time trigger currently supports fixed local clock values, not entity-backed
times or weekday selectors.

Conditions include state, numeric-state, template, and nested `and`, `or`, and
`not`. Automation and trigger variables are available to the shared template
renderer. Actions are validated and executed by `hass-script`'s shared
sequence runner, including its service, delay, condition, choose, repeat, and
variable actions. Each automation has at most 16 triggers, 16 conditions, and
32 top-level actions. Runs are bounded by the shared executor, and the engine
keeps at most 16 active runs, 128 queued runs, and 128 delayed triggers.
Queued and parallel `max` limits skip excess triggers with a warning; overflow
of the engine-wide pending queue stops the listener and is returned from
`shutdown`.

The engine publishes `automation.<id>` states (`on` or `off`) and registers
`automation.turn_on`, `automation.turn_off`, `automation.toggle`, and
`automation.trigger`. Enable state is stored through Runtime metadata and
restored when the engine starts again. `initial_state` sets the first value;
persisted state takes precedence on later starts.

Use `start_with_timezone(runtime, automations, "America/New_York")` for wall
clock triggers. `start` uses the timezone already configured in Runtime.
`mqtt_topics(&automations)` returns the static broker filters needed by the
root MQTT integration. MQTT triggers consume only private native
`mqtt_message` signals dispatched from real broker traffic; this crate does
not create broker traffic or subscribe directly.

Automation service contexts preserve the triggering user and link to the
triggering context ID. Context lineage prevents an automation from re-entering
itself through chains of other automations. Broadcast lag and queue overflow
are reported as engine errors. Shutdown cancels delayed triggers and active
actions, joins every task, then runs configured Home Assistant shutdown
triggers. Synchronous Runtime service handlers cannot be forcibly interrupted,
so shutdown waits for an in-flight handler to return.
