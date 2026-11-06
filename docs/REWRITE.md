# Home Assistant Rust rewrite

## Objective and current status

Rewrite Home Assistant Core in native Rust with explicit behavioral compatibility and no Python runtime bridge. This is an independent fork, not an official Home Assistant release. Full parity is not implemented. Never point it at an existing Home Assistant database or configuration directory.

Fork: https://github.com/javaDevJT/home-assistant-rust . Development branch: `rust-rewrite`. Upstream source remains on `dev`; local source remote is `upstream`. The original local starter remains on `main` and its `origin` remote is preserved.

## Baseline inventory

Measured 2026-11-05 from `git ls-tree -r --name-only upstream/dev`, commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`:

| Item | Count |
| --- | ---: |
| Tracked files | 30,403 |
| Python files | 20,410 |
| Integration manifest files | 1,565 |
| Files under homeassistant/ | 15,444 |
| Files under tests/ | 14,497 |

This inventory defines the size of the migration, not a claim of port coverage. Each integration requires its own native dependency/protocol implementation and parity evidence. Core rewrite work does not port the 1,565 integrations by implication. The frontend, Supervisor, OS, and companion applications are separate projects and outside this Core fork.

## Acceptance and remaining parity

First runnable milestone: native state/event/service runtime, persistence and history, bounded automations, authenticated REST and WebSocket APIs, executable configuration and shutdown, and failure-path checks. Source contracts: [CONTRACT.md](CONTRACT.md).

Full parity remains open: all upstream integrations; config entries/discovery; device/entity/area registries; OAuth/users/permissions/MFA; templates and scripts; all automation triggers/modes; recorder statistics/purge/migrations; frontend protocols and assets; backups/repairs; exact API semantics beyond verified endpoints. A static token authenticates this milestone; it is not a port of Home Assistant's authentication system. Virtual helpers do not control physical devices.

Protocol references: [REST](https://developers.home-assistant.io/docs/api/rest/) and [WebSocket](https://developers.home-assistant.io/docs/api/websocket/), retrieved through Context7 on 2026-11-05. Validate concrete behavior against pinned upstream source and integration tests as ports progress.

## Task ledger

Model policy read from swarm skill `references/model-config.md` on 2026-11-05. Live native catalog and T3 capabilities expose a single callable Luna: `gpt-6-luna`, effort `max`. All workers explicitly request that pair, using context-free native dispatch; native spawn does not expose service tier. Primary: `gpt-6.1-sol`, effort `ultra` (runtime supplied).

| Owner | Scope | Consumer/check | State |
| --- | --- | --- | --- |
| Primary | workspace, hass app except helpers.rs, docs, Git/integration | full workspace checks and executable smoke | running |
| /root/core | crates/hass-core only | all other crates; ordered persistence/state/event/service tests | accepted with primary corrections; 9 final tests; worker completed/released |
| /root/storage | crates/hass-storage only | runtime; reopen/history/transaction tests | accepted with primary review corrections; worker completed/released |
| /root/automation | crates/hass-automation only | app; trigger/action/shutdown/loop tests | accepted; 6 tests passed; worker completed/released |
| /root/api | crates/hass-api only | app/clients; REST and real WebSocket checks | accepted; 11 tests and lint passed; worker completed/released |
| /root/helpers | crates/hass/src/helpers.rs only | app; restored state, target/bounds/concurrency tests | accepted; 6 tests and example check passed; worker completed/released |
| /root/durability_review | read-only core/storage audit | primary; durability/order/failure and pinned behavior risks | accepted; 3 findings fixed with regression tests; completed/released |
| /root/protocol_review | read-only API and app lifecycle audit | primary; authentication/protocol/resource failure paths | accepted; no confirmed findings; documented sync-handler shutdown limit; completed/released |

Workers use the current shared worktree based on starter `45bf1ff61fbac7ee069a94dad18fb0698fb29601`, with an uncommitted ancestry merge of pinned upstream. Each scope is exclusive. Escalate contract mismatches; no child Git writes or external actions. Handoff: status, changed paths, checks, evidence, open risks. Completion releases native worker capacity. Primary reviews and accepts/rejects each result before publishing.

Primary review corrections: forced updates refresh timestamps to match pinned upstream; one Runtime constructor per Store (clone Runtime for shared access); disk stores hold an OS writer lock; schema checks include primary keys/null/default/autoincrement/time constraints; restored snapshots validate IDs/lengths and row-key agreement. The 2026-11-05 primary workspace test run passed 50 tests, including compiled-process crash recovery and SIGTERM with an open authenticated WebSocket. Final validation record: [VALIDATION.md](VALIDATION.md).

Requested worker model/effort: `gpt-6-luna` / `max` for every named worker, confirmed callable by the live native catalog and capabilities response. Spawn accepted the explicit fields. The native status surface does not expose observed backend model/effort or service tier, so those observations remain unverified rather than inferred. All builders' changed paths stayed within their declared scope; root Cargo.lock was produced by dependency resolution and accepted by the primary. Primary owns all integration corrections after builder completion. The final commit records the exact integrated source revision.

All seven native workers completed and released their capacity. Protocol review found no confirmed authentication, validation, subscription cleanup, blocking-I/O placement, or REST mutation/status bugs in its bounded source review. It noted that an in-flight synchronous service/storage call can delay WebSocket shutdown; built-in services are finite and the SQLite busy timeout is bounded, but arbitrary blocked handlers cannot be forcibly canceled. This is retained as a documented limitation.
