# Validation evidence

Status: the first native Rust milestone passed primary validation on 2026-11-05 using rustc 1.99.1 and Cargo 1.99.1 on macOS. Full Home Assistant parity remains unimplemented. No deployment to the user's Home Assistant instance was performed.

## Passing checks

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `cargo run --locked -p hass -- --config examples/configuration.yaml --check`
- `cargo build --release --locked`
- `target/release/hass --config examples/configuration.yaml --check`

The workspace test run passed **50 tests**: 9 application/config/helper tests, 4 compiled-process acceptance tests, 11 API tests, 6 automation tests, 9 core tests, and 11 SQLite tests. All doc-test targets passed. Clippy passed with warnings treated as errors; formatting and release build passed. Cargo.lock pins resolved dependencies.

Compiled-process acceptance tested unauthenticated rejection, state creation/update status and Location, native boolean service execution, an event-triggered boolean/counter automation, SQLite history, hard process termination and restart restore, and SIGTERM exit while an authenticated WebSocket remained open. Separate API socket tests cover authentication failures, subscription/service round trips, duplicate/malformed IDs, and shutdown during authentication and idle connections.

Independent durability review found three issues, all corrected with focused regressions: multiple runtimes caching one store; incomplete schema-constraint validation; valid JSON whose payload ID disagrees with its persisted row key. The implementation now refuses duplicate runtimes/writers and invalid/mis-keyed restore data. Primary upstream comparison also corrected forced-update timestamps to match pinned core.py.

Independent protocol review found no confirmed defects in the bounded authentication/REST/WebSocket/lifecycle source scope. It did not execute an additional test run. Its qualified in-flight synchronous-call shutdown limitation is retained below; no claim of exhaustive or production security certification is made.

## Acceptance coverage

Core checks cover state value/attribute/report timestamp semantics, ordered events, validation, failed persistence, and service handler re-entry. SQLite checks cover actual reopen/history, transactions, same-path writer refusal, and malformed or foreign schemas. Automation checks cover event/state/conditions/action execution, loop prevention and shutdown. Executable tests use isolated temporary directories and fresh databases; no live installation, device, or credentials are used.

## Limits of the evidence

Passing these checks establishes only the supported first milestone. No physical device integration, official frontend session, user/OAuth login flow, upstream database migration, TrueNAS runtime deployment, or full Home Assistant parity is verified. `--check` validates this Rust subset; it does not certify an upstream Home Assistant configuration.

Service target batches are fully validated before any mutation, but each state write commits separately: a storage failure halfway through a multi-target service can leave prior successful writes applied. There is no automatic recorder purge. History requires an explicit entity filter; listener inventory is unimplemented. Cooperating disk stores use an OS lock; external SQLite tools must not write to a live database. Already-running synchronous handlers must return before shutdown can finish.
