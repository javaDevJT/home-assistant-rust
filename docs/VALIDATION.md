# Validation evidence

Status: the current integrated Rust wave passed **228 workspace tests**, formatting, strict full-workspace Clippy, and its optimized build on 2026-11-06 with Rust/Cargo 1.99.1 on macOS. Publication checks are recorded below as they finish. Full Home Assistant parity remains open. No deployment to the user's Home Assistant instance was performed.

## Current integrated acceptance

`cargo test --workspace --locked` passed 228 tests across the 14-crate workspace and every doc-test target. `cargo clippy --workspace --all-targets --locked -- -D warnings` passed. Cargo.lock pins all resolved dependencies. The exhaustive upstream inventory reconciles 1,565 unique integration manifests across 32 disjoint batches: 22 partial native domains, 1,543 unported, zero claimed complete.

`cargo fmt --all --check` and `cargo build --release --locked` also passed. The optimized executable successfully validates `examples/configuration.yaml` with `--check`.

Actual transport fixtures cover native onboarding/password login, one-use authorization codes, JWT identity, refresh rotation/revocation, user-bound TOTP enrollment/QR and replay rejection, signed paths, proxy origin/local-only policy, public/private subscription separation, config/options flows and actual lifecycle callbacks. History fixtures exercise boundary snapshots, significant/minimal/no-attribute flags and compressed WebSocket output. Internal calls, HTTP and WebSocket services use the same live registry selector resolution.

MQTT tests use local TCP protocol peers to prove discovery, reported state/availability, outbound service commands, reconnect, config-entry setup/retry/reload/unload/restart, registry renames and shared-provider cleanup. REST tests use real loopback HTTP to verify polling/commands, templates, auth/error/recovery and startup/shutdown/drop cleanup. No production credentials, live brokers, physical devices or third-party API activity are used.

Compiled-process acceptance includes an actual YAML include, reactive template sensor updates through native automation, persistent identity/state across a hard process termination, history responses, and SIGTERM exit with an open authenticated WebSocket. The process test caught and corrected a YAML core-setting seed mismatch and static template-name rendering before acceptance.

Storage tests verify owned v1→v2 migration, rollback/foreign-schema rejection, single writer/cache ownership, private file modes, FULL synchronous commits, historical key/time validation, bounded multi-entity/event queries, purge preservation of current state/private metadata and actual repack/reopen. Lifecycle review found REST/TemplateEngine and template-action ownership cycles; owner-scoped cleanup, guarded target callbacks, failed-start rollback and same-process lease-release regressions correct those findings. Drop requests cancellation; explicit async shutdown joins owned work.

These receipts establish the implemented subset. They do not verify complete integrations, frontend/browser use, upstream database compatibility, custom authorization policies, hardware operation, or full Home Assistant parity. Synchronous handlers already running must terminate before joined shutdown can complete.

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
