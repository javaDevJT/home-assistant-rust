# Native scripts and scenes

This crate implements a strict, bounded subset of Home Assistant scripts and
scenes on top of `hass_core::Runtime`. The application may deserialize the
top-level `script:` mapping into `BTreeMap<String, ScriptConfig>` and the
top-level `scene:` list into `Vec<SceneConfig>`, then call
`ScriptEngine::start(runtime, scripts, scenes)`.

```yaml
script:
  welcome_home:
    alias: Welcome home
    description: Turn on the entry light
    icon: mdi:home
    mode: restart
    max: 4
    variables:
      brightness: 160
    sequence:
      - action: light.turn_on
        target:
          entity_id: light.entry
        data:
          brightness: "{{ brightness }}"
      - delay: "00:00:01"
      - choose:
          - conditions:
              - condition: state
                entity_id: input_boolean.quiet_mode
                state: "off"
            sequence:
              - service: input_boolean.turn_on
                target:
                  entity_id: input_boolean.entry_ready
        default:
          - stop: Quiet mode is enabled

scene:
  - name: Reading corner
    id: reading_corner_unique
    icon: mdi:book-open-page-variant
    entities:
      input_boolean.reading_light:
        state: "on"
      input_number.reading_brightness: "65"
```

`SceneConfig.name` is required. The scene entity object ID is the lower-case,
underscore-normalized name; optional `id` is exposed as the scene's unique-ID
attribute, matching pinned Home Assistant scene configuration. `entities`
accepts state strings or maps containing `state` and any saved attributes.
Boolean state values are normalized to `on` or `off`. Optional scene `metadata`
is accepted and retained with the configuration; it does not affect activation.

## Supported actions and conditions

Each script step must contain exactly one of:

- `action: domain.service` or legacy `service: domain.service`, with `target`,
  `data`, or legacy `service_data`.
- `delay` as seconds, `HH:MM:SS`, or a map with hours/minutes/seconds/
  milliseconds.
- `condition` actions for `state`, `numeric_state`, `template`, `and`, `or`,
  and `not`.
- `choose` with ordered `conditions` and `sequence` branches plus optional
  `default`.
- `repeat` with exactly one of `count`, `while`, or `until`, and a `sequence`.
- `variables` to update the current variable map.
- `stop`, optionally with `error: true` to fail the run.

Choose/repeat conditions support state, numeric-state, template, and nested
and/or/not forms. Templates in service data, target values, variables, and
template conditions use the bounded `hass-template` expression interface.
Every struct rejects unknown configuration keys. `for_each`, script `fields`,
`max_exceeded`, blueprints, parallel/if/then action blocks, wait/event/time
triggers, response variables, traces, and dynamic includes are unsupported and
fail strict deserialization or validation.

## Scheduling and limits

- `single` rejects a second invocation while the first is active.
- `restart` cancels prior runs before starting the newest request.
- `queued` executes one run at a time, with `max` counting the active and
  waiting runs.
- `parallel` permits up to `max` active runs.
- `max` must be 2–32. Each action list is limited to 64 steps, total configured
  actions to 256, nested sequence depth to 12, and repeat count to 100.
- A delay is limited to 30 seconds and each script run to 60 seconds. The
  engine has 64 global active-run slots, 128 pending runs/commands, and 16
  concurrent scene applications.

The engine registers `script.<object_id>`, `script.turn_on`,
`script.turn_off`, `script.toggle`, `scene.turn_on`, and `scene.create`. Direct
`script.<object_id>` calls wait for completion; `script.turn_on` acknowledges
accepted runs and returns while they execute. `script.turn_off` cancels active
delays and drops queued runs. A recursion chain that calls a script already in
that chain fails with a clear service error.

## Scenes, persistence, and service limits

`scene.create` accepts `scene_id` with `entities` and/or `snapshot_entities`.
Snapshots preserve current state and attributes under Runtime metadata, so a
created scene survives a restart. Recreating the same dynamic scene replaces
its saved snapshot; configured scenes cannot be overwritten.

Scene activation writes the scene entity's activation timestamp and restores
target entities through their actual registered services. It never injects a
requested state into the target entity. The current built-in service mapping
covers on/off controls, covers, locks, input booleans/numbers/text/selects, and
counters. A target without an applicable registered service fails with the
entity and missing service in the error. Extra saved attributes are passed to
supported device domains where their service can consume them. Transition is
accepted for `scene.turn_on` and forwarded to light services.

`Runtime::call_service` is synchronous, so its handler uses a bounded channel
and waits for actor acceptance/completion for at most 65 seconds. Script
actions call registered services through Tokio's blocking pool. The engine
owns a separate two-worker Tokio runtime, so a synchronous service callback
waiting for actor work does not block that actor. An arbitrary synchronous
handler that has already started cannot be interrupted; cancellation and
shutdown join it and therefore wait for it to return. The global run,
blocking-pool, command, pending-run, and scene-apply limits bound the number of
in-flight operations.

The public `validate_config` performs pure configuration checks for startup
and `--check` flows. `validate_sequence(&serde_json::Value)` and
`run_sequence(runtime, sequence, variables, context, cancel)` expose the same
action validator/executor for other native integrations; `run_sequence`
preserves the caller-supplied causal context.
