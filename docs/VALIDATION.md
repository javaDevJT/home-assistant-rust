# Validation evidence

The current 19-crate checkpoint passed **366 workspace tests**, formatting, strict full-workspace Clippy, optimized build and CLI configuration validation on 2026-11-06 with Rust/Cargo 1.99.1 on macOS. Publication is recorded below as it completes. The previous published 17-crate checkpoint and its green Linux run remain separately pinned. Image-upload work is outside this receipt. Full Home Assistant parity remains open. No deployment to the user's Home Assistant instance was performed.

## Current 19-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 366 tests across 49 unit/integration/doc-test targets, with no failed or ignored tests. Workspace formatting, strict all-target Clippy, `cargo build --release --locked`, and optimized executable example `--check` passed. Optimized executable SHA-256: `22fcac39e7f83aa7d2bc14b5445b2b6e49ffa52c244ccad4e9e7971f39124839`.

The compiled process now verifies actual `device_tracker.see` reports flowing through registry-backed person selection to zone occupancy and surviving a hard restart. A real authenticated WebSocket fixture verifies person config/storage lists, ordinary-user administration denial, private current/live updates, caller-only profile edits, tracker preservation on profile changes, and actual auth deletion/unlinking. Auth regression covers persisted self-name updates without privilege changes, disabled-account rejection, user event payloads and revocation. App fixtures prove secret-aware read-only known-device import, empty/default YAML, duplicate person identity preflight and writer-lease release after person host shutdown. Gravatar URLs are hashed locally with pinned precedence and require no external request.

The full inventory remains a partition of all 1,565 manifests over 32 batches: 28 partial native domains, 1,537 unported, zero complete. Source conditions, translations, scanner platforms, complete registry/provider lifecycle and other listed gaps remain open. Non-null person picture uploads remain unavailable until the excluded image component is integrated; arbitrary URLs are not accepted as uploaded pictures.

## Published 17-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 304 tests across the 17-crate workspace and every doc-test target, with no failures or ignored tests. `cargo clippy --workspace --all-targets --locked -- -D warnings` passed. Cargo.lock pins all resolved dependencies. The exhaustive upstream inventory reconciles 1,565 unique integration manifests across 32 disjoint batches: 26 partial native domains, 1,539 unported, zero claimed complete. Every partial-domain evidence path exists.

`cargo fmt --all --check` and `cargo build --release --locked` also passed. The optimized executable successfully validates `examples/configuration.yaml` with `--check`.

The current optimized macOS executable SHA-256 is `1dabe8a70a716bc2df0d93f0d44a28a4eff78426633ccc4361391ebdd2dd432c`. The pending person crate is outside the workspace and excluded from this checkpoint.

The new wave includes legacy group aggregation and provider delegation; process-local notifications with real authenticated live feeds and private automation triggers; provider-ready, one-shot startup triggers; source-correct helper initial/restore, select options and numerical precision; and native YAML/home/editable zones with WGS-84 geometry, collection CRUD, occupancy, actual reload and live persisted core location updates. The executable fixtures now cover both HTTP notification-to-automation execution and zone behavior through hard restart. Internal zone reload authorization and same-process writer-lease release also pass. Independent automation review found no confirmed discrepancy in the requested private-signal/causality/startup paths against the exact pinned source.

Primary review caught and corrected stale owned zone states after changed-YAML restart, collisions with foreign manual zones, and fixed-decimal formatting that could erase small helper values. The first full run found an invalid minimum-step value in the added precision fixture; the fixture was corrected while retaining the pinned step constraint. The final full run above passes. Zone state/metadata failures attempt compensation; individual state writes do not provide a multi-state transaction or retract events already observed by subscribers.

Actual transport fixtures cover native onboarding/password login, one-use authorization codes, JWT identity, refresh rotation/revocation, user-bound TOTP enrollment/QR and replay rejection, signed paths, proxy origin/local-only policy, public/private subscription separation, config/options flows and actual lifecycle callbacks. History fixtures exercise boundary snapshots, significant/minimal/no-attribute flags and compressed WebSocket output. Internal calls, HTTP and WebSocket services use the same live registry selector resolution.

MQTT tests use local TCP protocol peers to prove discovery, reported state/availability, outbound service commands, reconnect, config-entry setup/retry/reload/unload/restart, registry renames and shared-provider cleanup. REST tests use real loopback HTTP to verify polling/commands, templates, auth/error/recovery and startup/shutdown/drop cleanup. No production credentials, live brokers, physical devices or third-party API activity are used.

Compiled-process acceptance includes an actual YAML include, reactive template sensor updates through native automation, persistent identity/state across a hard process termination, history responses, and SIGTERM exit with an open authenticated WebSocket. The process test caught and corrected a YAML core-setting seed mismatch and static template-name rendering before acceptance.

Storage tests verify owned v1→v2 migration, rollback/foreign-schema rejection, single writer/cache ownership, private file modes, FULL synchronous commits, historical key/time validation, bounded multi-entity/event queries, purge preservation of current state/private metadata and actual repack/reopen. Lifecycle review found REST/TemplateEngine and template-action ownership cycles; owner-scoped cleanup, guarded target callbacks, failed-start rollback and same-process lease-release regressions correct those findings. Drop requests cancellation; explicit async shutdown joins owned work.

These receipts establish the implemented subset. They do not verify complete integrations, frontend/browser use, upstream database compatibility, custom authorization policies, hardware operation, or full Home Assistant parity. Synchronous handlers already running must terminate before joined shutdown can complete.

## Linux checkpoint CI

Checkpoint `7e093fc2bdc5f0a521f932a46f65f9df9a828b5d` passed [Linux run 42180581797](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42180581797): workspace formatting, strict all-target Clippy, all workspace tests, and actual CLI example validation. This immutable run includes groups, notifications, zones and helper corrections; it does not cover subsequent person/tracker/auth-bridge changes.

Implementation `2250d4780f44b4837766fba31b2e150078575c39` is published on the fork. [Linux run 42165900082](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42165900082) passed formatting and strict workspace Clippy, then exposed a timing-dependent automation queue test. Its fixed one-second wait began shutdown before a busy runner had drained initialization events and observed queue overflow. The regression now waits for actual listener termination, then checks the specific pending-queue error. The focused case passed five consecutive local runs; all 14 automation tests and strict automation Clippy passed after the correction.

Published correction `89ef0e3907a39a6152c883343ccf3721a9e08850` passed [Linux run 42167148335](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42167148335): formatting, strict full-workspace Clippy, all workspace tests and example configuration validation. This CI evidence applies to that immutable revision, not subsequent dirty group/control work.

## Initial published milestone (historical)

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `cargo run --locked -p hass -- --config examples/configuration.yaml --check`
- `cargo build --release --locked`
- `target/release/hass --config examples/configuration.yaml --check`

The workspace test run passed **50 tests**: 9 application/config/helper tests, 4 compiled-process acceptance tests, 11 API tests, 6 automation tests, 9 core tests, and 11 SQLite tests. All doc-test targets passed. Clippy passed with warnings treated as errors; formatting and release build passed. Cargo.lock pins resolved dependencies.

GitHub Actions also passed on Linux for implementation commit `546646ae3bdb6fef7b0c47ffb86e3ed5653c8cbc`: [Rust run 42145454261](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42145454261). It checked formatting, warning-free workspace Clippy, all workspace tests, and the example configuration. The fork's default branch is `rust-rewrite`; the upstream `dev` branch remains at the inventoried source commit. Local worktree and published implementation head were read back and matched.

Compiled-process acceptance tested unauthenticated rejection, state creation/update status and Location, native boolean service execution, an event-triggered boolean/counter automation, SQLite history, hard process termination and restart restore, and SIGTERM exit while an authenticated WebSocket remained open. Separate API socket tests cover authentication failures, subscription/service round trips, duplicate/malformed IDs, and shutdown during authentication and idle connections.

Independent durability review found three issues, all corrected with focused regressions: multiple runtimes caching one store; incomplete schema-constraint validation; valid JSON whose payload ID disagrees with its persisted row key. The implementation now refuses duplicate runtimes/writers and invalid/mis-keyed restore data. Primary upstream comparison also corrected forced-update timestamps to match pinned core.py.

Independent protocol review found no confirmed defects in the bounded authentication/REST/WebSocket/lifecycle source scope. It did not execute an additional test run. Its qualified in-flight synchronous-call shutdown limitation is retained below; no claim of exhaustive or production security certification is made.

## Initial milestone acceptance coverage

Core checks cover state value/attribute/report timestamp semantics, ordered events, validation, failed persistence, and service handler re-entry. SQLite checks cover actual reopen/history, transactions, same-path writer refusal, and malformed or foreign schemas. Automation checks cover event/state/conditions/action execution, loop prevention and shutdown. Executable tests use isolated temporary directories and fresh databases; no live installation, device, or credentials are used.

## Initial milestone limits (historical)

Passing these checks establishes only the supported first milestone. No physical device integration, official frontend session, user/OAuth login flow, upstream database migration, TrueNAS runtime deployment, or full Home Assistant parity is verified. `--check` validates this Rust subset; it does not certify an upstream Home Assistant configuration.

Service target batches are fully validated before any mutation, but each state write commits separately: a storage failure halfway through a multi-target service can leave prior successful writes applied. There is no automatic recorder purge. History requires an explicit entity filter; listener inventory is unimplemented. Cooperating disk stores use an OS lock; external SQLite tools must not write to a live database. Already-running synchronous handlers must return before shutdown can finish.
