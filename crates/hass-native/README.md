# Native helper integrations

`hass-native` provides native timer, input button, input datetime, and weekly
schedule entities. It registers service handlers on `hass_core::Runtime` and
persists state through the runtime's store; it does not control physical
devices.

## Configuration

`NativeConfig` uses the same flat integration maps as Home Assistant YAML. The
root application supplies `time_zone` from `homeassistant.time_zone`.

```yaml
timer:
  tea:
    name: Tea timer
    duration: "00:05:00"
    restore: true
input_button:
  doorbell:
input_datetime:
  weekday_alarm:
    name: Weekday alarm
    has_date: true
    has_time: true
    initial: "2026-11-06 07:30:00"
schedule:
  work_hours:
    name: Work hours
    monday:
      - from: "09:00"
        to: "17:00"
        data:
          mode: work
    tuesday: []
```

Timer durations accept numeric seconds, `HH:MM` / `HH:MM:SS` strings, and
component maps with `days`, `hours`, `minutes`, `seconds`, or `milliseconds`.
Schedule ranges use local wall time, allow `to: "24:00"`, and reject overlaps
within a weekday. Schedule `data` values must be JSON scalars.

Unknown fields, invalid IDs, invalid IANA time zones, malformed dates/times,
and invalid or overlapping ranges return `hass_core::Error::InvalidInput` from
`NativeConfig::validate()` before startup reads or mutates runtime state.

## Services and state

- `timer.start`, `timer.pause`, `timer.cancel`, `timer.finish`, and
  `timer.change` operate on explicit `entity_id` targets. Timer lifecycle events
  follow the `timer.started`, `timer.restarted`, `timer.paused`,
  `timer.cancelled`, `timer.changed`, and `timer.finished` names. Timers with
  `restore: true` persist restart metadata.
- `input_button.press` stores a UTC ISO timestamp as the entity state and emits
  the runtime's `state_changed` event. A fresh button starts as `unknown`; the
  last press timestamp is restored with runtime state.
- `input_datetime.set_datetime` accepts `date` and/or `time`, or one of
  `datetime` or `timestamp`. It preserves any enabled component not supplied by
  the call and validates values before updating targets.
- `schedule.get_schedule` returns the selected schedule's seven weekday lists,
  including each range's `from`, `to`, and optional `data`. Schedule entities
  update at timezone-aware start/end boundaries and publish `next_event`.

`NativeIntegrations::shutdown()` signals and joins its timer and schedule
workers. YAML reload services and editable websocket CRUD for native schedules
are outside this implementation.
