# Pinned upstream parity inventory

The machine-readable source of truth is [upstream-parity.json](upstream-parity.json). It records every integration manifest at Home Assistant Core commit cbf2c02c9bff331537c19bff69da8dc11c3fbfbf on upstream/dev. The inventory is a porting map; it does not claim that upstream integrations or Core behavior have been ported.

## Pinned scope and extraction

| Measure | Count |
| --- | ---: |
| Tracked upstream blobs | 30,403 |
| Python files | 20,410 |
| Integration manifests | 1,565 |
| Files in manifest-backed component directories, including manifests | 15,089 |
| Other source/resource paths in those component directories | 13,524 |
| Python source paths in those integrations | 10,733 |
| Files under the matching tests/components/domain directories | 13,958 |
| Python test paths under those directories | 8,784 |
| Integrations with Python source | 1,541 |
| Integrations with Python tests | 1,248 |
| Integrations with statically recognized PLATFORMS values | 866 |

The scan used one recursive tree listing and one git cat-file --batch stream for all 1,565 manifest blobs and 10,734 component Python blobs, including the shared components package initializer. No requested manifest or Python-source object was missing. It did not fetch each manifest separately. A static AST pass found no Python parse failures. Test and non-Python resource contents were not read; their tracked paths are inventoried.

Each JSON integration record contains the parsed manifest, immutable manifest object ID, every component file path relative to its component directory, every test file path relative to tests/components/domain, static setup hooks, config-flow classes, statically recognized PLATFORMS expressions, imported Python roots, protocol-family evidence, and its primary batch. Dynamic declarations are left for source review; seven non-platform values found in PLATFORMS assignments are retained as unresolved values rather than counted as platforms. Requirements retain their pinned package strings. Dependency and after-dependency edges are also listed separately. There are 564 direct dependency edges to 73 targets, 148 after-dependency edges to 48 targets, and 1,276 requirement references across 1,194 exact package specifications. Forty-six exact package specifications are reused by two or more integrations.

Five Python files under tests/components are shared test support rather than integration tests; 354 shared source paths sit outside homeassistant/components. Both shared path sets are included in the JSON. Twenty-four manifest domains have no Python source, and 317 have no Python test. The JSON lists each domain in those groups.

Protocol tags are evidence-led: each needs a matching manifest selector/dependency, requirement, integration name, or imported module. Tags overlap. They identify useful shared-port candidates, not proof that all members have identical wire behavior. The primary batches are a disjoint partition of all 1,565 integrations.

## Current Rust coverage

| Status | Integrations | Meaning |
| --- | ---: | --- |
| Complete | 0 | No entire upstream integration is accepted as full parity. |
| Partial | 38 | Rust source implements an identifiable subset; the full upstream behavior and acceptance remain open. |
| Unported | 1,527 | No behaviorally accepted native adapter is evidenced for the manifest domain. |

The partial domains are api, auth, automation, config, counter, device_tracker, group, homeassistant, http, image_upload, input_boolean, input_button, input_datetime, input_number, input_select, input_text, logger, mqtt, persistent_notification, person, recorder, repairs, rest, rest_command, scene, schedule, script, sun, system_health, system_log, tag, template, time_date, timer, uptime, webhook, websocket_api, and zone. Native groups, generic controls, notification and webhook triggers, tracker/person/zone pipelines, image upload/serving/profile selection, shared entity lifecycle, clock providers, template entities, MQTT config-entry/registry lifecycle, REST lifecycle, recorder maintenance, typed config-subentry flows and persistence, a native Repairs registry/fix-flow/API/host subset, and the native System Health provider/transport subset have focused acceptance evidence. The JSON retains initial source-export/line-count snapshots alongside updated port status and evidence paths; exact primary checks and revisions are recorded in [PORTING.md](PORTING.md) and [VALIDATION.md](VALIDATION.md). Source presence alone is never full acceptance.

System Health now has an explicit native provider SDK and authenticated `system_health/info` WebSocket stream over the existing API dispatcher, with no added public Runtime listener or HTTP route. The current subset covers weak-owner registration and replacement, concurrent five-second callback collection, typed dates and deferred pending/update/finish frames, generic client errors with detailed internal `get_info`, a five-second loopback GET helper, dropped-future and transferred-task cancellation, joined stream cleanup, exact two-key command validation, authenticated admin/ordinary/read-only access, and a native core version/current Runtime timezone provider. The compiled-process fixture confirms the saved timezone across a process restart. Empty configuration accepts values and warns on truthy parameters without echoing them. This remains partial: native provider coverage is limited to the core callback; Python/platform discovery, frontend, LLM integration and metadata, full pinned fixtures, and real external health probes are open. Arbitrary provider values remain source-compatible for authenticated readers without blanket redaction.

The published 27-crate checkpoint passed 578 tests across 71 targets, formatting, strict locked all-target Clippy, release build and optimized CLI validation. The frozen 28-crate workspace gate passed `cargo fmt --all --check`, strict locked workspace all-target Clippy, 617 tests across 75 targets (0 failed, 0 ignored), release build and optimized CLI validation. Root reports that this gate includes focused suites covering 23 Repairs, 18 config, 32 registry, 2 Repairs API, 1 config-subentry API and 2 host lifecycle cases, plus the compiled-process Repairs check for storage, events, confirmation and restart. The config-subentry API case passes through fixture teardown. These receipts are separate from the inventory writer's source scan (`test_execution_by_inventory_writer` remains false). The published checkpoint includes Sun's 26 native cases, 39 automation cases, 10 script cases and both compiled-process checks. Independent host and time-event/condition reviews led to corrections for enable/restart, range selectors and units, short/fractional offsets and trigger metadata. Actual UTC actions expose id/idx/alias correctly. Exhaustive pinned Sun fixtures and typed timedelta template methods remain open; `Drop` is best-effort, with joined cleanup through `shutdown().await`.

Published 29-crate System Health source `2724b8e4a68b8b8cd29124afa0f5da42c2b51db1` passes formatting, strict locked workspace all-target Clippy, 636 tests across 78 targets (0 failed, 0 ignored), optimized build/CLI validation and [exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42342899022). Focused System Health evidence includes 12 SDK cases, 4 API protocol cases, 2 app cases and 1 actual-process health case. The optimized CLI accepts truthy configuration with a generic diagnostic, no parameter-value echo and no database creation.

The following cleanup-journal candidate still has 29 crates. It passes formatting, strict locked workspace all-target Clippy, 644 tests across 78 targets (0 failed, 0 ignored), optimized build and optimized CLI validation; publication receipts follow separately. All 25 config cases pass, including atomic minimal removal records, exact-ID durable acknowledgements, parent/child coalescing, callback/ack failure retry and overlapping/reentrant replay. Three app host cases include actual SQLite write failure and cold reopen for parent and child removal, preserving sibling/other-parent metadata and foreign current state. A red/green regression prevents a stale child update after reentrant parent removal. App startup installs the actual registry/Repairs observer before replay. Cleanup replay is separate from public event delivery, and writes across documents remain non-atomic. Exhaustive pinned acceptance and full parity remain open.

System Log's nine native cases and actual API/process checks pass config/service coercions, zero retained entries, common-caller cross-level deduplication, first critical severity and message order, regex filtering, raw arrays, unsubscribe, unconfigured native integration filters and process-local restart. Anchored backlog headers preserve multiline filtering. Optional event loss and persistence errors are observable. Real child-process TRACE and API DEBUG fixtures prove transport-only raw echo suppression while genuine DEBUG service writes remain visible. LLM `get_entries`/`get_logs` consumers remain absent; native exceptions retain actual callsite and explicit exception data without fabricated Python frames. The app has no file backlog unless a reader is registered, and bounded event dispatch differs from lossless source scheduling. Exhaustive pinned acceptance remains open.

Logger's 16 cases pass genuine LogTracer target/regex handling despite a conflicting logger named `log`, distinct critical/error severity, original span/event/TRACE fields and transport context under independent logger thresholds. The published 25-crate revision `216bb1f1271e8f2cf7dd6ae4c4161cda4df34bff` separately records its historical 517-test/66-target receipt; current 27-crate evidence is recorded above and in VALIDATION.md. Rust regex still lacks Python lookaround/backreference support; attribution, renamed-logger repairs, remaining loader/discovery/frontend behavior and exhaustive pinned acceptance keep Logger partial.

Tag has focused native coverage for persistent CRUD and names, registry-backed entities, the public `tag_scanned` callback and automation triggers. Authenticated WebSocket commands enforce roles and strict fields and deliver private initial/live subscriptions. YAML accepts `tag: null` or `tag: {}` only. Fifteen native unit cases cover normal operations, removed entities and injected failure compensation; a real SQLite case proves reopen behavior. Two actual WebSocket cases cover tag commands and composite inventory/error policy. The compiled process checks CRUD, names, stable identity, HTTP history, event automation, hard restart and deletion cleanup.

Composite migration corrections retain all legacy owner/subentry associations, split deleted identities, replace copied identifiers once and resolve parent aliases using the actual config-entry domains. Failure tests preserve current and removed tag identities, restore raw names and leave concurrent edits and foreign state intact. The frozen 24-crate gate passes 499 tests across 63 targets, strict lint, formatting, optimized build and CLI validation. Exhaustive registry and integration acceptance remain open.

Tag history omits `tag_id` while current state and events retain it. Native generic event history still records `state_changed`, unlike Home Assistant Recorder's state-row route; custom stores must override `Store::put_state_with_history` to honor the projection. Complete Recorder parity remains open. The pinned Tag source defines no dedicated scan HTTP/service/WebSocket endpoint; broader NFC/mobile-app reader flows remain outside this adapter.

## Core work that gates the integration waves

These shared systems need pinned-source behavior and acceptance evidence before the integration catalogue can be considered usable.

| Order | Core subsystem | Remaining work |
| --- | --- | --- |
| 0 | Startup, loader, configuration entries and discovery | Native typed subentry models, persistence, flows, lifecycle observations and a focused HTTP/WebSocket subset now exist. Extend accepted native config/options flows and setup/unload/retry/migration with manifest-aware validation, dependency ordering, remaining discovery/platform flows, exhaustive pinned subentry errors and complete rollback/reload contracts. YAML startup accepts a limited native subset. |
| 1 | Entities and platform lifecycle | Entity-specific history policy now omits Tag's `tag_id` from history while preserving live state and event payloads. Extend shared identity, metadata, leases, registry linkage, restore and unload with generic platform setup, polling/coordinator ownership, full capabilities/domain behavior and exhaustive failure/parity cases. Actual template and clock providers already use the lifecycle. |
| 2 | Registries | Extend accepted CRUD, identity, event and composite migration behavior to remaining child-device semantics, categories, registries and exhaustive pinned payloads. Preserve complete owner/subentry associations, guarded rollback, foreign references and event ordering. |
| 3 | Users, auth providers and permissions | Extend accepted local password/session/JWT/TOTP behavior with remaining providers, custom permissions and all pinned command/service authorization cases. |
| 4 | Runtime, storage and recorder | SQLite reopen evidence proves `tag_id` is omitted from Tag state-history while current state and full event payloads retain it. Native generic event storage still records `state_changed`, unlike Home Assistant Recorder's state-row route; the default `Store::put_state_with_history` ignores the projected snapshot, so custom stores that persist history must override it. Complete recorder ingestion/filtering, statistics, repairs, migrations and every pinned query contract remain open. |
| 5 | REST, HTTP security and WebSocket APIs | Repairs and config-subentry HTTP/WebSocket subsets now have transport fixtures. System Health also has a strict authenticated `system_health/info` stream with callback cancellation and admin/ordinary/read-only access. Complete the remaining endpoint, middleware, error, message and authorization parity. Native inventory now counts actual public wildcard receivers; consumer filters are local. |
| 6 | Automation, scripts, scenes, templates and native helpers | Native `tag_scanned` automation triggers now have callback, context, metadata and composite-ID expansion evidence. Expand behavior from pinned tests for remaining triggers, conditions, actions, execution modes, scenes, template helpers, resource limits, timers, buttons, datetimes and schedule transitions. |
| 7 | Frontend, voice/media and system workflows | Repairs has a native issue registry, fix-flow manager, host lifecycle and limited HTTP/WebSocket adapters. System Health has a native provider SDK and core version/timezone callback, with persisted timezone verified across a process restart; Python provider discovery, frontend/LLM integration and full pinned acceptance remain open. Frontend assets/translations, Assist and media pipelines, backup/restore, diagnostics and remaining onboarding workflows remain separate Core gaps; complete Repairs parity and exhaustive pinned acceptance remain open. |

The JSON has 14 core-gap records with pinned source/test anchors, local evidence paths and focused next acceptance. It also inventories shared protocol families that have no accepted generic native transport yet.

## Ordered integration batches

Batch numbers define the recommended sequence. The JSON carries exact domain membership and source/test path counts for every batch.

| Batch | Candidate scope | Domains |
| --- | --- | ---: |
| 01 | No declared package/protocol requirement; audit for HA-native behavior | 287 |
| 02 | Bluetooth and BLE | 72 |
| 03 | Zigbee | 1 |
| 04 | Z-Wave | 2 |
| 05 | Matter and Thread | 3 |
| 06 | MQTT | 17 |
| 07 | HomeKit | 1 |
| 08 | UPnP and SSDP | 46 |
| 09 | Modbus, serial and USB | 47 |
| 10 | Infrared and RF | 20 |
| 11 | DHCP, Zeroconf and mDNS discovery | 188 |
| 12 | Camera, media and streaming | 18 |
| 13 | Conversation, voice and LLM clients | 20 |
| 14 | Calendar, weather and location feeds | 20 |
| 15 | HTTP, cloud, OAuth and webhook clients | 314 |

Batch 16 has 10 shared-library batches covering 27 remaining integrations. Batch 17 has six integration-type batches covering 321 integrations. Batch 18 has 161 vendor-specific long-tail integrations. Together, these disjoint batches cover exactly 1,565 manifests. The JSON lists each repeated requirement, integration-type grouping, and domain membership.

For each batch, port the reusable lifecycle/protocol layer first and then the listed domain adapters. Acceptance should exercise setup and unload, inbound state and outbound service behavior where applicable, reconnect and failure handling, stable identity, config and discovery semantics, and the pinned test expectations. Network work uses local protocol fixtures; no physical device or production credential is needed.

