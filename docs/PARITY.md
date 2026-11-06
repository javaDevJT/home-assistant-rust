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
| Partial | 22 | Rust source implements an identifiable subset; the full upstream behavior and acceptance remain open. |
| Unported | 1,543 | No behaviorally accepted native adapter is evidenced for the manifest domain. |

The partial domains are api, auth, automation, config, counter, http, input_boolean, input_button, input_datetime, input_number, input_select, input_text, mqtt, recorder, rest, rest_command, scene, schedule, script, template, timer, and websocket_api. Native template entities, MQTT config-entry/registry lifecycle, REST lifecycle, and recorder maintenance now have focused acceptance evidence. The JSON retains initial source-export/line-count snapshots alongside updated port status and evidence paths; exact primary checks and revisions are recorded in [PORTING.md](PORTING.md) and [VALIDATION.md](VALIDATION.md). Source presence alone is never full acceptance.

## Core work that gates the integration waves

These shared systems need pinned-source behavior and acceptance evidence before the integration catalogue can be considered usable.

| Order | Core subsystem | Remaining work |
| --- | --- | --- |
| 0 | Startup, loader, configuration entries and discovery | Implement manifest-aware validation and dependency ordering, config flows, options/reconfigure/migration, discovery handoff, rollback, unload and reload. The current YAML startup accepts a limited native subset. |
| 1 | Entities and platform lifecycle | Add entity identity and capabilities, platform setup, add/remove/restore/update, polling/coordinator ownership, registry linkage and unload cleanup. A state row or helper-created entity does not cover this lifecycle. |
| 2 | Registries | Complete entity, device, area, floor and label CRUD, stable identity, foreign-reference checks, atomic persistence, websocket command shapes and registry-updated events. The native registry source still needs its scoped acceptance evidence. |
| 3 | Users, auth providers and permissions | Finish password/session/token behavior, providers, permission checks across commands/services, MFA boundaries, revocation and restart behavior against pinned auth tests. |
| 4 | Runtime, storage and recorder | Preserve state/event/service ordering while adding recorder ingestion, retention/purge, statistics, repairs, migration coverage and the full history/query contract. SQLite history is only a subset of recorder. |
| 5 | REST, HTTP security and WebSocket APIs | Port exact endpoint, middleware, proxy-trust, error and message behavior; enumerate all WebSocket commands and authorization. Event-listener inventory currently returns 501. |
| 6 | Automation, scripts, scenes, templates and native helpers | Expand behavior from pinned tests for triggers, conditions, actions, execution modes, scenes, template helpers, resource limits, timers, buttons, datetimes and schedule transitions. |
| 7 | Frontend, voice/media and system workflows | The frontend/API assets, Assist and media pipelines, backups, repairs, diagnostics and onboarding remain separate Core gaps. |

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

