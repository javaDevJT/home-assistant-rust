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
Home Assistant startup/shutdown, event, tag scans, MQTT message signals, and
persistent notification changes. Tag triggers accept a scalar or list of
`tag_id` values and an optional scalar or list of `device_id` values. They
listen for public `tag_scanned` events emitted by tag providers; the trigger
variables include the event, `description: "Tag scanned"`, and the configured
`id`, `idx`, and `alias`. The scan event's context is passed to the automation
action. When the application supplies its shared registry through
`AutomationFeatures`, legacy composite device IDs expand to their active split
devices; unknown IDs remain as configured. This crate does not add a scan
service, WebSocket command, QR route, or hardware transport. Notification
triggers support optional `notification_id`
and scalar or list `update_type` filters (`current`, `added`, `updated`, or
`removed`). Empty IDs and update-type lists do not filter. The native notification
feed does not replay existing items or synthesize `current` updates when the
automation engine starts; each changed notification in a bulk dismissal gets
its own trigger run. Notification changes arrive only through the private
`persistent_notifications_updated` signal. State,
numeric-state, and template triggers accept `for` durations up to 24 hours.
State triggers can observe an attribute; numeric thresholds are exclusive.
Template triggers fire on false-to-true transitions. The time trigger accepts
`HH:MM` or `HH:MM:SS`; time-pattern fields accept an integer, `*`, or `/step`.
The time trigger currently supports fixed local clock values, not entity-backed
times or weekday selectors.

Sun triggers use the pinned `hass-sun` calculation and Runtime state feed.
Legacy `platform: sun` sunrise/sunset triggers accept `event` and a signed
`offset`; duration strings accept `HH:MM` or `HH:MM:SS` with fractional
seconds to microsecond precision. The legacy form does not accept
`offset_type`. Registry event triggers accept `offset_type` inside `options`,
where `before` negates the configured offset. Solar event and elevation trigger
actions expose generic `trigger.id`, `trigger.idx`, and `trigger.alias` fields;
`id` defaults to the trigger's zero-based index string, and an omitted alias is
`null`. Configured `id` and `alias` values must be strings. The string form of
a legacy `trigger.offset` follows Python's `timedelta` day and microsecond
formatting.

The `sun.elevation_changed` and `sun.elevation_crossed_threshold` triggers
observe `sun.sun` `state_changed` transitions and its published `elevation`
attribute; the Sun host rounds that attribute to two decimals before
publishing it. Both triggers require a real old and new entity state, matching
the pinned numeric entity trigger base, so entity creation, removal, unknown,
and unavailable states do not trigger. Elevation thresholds support numeric
or entity bounds, `any` only for the changed trigger, and inclusive
`between`/complementary `outside` ranges. Entity-backed bounds are active only
when their Runtime state has a numeric value and a degree (`°`) unit.
Crossed-threshold `for` timers cancel when the current Sun elevation no longer
satisfies the configured range. They use the same bounded `for` scheduler and
shared script action runner as other automation triggers.

The `sun.elevation` condition follows the pinned `NumericThresholdSelector`
`mode: is` and `EntityNumericalConditionBase`: `above`/`below` use `value`,
while `between`/`outside` use `value_min` and `value_max`. A bound uses either
`number` or `entity`; entity thresholds require a numeric Runtime state in
degrees. `between` includes both endpoints, and `outside` excludes both. This
follows Home Assistant Core `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`
`components/sun/condition.py`, `helpers/selector.py`,
`helpers/condition/entity_condition.py`, `components/sun/trigger.py`, and
`helpers/trigger/entity_trigger.py`, `helpers/config_validation.py`, and
`helpers/trigger/models.py`.

Verification checkpoint: `cargo test -p hass-automation --locked --offline`
passed all 39 tests; strict package Clippy and formatting checks passed. Sun
regressions include `legacy_and_registry_sun_trigger_yaml_use_pinned_names`,
`sun_trigger_and_condition_offsets_accept_clocks_and_preserve_microseconds`,
`sun_offset_string_matches_python_timedelta_format_and_precision`,
`sun_trigger_metadata_uses_the_pinned_string_schema`,
`sun_elevation_thresholds_use_selector_inclusive_and_exclusive_edges`,
`sun_elevation_condition_matches_numeric_threshold_selector_contract`,
`sun_elevation_changed_uses_sun_state_attribute_transitions`,
`sun_elevation_crossing_ignores_creation_and_cancels_for_on_exit`, and
`sun_scheduler_delivers_legacy_trigger_data_through_shared_action_runner`.
The root-owned compiled-process sunrise fixture passed before this correction
pass; the root owner will rerun it after integration.

Conditions include state, numeric-state, template, Sun elevation, and nested
`and`, `or`, and `not`. Automation and trigger variables are available to the
shared template renderer. Actions are validated and executed by `hass-script`'s
shared sequence runner, including its service, delay, condition, choose, repeat,
and variable actions. Each automation has at most 16 triggers, 16 conditions,
and 32 top-level actions. Runs are bounded by the shared executor, and the engine
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
`start_with_features(runtime, automations, features)` accepts an optional
timezone, the shared webhook manager, and the shared registries while keeping
the existing constructors available.
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
