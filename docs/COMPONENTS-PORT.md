# Loaded component receipts

The source contract is pinned to Home Assistant Core
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.

## Source behavior

- `components/api/__init__.py:480-488` exposes `hass.config.components` through
  authenticated `GET /api/components`.
- `core_config.py:493-539` keeps explicit bare and dotted names. A bare name is
  also a top-level integration. For `integration.platform`, `all_components`
  includes the integration prefix only when the complete suffix belongs to the
  generated base-platform set. Dotted API names remain intact.
- `setup.py:459-461` records successful component setup;
  `helpers/entity_platform.py:553-574` records successful forwarded platform
  setup. Service registration, a declared entry and a loader-cache entry are
  separate facts.
- Forwarded entry setup first ensures the base platform is set up
  (`config_entries.py:3025-3053`). Producers explicitly record the base platform
  as well as the dotted child receipt; the set itself does not infer the base
  name from a dotted name. Config-entry sensor setup also records its owning
  integration, while a YAML child-platform receipt remains a separate name.
- Entry unload does not remove these names. They describe successful module
  setup during the process and are rebuilt after restart.
- Logger reporting uses `all_components`, supplemented by in-progress discovery
  handlers and explicitly configured integration log settings
  (`setup.py:655-658`, `components/logger/websocket_api.py:32-59`).

## Native foundation

`Runtime::loaded_components()` returns the shared `LoadedComponents` handle.
`record_successful_component` adds an explicit receipt; `components` and
`all_components` return sorted snapshots. Runtime clones share receipts, and a
fresh runtime starts empty. No service-name inference, entry-state filtering,
provider-unload deletion or storage restoration is performed.

The five Core integration tests cover canonical and dotted names, idempotence,
retention, runtime isolation and concurrent writers. The complete Core package
also preserves its existing service, state and ownership checks.

The application now supplies explicit receipts after successful native host
initialization. MQTT records supported base and dotted platform names after
successful registration; Spotify records its media-player child after player
registration. Time/date and uptime record their sensor base and child names
after provider setup. Entry unload preserves the receipts.

`ApiFeatures.loaded_components` enables dynamic `GET /api/components`, HTTP
config and WebSocket config snapshots. The main application supplies it. An
embedded caller without the optional provider keeps its existing config JSON;
the component endpoint uses its supplied static names or an empty list.

The logger filters its canonical domain-to-Rust-namespace catalog through
`all_components`. SDK namespaces remain distinct from the MQTT namespace;
listing them does not claim those pure SDKs currently emit tracing events.
In-progress discovery-handler supplementation requires an exact native seam and
must not be approximated by listing every registered flow handler or currently
loaded entry. Broader producer coverage and component-loaded event parity remain
separate work. Expanded acceptance is recorded in VALIDATION.md.
