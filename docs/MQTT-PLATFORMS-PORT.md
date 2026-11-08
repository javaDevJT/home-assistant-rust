# MQTT Provider Contract: Lock, Humidifier, Siren, Vacuum

Source baseline: Home Assistant Core `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. This is a provider-side source map for the four platforms; it does not replace the native entity SDK contracts owned in their platform port work.

## Shared MQTT contract

All four `async_setup_entry` functions use `async_setup_entity_entry_helper`, and each entity inherits `MqttEntity` plus its native platform entity (`LockEntity`, `HumidifierEntity`, `SirenEntity`, or `StateVacuumEntity`). Per-platform `config_schema()` returns its discovery schema. The adapter hooks are `_setup_from_config`, `_prepare_subscribe_topics`, and `_subscribe_topics`; message handlers mutate provider state, while commands await `async_publish_with_config(topic, payload)` before any provider-side optimistic state write. See `homeassistant/components/mqtt/{lock,humidifier,siren,vacuum}.py` at the pinned revision.

`MQTT_RW_SCHEMA` (used by lock, humidifier, siren) requires `command_topic`; `state_topic` is optional; `optimistic` defaults false and `retain` false. `MQTT_BASE_SCHEMA` (vacuum) requires neither command nor state topic and does not define an optimistic flag. Both inherit QoS `0`, encoding `utf-8`, and optional group from `config.py:27-51`; all four extend the common entity schema, which includes availability, device/origin, entity presentation, and JSON attributes (`schemas.py:71-84,207-222`). Availability payloads default to `online` / `offline` (`schemas.py:87-100`).

Lock, humidifier, and siren discovery schemas use `REMOVE_EXTRA`; vacuum uses `ALLOW_EXTRA` (`lock.py:106`, `humidifier.py:169-173`, `siren.py:88-90`, `vacuum.py:191-194`).

`add_subscription(config_key, callback, tracked_attributes)` registers only a configured non-null topic and applies the entity QoS/encoding. The callback wrapper writes HA state only when one of the tracked attributes changes; a failed value template is logged and ignored (`entity.py:2091-2148`). The shared subscription helper reuses an existing subscription only when topic, QoS, and encoding are unchanged; a rebuilt topic set must include every subscription the entity still wants, because omitted prior subscriptions are removed (`subscription.py:30-81,84-156`).

For component discovery, abbreviated keys are expanded before the provider schema is applied. The `~` base-topic macro is then expanded only at the start or end of top-level keys ending in `topic`, and in availability `topic` entries (`discovery.py:195-204,207-220,266-286,708-736`). The generic expansion test covers abbreviated state/command topics and list/object availability (`tests/components/mqtt/test_discovery.py:2362-2417`).

## Lock

- Schema: `command_topic` required; `state_topic` optional. Defaults are name `MQTT Lock`, `payload_lock=LOCK`, `payload_unlock=UNLOCK`, `payload_reset="None"`; state tokens default to `LOCKED`, `LOCKING`, `OPEN`, `OPENING`, `UNLOCKED`, `UNLOCKING`, and `JAMMED`. `payload_open` is optional. `code_format`, `command_template`, and `value_template` are optional (`lock.py:71-116`).
- A missing state topic implies optimistic mode even when `optimistic` is false: initial state is unlocked and assumed. With a state topic and no explicit optimism, initial state is unknown. A rendered empty message is ignored; `payload_reset` clears all lock-state fields to unknown; one of the seven configured tokens sets the matching state, while any other value leaves the prior state unchanged (`lock.py:158-211`; `test_lock.py:95-156,339-371`).
- State input is passed through `value_template` (which can extract JSON fields); it is not otherwise JSON-decoded. `command_template` gets `value` and `code`. Lock, unlock, and open publish to `command_topic`; after publish, optimistic lock/unlock set `is_locked` true/false, and open sets `is_open` true. `OPEN` is the only advertised feature, and only when `payload_open` exists (`lock.py:173-184,188-233,236-281`; `test_lock.py:227-319,394-432,507-553,645-737`).

## Humidifier

- Schema requires both `command_topic` and `target_humidity_command_topic`. `state_topic`, action/current-humidity topics, target-humidity state topic, and mode state topic are optional. `modes` defaults empty and is inclusive with `mode_command_topic`: configuring either requires both. Defaults: name `MQTT Humidifier`, device class `humidifier`, payloads `ON`/`OFF`, min/max humidity `0`/`100`, and humidity/mode reset payloads `"None"`. Templates are independently configurable for action, on/off state, current humidity, target humidity, and mode, plus each command channel (`humidifier.py:97-184`; core humidity defaults at `homeassistant/components/humidifier/const.py:46-47`). Min must be less than max and max may not exceed 100; mode reset cannot equal a listed mode.
- Main-state optimism is `optimistic || no state_topic`; `assumed_state` reflects this. Target-humidity and mode optimism are computed independently as `optimistic || no corresponding state topic`. Unlike lock and siren, the entity does not initialize its on/off value to false, so no-topic startup remains unknown until an on/off command. Each command publishes first, then updates its optimistic field if its own optimistic flag is true (`humidifier.py:226-280,430-487`; `test_humidifier.py:547-623,752-861`). `MODES` is advertised only for a nonempty configured mode list.
- State/action empty input is ignored; state payload `None` maps to unknown. Humidity reset payloads clear the respective field to unknown. Numeric current/target humidity is rounded to an integer; current humidity must be 0–100 and target humidity must be within configured min/max. Invalid values and invalid/unlisted modes leave the prior field unchanged. Mode reset clears mode; empty mode is ignored (`humidifier.py:282-400`; `test_humidifier.py:177-303`). The four state channels subscribe separately (`humidifier.py:402-427`).

## Siren

- Schema requires `command_topic`; `state_topic` is optional. Defaults: name `MQTT Siren`, payloads `ON`/`OFF`, `support_duration=true`, and `support_volume_set=true`. `state_on`/`state_off` default to the corresponding command payload; `available_tones` is optional (`siren.py:66-107,151-180`).
- TURN_ON and TURN_OFF are always supported. DURATION and VOLUME_SET follow their booleans (both default on); TONES is supported only for a nonempty `available_tones` list. A missing state topic implies optimism and initial off; explicit pessimistic mode with a state topic starts unknown (`siren.py:161-180`; `test_siren.py:133-159,288-416`).
- `state_value_template` may extract/transform input. The receiver accepts a scalar state token or a JSON object. Empty output and `{}` are ignored; `None` sets unknown. A JSON `state` token updates on/off, and duration/tone/volume fields are validated and filtered against advertised features. Unknown scalar tokens or invalid JSON do not update state. The state token is applied before JSON attribute validation, so a valid `state` can change even when a sibling attribute is invalid (`siren.py:198-259`; `test_siren.py:179-285`).
- Without command templates, publish JSON `{ "state": payload, ...turn_on_service_fields }`; turn-off sends the state only. On and off templates both receive `value`; off falls back to the on template and gets no service fields. An empty or literal `None` rendered payload suppresses MQTT publication, but the caller then still performs the optimistic state update. Otherwise publish completes before the on/off and optimistic attribute update (`siren.py:182-201,292-357`; `test_siren.py:778-849`).

## Vacuum

- Uses `MQTT_BASE_SCHEMA`, so `command_topic`, `state_topic`, fan-speed topic, send-command topic, and clean-segments topic are all optional; no provider optimistic mode is defined. Name defaults to `MQTT State Vacuum`; `retain=false`, fan-speed list empty, and supported services default to `start`, `stop`, `return_home`, and `clean_spot`. Configurable action payloads default to lowercase (`start`, `pause`, `stop`, `return_to_base`, `clean_spot`, `locate`). The supported-feature list also accepts `pause`, `fan_speed`, `status`, `send_command`, and `locate`. `STATE` is always added to the feature mask (`vacuum.py:75-127,147-194,247-290`).
- State is raw JSON with no value template. `state` maps `idle`, `docked`, `error`, `paused`, `returning`, `cleaning`, or null; null clears activity to unknown. Unrecognized state strings remain in the attributes. Other JSON fields are merged into state attributes; fan speed is read from the `fan_speed` attribute. Malformed JSON leaves state unchanged in the test fixture (`vacuum.py:292-341`; `test_vacuum.py:662-763`).
- Start/pause/stop/return/spot-clean/locate publish their configured payload to `command_topic`; a missing topic or unsupported service produces no publish. They write HA state after the publish call but do not mutate the reported vacuum activity optimistically. Fan speed publishes a configured listed value directly to its separate topic. `send_command` publishes the command string unless `params` is a dict, in which case it publishes JSON `{ "command": command, ...params }` (a list of params is not serialized). These service/topic contracts are covered at `vacuum.py:359-428` and `test_vacuum.py:125-328`.
- Clean-area is conditional: `clean_segments_command_topic` requires `unique_id`. A nonempty `segments` object on state input plus that topic enables `CLEAN_AREA`; changed segments can create a repair issue. The command publishes the JSON segment-ID list, or passes its list as template `value`. Discovery reconfiguration preserves the feature only if the command topic remains configured (`vacuum.py:147-157,259-273,297-326,343-357`; `test_vacuum.py:329-659`). The schema notably validates `state_topic` with `valid_publish_topic` although the entity subscribes to it (`vacuum.py:160-189,330-341`); decide whether strict parity requires preserving this upstream quirk.

## Vacuum state-topic validation decision

Implementation decision: preserve the pinned publish-topic validator for
vacuum `state_topic`, including its rejection of wildcard subscriptions. A
general MQTT subscribe-topic validator would silently broaden this schema.
This follows the source at `vacuum.py:182`; changing that behavior belongs to
a later upstream-pin update or an explicitly documented native extension.

## Discovery abbreviations used by these providers

Common MQTT discovery keys include `cmd_t=command_topic`, `stat_t=state_topic`, `opt=optimistic`, `ret=retain`, `uniq_id=unique_id`, `dev=device`, `avty=availability`, and `avty_t=availability_topic`. Platform-specific examples from `homeassistant/components/mqtt/abbreviations.py:3-280`:

| Platform | Relevant abbreviated keys |
| --- | --- |
| Lock | `cod_form`, `cmd_tpl`, `pl_lock`, `pl_unlk`, `pl_open`, `pl_rst`, `stat_locked`, `stat_locking`, `stat_open`, `stat_opening`, `stat_unlocked`, `stat_unlocking`, `stat_jam`, `val_tpl` |
| Humidifier | `act_t`, `act_tpl`, `curr_hum_t`, `curr_hum_tpl`, `hum_cmd_t`, `hum_cmd_tpl`, `hum_stat_t`, `hum_state_tpl`, `mode_cmd_t`, `mode_cmd_tpl`, `mode_stat_t`, `mode_stat_tpl`, `min_hum`, `max_hum`, `pl_rst_hum`, `pl_rst_mode`, `modes`, `dev_cla`, `stat_val_tpl` |
| Siren | `av_tones`, `cmd_tpl`, `cmd_off_tpl`, `pl_on`, `pl_off`, `stat_on`, `stat_off`, `stat_val_tpl`, `sup_dur`, `sup_vol` |
| Vacuum | `cln_segmnts_cmd_t`, `cln_segmnts_cmd_tpl`, `fanspd_lst`, `pl_cln_sp`, `pl_loc`, `pl_ret`, `pl_strt`, `pl_paus`, `pl_stop`, `send_cmd_t`, `set_fan_spd_t`, `sup_feat` |

`modes` has the same spelling in both forms. The `~` topic-base marker is a discovery transformation rather than an abbreviation.

## Minimal shared-lifecycle adoption

The current `hass-mqtt` README describes the intended split: MQTT owns discovery, state-topic subscriptions, command queue, reconnect, and removal; providers register native descriptors and project state through the shared entity lifecycle (`crates/hass-mqtt/README.md:94-110`). The narrow extension point is a platform adapter that builds its native `EntitySpec`, registers it through the existing `register_shared_spec` helper to obtain an `EntityHandle`, and keeps MQTT topic callbacks / command publishing in the MQTT provider (`crates/hass-mqtt/src/lib.rs:178-207,2265-2279`). Add the four platforms' feature/state projections and service routing at that adapter boundary; keep entity ID and metadata readback attached to the discovery identity.

For exact parity, preserve each platform's different initial-state and optimism rules, topic presence, and reset/unknown behavior above. Do not treat vacuum's `write_ha_state` after a command as an optimistic activity transition. No Home Assistant code or tests were executed for this source-only investigation.

## Future implementation boundary: four MQTT adapters

The SDK implementations referenced below passed isolated worktree checks but
are outside the native53 source receipt. Their paths and APIs describe the
next enrollment and adapter work; they are not supplied by this snapshot.

**Proposal for work after native53 publication.** This section defines the implementation seam only; these MQTT adapters are not implemented here, and this is not an implementation or parity claim. The native SDKs in `crates/hass-{lock,humidifier,siren,vacuum}` are implemented and checked; do not repeat their source research.

Keep each adapter a direct private child of `hass-mqtt` so it can use parent-private helpers and types, while the parent can see only each adapter's `pub(super)` API:

```rust
#[path = "platform/lock.rs"] mod mqtt_lock;
#[path = "platform/humidifier.rs"] mod mqtt_humidifier;
#[path = "platform/siren.rs"] mod mqtt_siren;
#[path = "platform/vacuum.rs"] mod mqtt_vacuum;

// In each child, with its corresponding SDK descriptor/cache:
#[derive(Clone, Debug)]
pub(super) struct Adapter {
    descriptor: PlatformDescriptor, // concrete types are listed below
    state: Arc<Mutex<State>>,
    handle: Option<EntityHandle>,
    // module-private parsed config; fields remain private to this child
}
// In mqtt_lock.rs, mqtt_humidifier.rs, and mqtt_siren.rs only:
#[derive(Debug)]
pub(super) struct OptimisticMutation { /* private fields */ }
pub(super) fn validate_config(config: &Map<String, Value>) -> Result<()>;
pub(super) fn state_topic_keys() -> &'static [&'static str];
pub(super) fn command_topic_keys() -> &'static [&'static str];
pub(super) fn prepare(
    entity: &DiscoveredEntity,
    previous: Option<&DiscoveredEntity>,
) -> Result<Adapter>;

impl Adapter {
    pub(super) fn register(
        &mut self, entity: &DiscoveredEntity,
        context: &SharedEntityLifecycleContext,
    ) -> Result<()>;
    pub(super) fn handle(&self) -> Option<&EntityHandle>;
    pub(super) fn update_topic(
        &self, runtime: &Runtime, entity: &DiscoveredEntity,
        topic: &str, payload: &str, templates: &TemplateEngine,
        current: LastValue,
    ) -> Result<LastValue>;
    pub(super) fn project(
        &self, entity: &DiscoveredEntity, current: &LastValue,
        available: bool, context: Context,
    ) -> Result<(EntityPublication, EntitySnapshot)>;
    pub(super) fn build_commands(
        &self, runtime: &Runtime, call: &ServiceCall,
        entity: &DiscoveredEntity, templates: &TemplateEngine,
        registries: Option<&Registries>,
    ) -> Result<Vec<Command>>;
}
```

Replace the schematic descriptor field with the exact child-local SDK type: `mqtt_lock::Adapter` stores `hass_lock::LockDescriptor`; `mqtt_humidifier::Adapter` stores `hass_humidifier::HumidifierDescriptor`; `mqtt_siren::Adapter` stores `hass_siren::SirenDescriptor`; and `mqtt_vacuum::Adapter` stores `hass_vacuum::VacuumDescriptor`. `PlatformDescriptor` and `State` above are explanatory placeholders, not declarations to add.

Each optimistic module exposes its `OptimisticMutation` as `pub(super)` with private fields, so root can wrap it in its tagged `PlatformMutation` while only the child interprets it. Vacuum has no such type. `Config` and cache fields stay private. The `Adapter`, stored once in the matching `SharedMqttEntity` variant, owns the one typed cache and optional lifecycle handle; parent code sees the handle only through `handle()`. `LastValue` is the generic runtime projection, not a second platform cache. `previous` lets `prepare` retain compatible typed state during discovery replacement. `prepare` always runs; `register` runs only when lifecycle context exists.

Each `validate_config` checks only that platform's fields and cross-field constraints. The two topic-key lists let root `parse_discovered_entity` use its existing expansion/validation helpers and fill `state_topic`, `extra_state_topics`, `command_topic`, and `extra_command_topics`; `reconcile_subscriptions` already subscribes the resulting state-topic map and removes stale topics. Preserve the vacuum `state_topic` publish-topic validation decision above. `register_mqtt_device` is an existing helper (`lib.rs:2559`); with lifecycle context, each adapter builds its SDK descriptor, calls that helper as needed and the existing `register_shared_spec`, then stores the returned handle. Without context, keep the prepared adapter usable through ordinary `MqttIntegration::start`: subscriptions, state updates, command dispatch, and SDK snapshot projection still run, while root writes the returned `EntitySnapshot` to `Runtime` instead of publishing through a lifecycle handle. The SDKs are implemented and checked: `LockDescriptor`/`LockSnapshot`, `HumidifierDescriptor`/`HumidifierSnapshot`, `SirenDescriptor`/`SirenSnapshot`, and `VacuumDescriptor`/`VacuumSnapshot` provide the lifecycle projection pieces (`crates/hass-lock/src/lib.rs:238-379,406-667`; `crates/hass-humidifier/src/lib.rs:263-503,535-669`; `crates/hass-siren/src/lib.rs:393-607`; `crates/hass-vacuum/src/lib.rs:287-479,529-610`).

Topic handlers are synchronous event-loop updates: parse/template/validate the platform topic into its typed cache and return the matching `LastValue`. Projection builds the SDK snapshot from that cache, availability, extra attributes, and causal context, then returns both publication and snapshot. Service builders receive one entity already chosen by root target resolution and use the SDK pure plans: `LockCommand::plan_service_call(call, descriptor, snapshot, default_code)`, `HumidifierServiceRequest::parse_service_call(call)?.resolve(descriptor, snapshot)`, `SirenServiceRequest::parse_service_call(call)?.resolve(descriptor)`, and stateful `VacuumServiceRequest::parse_service_call(call)?.resolve(descriptor)` (`crates/hass-lock/src/lib.rs:744-864`; `crates/hass-humidifier/src/service.rs:240-354`; `crates/hass-siren/src/lib.rs:888-1058`; `crates/hass-vacuum/src/service.rs:192-290`). Modules translate these plan effects into MQTT topic/payload commands; they do not reimplement validation, features, service semantics, target resolution, or I/O. Lock's `default_code` is read at service time from the current `Registries::entity(mqtt_entity_id(entity)).options[hass_lock::CONF_DEFAULT_CODE]`; root captures optional `Registries` in the registered service closure and passes it through. Standalone mode supplies `None`.

Root alone wires the modules: discovery validation/topic-key dispatch; four `SharedMqttEntity` variants and handle lookup; always prepare adapters during discovery and register them only when lifecycle context is present; platform arms in `update_reported_state`, `shared_entity_projection`, and the standalone `Runtime` projection fallback; and service dispatch from `build_entity_commands`. Use this root-owned envelope:

```rust
#[derive(Debug)]
enum PlatformMutation {
    Lock(mqtt_lock::OptimisticMutation),
    Humidifier(mqtt_humidifier::OptimisticMutation),
    Siren(mqtt_siren::OptimisticMutation),
}
// Add to the existing optimistic command envelope:
platform_update: Option<PlatformMutation>,
fn make_platform_command(
    runtime: &Runtime, entity: &DiscoveredEntity, call: &ServiceCall,
    topic: String, payload: String, publish: bool,
    platform_update: Option<PlatformMutation>,
) -> Result<Command>;
```

`Vacuum` has no optimistic mutation variant. The helper preserves payload limits, QoS, retain, and lifecycle target handle while adapters decide each channel's optimism. Consume `platform_update` only in `complete_publish`, after publish completion; the existing local-completion path must also reach it when a Siren template suppresses publication (`publish: false`). This preserves the source phase: lock, humidifier, and siren mutations are post-publish-completion operations, except suppressed Siren payloads which use local completion; climate retains its separate actor-time mutation and live-cache ACK projection; vacuum never optimistically changes activity. Keep `enqueue_service_commands`, `queue_command_chains`, `complete_publish`, timeout handling, target admission, QoS, and lifecycle lease checks centralized (`crates/hass-mqtt/src/lib.rs:775-818,1173-1202,1247-1368,3937-4259,4266-4378,4983-5025`). No adapter owns another queue, ACK path, lease, availability policy, diagnostic receipt, or entity-id readback.

Focused wire acceptance can be split into `tests/wire_lock.rs`, `wire_humidifier.rs`, `wire_siren.rs`, and `wire_vacuum.rs`, each using one root-extracted `tests/common` broker harness from `tests/wire.rs`. Cover both `MqttIntegration::start` (no lifecycle handle; snapshot still reaches `Runtime`) and the lifecycle-enabled start for registration/readback. For each, verify discovery rejection/defaults, configured subscriptions and stale-topic removal, valid/invalid state-topic updates, planned service payload on the broker, unsupported service/feature behavior without a publish, and source-specific reset/unknown rules. With QoS 1, assert lock/humidifier/siren state stays unchanged before PUBACK and projects after completion; for siren also assert empty/`None` template payload makes no publish but does update state on local completion. Assert vacuum commands never change reported activity. These are proposed checks only; no broker or Home Assistant tests were run for this section.
