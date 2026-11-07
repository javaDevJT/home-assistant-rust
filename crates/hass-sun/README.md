# Native Sun integration

This crate follows Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and its pinned Astral 2.2
behavior. The source-traced contract and exact reference fixtures are in
[`docs/SUN-PORT.md`](../../docs/SUN-PORT.md). Preserve the formulas and source
attribution in `NOTICE` when changing the math implementation.

## Runtime

`SunIntegration::start(runtime, registries, lifecycle, entries, import_yaml)`
registers the singleton `sun` config-entry driver, optionally imports the
empty YAML entry, and restores or sets up the saved entry. Its empty user/import
flow creates title `Sun`, empty data/options, and no config-entry unique ID.
The observer and timezone come from `CoreConfig`.

The host publishes fixed `sun.sun`, six timestamp sensors, two default-disabled
position sensors, and one default-disabled rising binary sensor. The generated
sensors use the entry-scoped unique IDs and share the service device identified
by `(sun, entry_id)`. Position sensors declare `measurement` state class and
degree units. The nine Sun attributes remain available in live state and
events but are excluded from recorder history.

The worker schedules in UTC from the next solar boundary, updates position at
the phase-specific interval, reacts to core location/timezone updates, and
joins on unload or shutdown. Startup rollback, config-entry retry, removal of
the deprecated rising sensor identity, and preservation of user-enabled
position sensors across reloads are covered by the focused tests. Use
`shutdown().await` for joined cleanup; `Drop` is best-effort.
Entity enablement watches refresh all values immediately, including fresh
default-disabled entities with no restore snapshot. They compare statuses
because watch updates can coalesce an `Enabled` reason into a later registry
notification. Drop cancels workers and releases leases; a new host using the
same config manager reactivates a loaded marker without an active provider.

## Conditions

`PureSunCondition` and `evaluate_condition` implement the pinned pure Sun
conditions. The legacy before/after evaluator preserves offsets, local-date
selection, inclusive bounds, the overnight OR case, DST UTC instants, and
false results when a required polar sunrise or sunset does not occur. The
`SunCondition` and `evaluate_sun_condition` names remain as aliases for callers
already using the earlier API.

The app hosts the integration using the shared config-entry, registry and entity
managers. Empty `sun:` YAML imports its singleton entry; the native generic
configuration flow can also add it. Core location updates recalculate the host
and remain authoritative after restart. Automation scheduling and shared script
conditions use this crate's calculated events and condition helpers.

## Ownership and checks

The crate is now in the root workspace. All 26 focused cases pass there,
including primary regressions for immediate enablement and same-manager Drop
recovery. The original isolated 24-case suite also passed strict Clippy and
formatting. Compiled-process fixtures pass configuration-flow creation, immediate
enablement, saved observer updates, history attribute projection, hard restart,
entry removal and a real-clock offset sunrise action. The full workspace gate
and remaining trigger acceptance stay in progress under the primary owner;
these focused checks do not establish complete integration parity.
