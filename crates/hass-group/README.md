# hass-group

`hass-group` ports the pinned Home Assistant legacy `group:` integration to
the native Rust runtime. It owns `group.<object_id>` aggregate states and the
`group.set` / `group.remove` services. Platform groups such as `light.group`
remain unsupported until their actual providers are ported.

The upstream reference is `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.

## Configuration

`GroupConfig` is strict: object form accepts `name`, `entities`, `icon`, and
`all`, and rejects additional keys. Pinned legacy shorthand is also accepted:
a single entity ID or a list of entity IDs means `entities`. IDs are normalized
to lowercase when validated. The public pure validator rejects malformed IDs,
oversized configurations, unknown fields during deserialization, and cycles
before the integration writes any state.

```rust
use hass_group::{GroupConfig, GroupIntegration};
use std::collections::BTreeMap;

let groups = BTreeMap::from([(
    "downstairs_lights".to_owned(),
    GroupConfig {
        name: Some("Downstairs lights".to_owned()),
        entities: vec!["light.kitchen".to_owned(), "light.hall".to_owned()],
        icon: Some("mdi:lightbulb-group".to_owned()),
        all: false,
    },
)]);
let integration = GroupIntegration::start(runtime, groups).await?;
```

Use `GroupIntegration::start_with_order(runtime, groups, yaml_key_order)` when
the caller retains the original YAML mapping order. That order is exposed in
each state's `order` attribute; dynamic groups follow configured groups.

## Runtime behavior

- Group output includes ordered `entity_id` membership, `order`,
  `friendly_name`, optional `icon`, and `auto: true` for service-created
  groups. It reports `assumed_state` when the configured any/all mode says the
  tracked members are assumed.
- `all: true` requires all currently tracked members to be active; omitted or
  unavailable states follow the legacy aggregate rules. Empty groups and
  groups with no recognized active/off state report `unknown`.
- Legacy `home`/`not_home`, `open`/`closed`, `locked`/`unlocked`, and other
  pinned domain mappings are preserved. Nested groups update in dependency
  order from real runtime state-change events.
- `group.set` supports object ID, name, icon, all, and one of `entities`,
  `add_entities`, or `remove_entities`. `group.remove` removes the active
  group. YAML groups remain configuration-owned.
- Service-created groups are live for the current run and disappear after a
  restart, matching the pinned upstream service path (`components/group/services.py:101-128`);
  upstream marks them with `auto: true` (`components/group/entity.py:193,290-299`)
  but does not save membership as config-entry data. Runtime metadata stores
  only the IDs of groups this integration owns so stale restored state rows
  can be removed at startup. YAML definitions overwrite stored state rows and
  remain the source of membership after restart.
- One bounded event listener serves all groups. Shutdown cancels and joins it,
  unregisters owned services, and invalidates membership lookups. Dropping the
  integration performs the same cleanup synchronously and aborts the listener.
- `GroupLookup` is a weak, runtime-free view of active native groups.
  `expand_entity_ids(runtime, lookup, ids)` expands active groups first, then
  follows pinned legacy `group.*` state `entity_id` attributes, while keeping
  unknown ordinary IDs.

Members, groups, expansion depth, and expansion work are bounded. The crate
does not create registry state, registry rows, or additional stores.
