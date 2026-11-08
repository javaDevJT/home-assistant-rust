# Full Rust rewrite execution ledger

## Standing objective

Complete the entire native Rust rewrite. Work resumes from `04c8ff6e96eb98957c6a5d49a959fdc5f07cae1e`. This ledger tracks successive executable ports; a passing wave does not complete the objective. Upstream remains pinned at `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Never count a manifest, type stub, or promised implementation as a completed integration.

The primary owns the workspace manifest/dependencies, app configuration and integration, API adapters, final checks and Git. Each worker exclusively owns the listed source scope. Workers may not change root manifests, another scope, Git state, live services/devices, or spawn children. Use pinned upstream source as behavioral evidence and local protocol fixtures for network work. No production credentials or device commands are needed for fixture checks.

## Current wave contracts

### Sensor platform continuation (after the accepted 44-crate source)

Root has enrolled the released binary-sensor SDK as member 45, normalized its
manifest and removed its generated standalone lockfile. Sun's rising descriptor
and value publication now use the typed SDK with the existing identity and
registry metadata. Root fixed explicit JSON null extras to survive projection;
`None` still removes a builder attribute, and the shared lifecycle already omits
dynamic attributes while unavailable. The focused gate passes four binary SDK
and 26 Sun cases, plus strict all-target Clippy for both packages.
Numeric v151 and system-adapter v153 are released. Root has enrolled sensor as
member 46 and added runtime-feature-scoped sensor/tracing dependencies for
system sensors. Time/date and uptime now publish typed SDK projections, passing
15 default and 11 no-default-feature cases plus strict lint. Sun retains its
eight numeric descriptors alongside actual handles and publishes event times
and positions through the SDK; all 26 Sun cases pass. Root's seventh SDK case
verifies exact source ordering: enum options are optional, unknown readings
skip later numeric-unit checks, integer text stays integer and f64 keeps `.0`.
The eighth covers native unsigned counters, booleans and integer text beyond
i64 without floating-point string loss, including valid underscore separators.
The ninth checks 16 independent built-in float-format golden values, including
scientific boundaries, signed zero, subnormal/extreme values and nonfinite
plain values; formatting uses Python-style exponent signs and padding.
v156 released the complete native-unit table: 57 rules / 270 entries over 62
classes, source-AST compared with exact source hashes. Root wired it into actual
projection, removed unsupported-table warnings, and added valid/no-unit/invalid
warning regressions. All 13 native-unit spelling aliases are applied before
validation/publication, with raw provider units retained. Sensor now has two
unit-table and 11 projection cases; Sun remains green. Binary device automation
v155 is released with 47 cases and strict lint; the compiled YAML condition,
trigger and template-to-helper path passes, along with the clock/restart case.
The 46-member workspace passes 1,186 tests in 134 targets, zero failures or
ignored cases, strict all-target Clippy and formatting. The optimized build and
ten owned-temp CLI cases pass, including device mapping/type/duration checks;
binary SHA-256 is
`9ff135c40fd11f90a9d1dd84dda102285aee5f704a52f2f80dbab7503f88390e`.
Source `aade9aa33ecccce1b627359f7fd1023353bb5f49` is published and passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42438729634).
Root independently verified all 956 evidence paths and 610 Rust symbols, unchanged
pinned snapshots and unrelated records. Inventory is 49 partial / 1,516 unported
/ zero complete. v150's receipt update is published at
`9cba7580a5a03d4ab5a85e8ea86002b20d23da6a`; that owner is released.

### Switch and shared live metadata continuation (47 crates)

Root enrolled member 47 and SDK/Entity/Registry dependencies for MQTT. Switch
v157, MQTT adoption v158 and additive entity publication v159 are released.
Focused checks pass 11 switch, 25 shared entity and seven MQTT wire cases,
including unavailable assumed state, explicit null/removal, SQLite reopen,
renamed target commands, disabled target rejection and unknown-state toggle.
Main supplies its existing shared lifecycle and registries; only switches use
this SDK path, with other MQTT platforms retaining their existing adapters.
The live attribute API preserves the four-field legacy snapshot shape and
user registry override priority. Root owns all production code again.

Review v160 found driver ownership gaps in canceled startup, canceled shutdown
and same-manager restart. Root now retains startup and cleanup tasks, closes
admission before cleanup, drains entries and unregisters the exact driver.
v161's three numeric-loopback lifecycle regressions pass and that owner is
released. Compiled-process owner v164 writes only new
`crates/hass/tests/mqtt_process.rs` to verify actual main-path MQTT adoption.
Button v162 owns only new `crates/hass-button/**` and BUTTON-PORT.md; light v163
owns only new `crates/hass-light/**` and LIGHT-PORT.md. Their scaffolds remain
outside workspace acceptance until enrolled. Every worker uses the single
callable Luna family member `gpt-6-luna` at `max` effort, verified against the
live native catalog on 2026-11-07; provider/tier controls are unexposed.
The provisional 47-member gate passed 1,205 tests in 137 targets, formatting,
strict all-target Clippy, optimized build and ten CLI cases. Its working binary
hash was `4055171333f14b2f231586a5aa5c7b41755ebf60fb1de4b5e35579ce3c4e7cfa`.
It was not published or accepted as an immutable source receipt: the actual
compiled-process fixture exposed the missing config-entry registry propagation
below. All final checks and the optimized hash must be recomputed after fixes.

### Config-entry propagation and awaited MQTT commands (48 crates)

v164 is released; root owns its compiled MQTT process fixture. It proves actual
discovery, class/name/assumed state, renamed-ID wire commands and entity-level
disable. Its final config-entry disable check remains red until registry flags
are propagated. Source `config_entries.py:2686-2721` enables registry entities
before setup and disables them after unload. A generic enabled-lease unload
leaves an unavailable restored placeholder; config-entry disable subsequently
derives `disabled_by: config_entry` and removes it. Both assertions remain
distinct in the process fixture; no placeholder assertion substitutes for
the required final absence. Temporary root diagnostics are removed.

Root added a separate `EntryRegistryDisabledByChanged` lifecycle event with
that source ordering, preserving Repairs' earlier visibility callback. All
39 config unit and four integration cases pass after the observer extension.
v168 is released; root owns new registry `config_lifecycle.rs` and app
`registry_lifecycle.rs` plus their parent/main wiring. Six registry cases,
strict package Clippy and formatting pass. Source review added cross-entry
device-linked entity derivation and startup repair for rows without an owner.
Startup preserves device history and only rederives entity flags, following
`entity_registry.py:2610-2645`; replaying device callbacks in entry-ID order
was rejected. The observer uses a weak config-manager reference and root added
serialized registry projections outside config-manager locks. Source-AST
comparison permits `hass` for entity disablers only; the separate device enum
is preserved. Fresh host/process validation awaits the MQTT implementation.

Root enrolled the released button SDK as member 48 and added its MQTT
dependency. Nine standalone cases cover timestamp formatting, state-before-
provider ordering, preserved timestamps on provider failure, repeated equal
presses, inactive leases and live device-class removal. Core already returns
successful unchanged publications; explicit lease admission is defensive.
The actual provider adoption remains pending v169, who exclusively owns MQTT
source/tests/README for SDK-backed buttons and awaited QoS completion. The
command contract requires driver ownership after caller cancellation, with
optimistic state after source-defined completion or the ten-second soft timeout.
v170 owns only app `homeassistant.rs` for generic controls that await async
providers; its first 12 cases and package-only strict Clippy pass. Root rejected
sequential per-domain awaits because source gather starts all providers and
returns the first error without canceling already-started siblings. v171 owns
Core source/tests/README for an explicit tracked async-call API with inherited
execution scope, explicit cancellation and owned shutdown joins. Direct inline
async cancellation remains unchanged. v170 consumes that API for gather
concurrency and parent cancellation; root's process fixture adds actual
generic-control-to-MQTT wire/context proof. Root converted the four app MQTT
service test callers to async dispatch. Neither extension is accepted before
its focused and complete expanded gates.

Light v163 and cover v167 are released outside the workspace: their isolated
SDKs pass 12 and seven cases respectively plus strict lint/formatting. Their
contracts are LIGHT-PORT.md and COVER-PORT.md; actual adapter migration and
root enrollment are still pending. Inventory v165 completed a read-only audit,
changed no files, and is released until the final expanded gate. Accepted
inventory remains 49 partial / 1,516 unported / zero complete at source 46.
The compiled clock fixture now validates UTC whole-second output and waits for
an observed second boundary before proving a fresh restart timestamp.
Read-only v154's concerns were rejected against the actual pinned
`sensor/entity.py:574-732`; the source contract's enum wording was corrected.
SDK warnings are logged without changing valid publication; projection errors
use the existing provider error path. Actual loaded platform names are exposed
in config/logger catalogs. Released v155 implemented binary device conditions
and triggers with state semantics and source UUID resolution; device_id is
metadata and adds no invented ownership check.

The final 48-member local gate is green: **1,250 tests / 140 targets**, zero
failures or ignored cases, final workspace formatting and strict locked offline
all-target Clippy. The optimized workspace build and all 13 owned-temp CLI
cases pass. Three new switch device cases accept a mapping duration and reject
an invalid type or string duration; the prior ten CLI cases remain green with
no database/extra files or ignored-secret echo. Optimized macOS binary SHA-256
is `6f755a88bc40f437ff41fc7f377e41dc098dff59552a6738aa629bc2bd443bca`.

v169 and v172 are released. MQTT passes two unit and 14 wire cases, including
actual changed-state results, completion-time availability and fresh attribute
retention. Automation passes 53 cases. Root's compiled MQTT fixture passes
actual direct/generic/HTTP wire calls and results, registry/config-entry disable,
quiescence and restart identity. The three registry-host cases and both binary
and switch compiled device-automation cases pass in the full 22-case process
target. Root's released-placeholder cleanup regression passes with all 26
entity cases. Core v171's final 46 cases retain completed unexpected join
failures until shutdown while keeping intentional cancellation separate;
generic controls pass 14 cases. Only parity v165 writes the two inventory
files now; root owns the frozen code, other docs and publication.

Next-wave v173 owns only isolated `crates/hass-fan/**` and FAN-PORT.md with the
same explicit native Luna/max policy. Light/cover SDKs remain ready for later
enrollment and concrete MQTT adoption. Isolated v174 owns only new
`crates/hass-units/**` and UNITS-PORT.md for complete source-backed conversion
families; current sensor conversion families remain partial. Neither future
SDK enters this receipt. Root independently verified the inventory changes
are confined to six domain records and their policy mirrors, with immutable
snapshots and unrelated records unchanged. All 1,586 local path references and
966 distinct Rust symbols resolve; the historical Core public `call_service`
method is implemented in its declared `service_dispatch` module.
Another follow-up is actual loaded
platform names in the config/logger catalog: current API config is seeded
from YAML, while the logger adds loaded entry domains without their platform
dependencies. These gaps do not change the current partial acceptance.

The 48-crate source is published at
`2c9d9e5bca6b8b95d13c06e745640fa369d49a3a`.
[Exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42453600318)
passes formatting, strict Clippy, full workspace tests and CLI validation.
Root independently verified its exact head, completed status and successful
conclusion. No deployment or PR was created.

### Light, cover and fan lifecycle adoption (51 crates, pending)

Root enrolled the released light, cover and fan SDKs as members 49-51, removed
their generated standalone locks, added MQTT dependencies and refreshed the
offline workspace lock. Their isolated evidence is 12 light / seven cover /
seven fan cases with strict package checks. v175 now owns only MQTT
source/tests/README for actual SDK-backed discovery/state/lifecycle adoption,
preserving the accepted command completion and button/switch paths. It exposes
`MqttEntityMetadata.entity_lifecycle_managed` from actual lease ownership;
root owns the app reconciler's switch-only guard replacement and all manifests,
SDK corrections and docs. No platform status changes are claimed yet.
v174 independently owns isolated `hass-units` and UNITS-PORT.md. The source
contains 31 converter classes; its planned fallible API preserves optional
unit identifiers, source identity shortcuts and invalid-unit diagnostics.
All new owners use native Luna/max, with provider/tier controls unexposed.

### Accepted native media and asynchronous services (44 crates)

The next independent platform work is outside this 44-member receipt: v151
owns only isolated `crates/hass-sensor/**`; v152 owns only isolated
`crates/hass-binary-sensor/**`. Their contract is `SENSOR-PORT.md`; root owns
future workspace enrollment and actual consumer migration. Their scaffolds or
isolated tests do not change the sensor domain inventory. Both use the same
explicit native Luna/max selection and synthetic-only constraints.

The OAuth foundation below is published and Linux-accepted. The next workspace
has 44 members, adding the released Spotify wire client and media-player SDK.
Core, API, script and SDK owners v134/v136/v137/v138 are released; root owns
their integration and corrections. Only v144 writes Spotify `mod.rs`,
`player.rs` and runtime tests, while v145 writes `spotify/browse.rs`. Root owns
account policy, controls, projection, manifests/lock, main, shared docs and Git.
Both implementation owners are now released. Read-only v147 reviews runtime
cancellation and teardown; v148 owns only `spotify/tests.rs` for held-request
shutdown and persisted-entry acceptance. v146 owns only `hass-script` source,
README and tests for service response variables and stop-response propagation.
All use explicit native Luna/max, no children or real provider I/O.

Focused checks pass: Core 40 tests including registration handoff and callback
destructor reentrancy; media SDK 14; asynchronous service HTTP/WS five; direct
media browse/search WS five; app media-service integration one. SDK corrections
reject undeclared command fields, skip unavailable players, preserve grouping
null semantics and separate remote/local pictures. Core reads a consistent
registration generation, drops removed callbacks outside locks, and reports
typed service validation separately from transport schema failures. API tests
cover source-defined user-facing service errors and zero effects on bad targets.
Script actions/scenes await the shared dispatcher. v146's response variables
and stop-response propagation pass 18 focused cases; strict checks and final
handoff remain pending. `schedule.get_schedule` has typed response-only registration.

Root's media host registers the standard services through Core with causal
contexts, response gates and actual state changes. Provider shutdown precedes
the shared entity lifecycle and owned service-task drain. OAuth activation is
retained on a blocking worker so synchronous config-entry setup may await its
captured runtime; host checks pass one unit and 21 integration cases including
durable token restart. Spotify account/projection policy passes nine app cases,
and controls pass two real loopback cases. Runtime compile corrections,
production activation, polling/browse lifecycle acceptance and the complete
44-member gate remain pending. Main now registers Spotify before startup
events, restores entries through its driver, exposes the actual provider in the
credentials catalog, and stops it before media/OAuth ownership. Encoded wrapper
commands pass three control cases; five browse cases pass all pinned branches.
Strict app Clippy exposed ten issues; root corrected the actual lock scope and
the remaining lint findings, with a fresh gate pending. The compiled-process
credentials restart/open-stream/SIGTERM case passes with the actual Spotify
catalog and component list. Review v147 found canceled startup/shutdown ownership
holes and a permanently retained reauth marker. Root constructs the integration
owner before restoration, transfers all joins and the provider lease into an
owned cleanup coordinator, and removes the poll/state ownership cycle. Reauth
deduplication now tracks active flow IDs and expires them without resolving
callbacks. The host's one unit plus 21 integration cases pass, including that
non-resolving check. A 20ms TTL fixture expired during its own setup under build
contention; its test budget is now one second, with runtime TTL policy unchanged.
v148's nine runtime cases pass, including cancellation, restored setup-error
recovery, due retry, abandoned reauth/reload and changed-playlist 401 handling.
The full 44-member workspace passes 1,158 tests across 129 targets, zero failures
or ignored tests, fresh strict all-target Clippy and formatting. The optimized
workspace build and seven owned-temp CLI checks pass; binary SHA-256 is
`8f8805e48b951a112eecdaf41a7fa56f0a8d5d66316d76d68b995d3e4afd829f`.
The first full run exposed an obsolete response expectation and restart's own
shutdown canceling its reply. Both are corrected; the compiled restart case
verifies WebSocket and HTTP acknowledgments, in-place exec and persisted state.
Source `90fd6b15e01c176ce2e65ceb992705843da2b089` is published and passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42431349045)
including formatting, strict all-target Clippy, full tests and CLI. v150 is
released; root independently verified all 878 partial-row paths and 546 Rust
symbols, unchanged source snapshots and unrelated records. Inventory is now
47 partial / 1,518 unported / zero complete. The sensor SDK wave continues
outside this receipt; binary SDK is released to root with four isolated cases,
and root corrected explicit-null extra attributes against the pinned entity helper.

### OAuth external-flow implementation (2026-11-07)

Current primary checkpoint: all 42 workspace source scopes are released to root.
Core passes 39 library plus four public-API lifecycle cases. Independent review
v130 found a late synchronous child-flow admission during shutdown; the shared
admission barrier and gated post-create regression close it. Synchronous
activation/continuation now runs in retained blocking tasks, preserving the
existing HTTP callback-thread and MQTT-backed process tests. Host v127 passes
one unit plus 18 integration cases. Review v132 found completed cleanup-receipt
retention; identity-checked removal, a 2,048-record unit case and repeated expiry
cycles cover the correction. No cancellation orphan or HostInner cycle was
confirmed. The eight HTTP OAuth cases, both legacy flow protocols, three app
callback-origin cases and credentials restart/public-callback/SIGTERM case pass.
The final workspace passes 1,070 tests across 121 targets, zero failed or ignored,
including 148 app-bin and 20 process cases. Strict all-target workspace Clippy,
formatting, the optimized build and seven isolated optimized CLI checks pass.
The unchanged auth target passed after two request timeouts during concurrent
LTO; the final full run passed with heavy builds held. Optimized macOS SHA-256
is `3f812bd98da78ad8d115a6f44e8ab6a96d03d654f7fc7311da3c078acddb9747`.
Published source `eefd57e453c74229ad6f53fb02ab9f9734f424a0` passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42415440469)
(formatting, strict all-target Clippy, full tests and CLI). Inventory remains 45
partial / 1,520 unported / zero complete, with all 780 partial-row paths and
476 Rust symbols resolved; other source snapshots/domain records are unchanged.

Work after this receipt is isolated from the 42-crate acceptance: v126 released the Spotify
wire crate (15 tests, strict Clippy and formatting); independent v133 found no
default-path wire mismatch. Root must use the default trusted API origin,
enforce queue media types and preserve account identity on integration. The
media SDK owner is now released. v135 released the additive
Runtime async service/response-capability contract in ASYNC-SERVICES-PORT.md.
Root owns manifests, all live
integration and index updates. The chronological records below preserve
superseded assignments.

Host review v125 is released with two substantive corrections: canceled
unregister/shutdown waiters can orphan Core driver cleanup, and get/submit do not
enforce idle host TTL. `oauth_host_cleanup_v127` owns only the host lifecycle
source/tests/README for cancellation-safe retained teardown and consistent
expiry. Root keeps manifests and all integration files. The host must affect
only its own registrations/flows; main retains and drains the shared OAuth
factory separately after provider/host shutdown. Same explicit Luna/max native
selection, no children or Git; concise gated regressions and lifecycle handoff
release the worker. Root remains the only Core correction writer.

Root has taken over all `crates/hass-config/**` corrections after v120's
repeated context resets. That worker is released, reported no active build,
and must not be a second writer. Its last useful checkpoint said Core check
passed with owned options/subentry APIs; the five v124 findings still require
primary inspection and completion. The later generic handoff described API
artifacts rather than its Core scope and is not new Core acceptance evidence.

Queued `spotify_wire_v126` owns only the isolated future
`crates/hass-spotify/**` typed HTTP client/models and loopback fixtures, using
the indexed pinned contract and official current protocol facts. Root consumes
it for the first provider, keeping runtime/media-player/OAuth registration
wiring primary-owned. No real account, provider requests, playback/device
commands, shared manifests, Git or children. Same explicit native Luna/max
selection; release after focused source/protocol checks and handoff.

Host v121 and API v122 are released; root now owns their activation and fixes.
The host's first nine SDK cases, Clippy and formatting passed. Its later 502
reason-mapping case is pending because Core is mid-correction. Root activated
the eight HTTP cases and proved the first complete callback/deferred-exchange/
private-entry path. Other fixtures needed source-compatible abort expectations,
shared-result concurrency and cancellation-safe test-server completion. Further
builds wait for Core's clean checkpoint. The pure client now passes 30 cases,
including immediate pending-flow cancellation and capacity recovery.

Queued `oauth_host_review_v125` reads only the frozen host source/tests to
challenge commit-admission cancellation, deferred activation ownership, scoped
cleanup, provider registration identity and private proof/session binding.
Core may remain temporarily unbuildable; the reviewer must not touch it or
claim an unrun probe. Same explicit Luna/max native policy, no children or Git.
Root consumes the evidence and verifies integration; completion releases review.

Review v124 is released and found unfinished boundaries in v120: options and
subentry submissions escaped task ownership; subentries could commit after
abort, shutdown did not close admission, Running progress could expose reserved
private context keys, and event failures could skip deferred activation or abort
joining. The reported 36-case pass is not full acceptance. Core owner v120 is
intentionally resumed in correction for those concrete races; public host APIs
stay frozen. Root owns all other integration and will review the corrected
source and targeted failures before the next gate.

Core owner v120 is released with 36 tests, strict Clippy and formatting passing.
Its awaited drain includes actual legacy blocking callback completion. Queued
`oauth_config_review_v124` reads that frozen Core change (SHA-256
`c92de680a04ec06e12f36ab455861896c0938573c43c8355c1c9512165a800d4`)
to challenge atomic lease/entry commits, private event redaction, deferred
callbacks and cancellation/drain races. Read-only Core scope and optional owned
synthetic probes; no edits, real providers, Git or children. Same explicit
native Luna/max selection. Root integrates findings and verifies the result;
completion releases the reviewer.

Running `/root/spotify_contract_v123` owns only `docs/SPOTIFY-PORT.md`, the next
production-provider source contract for the primary. Audit the pinned adapter,
its exact package/wire behavior and focused tests, with primary public protocol
documentation as needed. Define native OAuth/account identity, playback/device
state, services, polling and unload falsifiers. Read/download source only;
no package execution, provider API/account access, playback commands, Git or
children. Root indexes and consumes the result for implementation after the
generic host contract. Same explicit native `gpt-6-luna`/`max` selection and base;
completion releases the source auditor.

Running `/root/oauth_flow_api_v122` owns only the new API OAuth helper module and draft
loopback protocol fixture. It consumes the host's weak manager, adds a public
signed-state callback with fixed close-window HTML and bounded/strict callback
parameter parsing, sanitized error mapping, and shutdown-aware flow waits.
Root wires existing routes and keeps their administrator boundary. Callback
origins come from root-validated host options; arbitrary frontend headers do not
override them. Fixtures exercise the actual HTTP flow/callback/token/store path,
duplicate and invalid callbacks, reauth, abort and shutdown without real OAuth
providers. Same explicit native Luna/max policy and base; no children, shared
manifests, existing route edits or Git. Release after API contract and focused
handoff; root activates and verifies the draft.

Running `/root/oauth_flow_host_v121` owns only the isolated future
`crates/hass-oauth2-flow/**`. It consumes v120's frozen exact-flow leases,
atomic completion and deferred activation, plus the client's generic
`commit_authorization` hook. Deliver a real SDK path from loaded credential
selection through external URL, verified public callback, external_done,
one-time async resolve, private entry/token commit and reauth/refresh. Keep all
proofs/codes/tokens out of public flow context. Weak manager/registration handles,
bounded owned resolver tasks and joined abort/shutdown are mandatory. Root owns
the API's callback-origin trust checks and all shared enrollment/wiring. No
production providers, real credentials, Git or children. Same base and explicit
native `gpt-6-luna`/`max` policy; backend/tier unexposed. Send frozen public APIs
early for the API adapter, then concise checks/lifecycle handoff and release.

The accepted Credentials source is d9025ac2 with exact Linux CI green; its
immutable receipt is separate from this next wave. Root owns OAuth client
enrollment, shared manifests/lock, app/API wiring, documentation, checks and Git.
The corrected isolated client passes 27 tests and strict Clippy; review v119
is released with no confirmed lifecycle defect. Successful shutdown joins
admitted commits; last-owner Drop is an abort fallback.

Running `/root/oauth_config_flows_v120` owns only `crates/hass-config/**`: native
external/external_done values, asynchronous flow dispatch compatible with
existing synchronous drivers, and narrow trusted private-data transactions.
The host must perform provider I/O outside config locks and blocking adapters.
Abort/owner removal must defeat stale completion; concurrent refresh advances
an external_done step once. Refresh patches only token data atomically. Reauth
commits preserve unrelated entry data/options and complete the exact flow, with
setup/reload callbacks deferred until after the token account lock is released.
Root supplies the generic OAuth authorization commit callback in the client.
Worker proposes frozen public signatures promptly for the next host owner,
then implements and verifies focused fixtures. No shared manifests, API/app,
real providers, Git or children. Base is the published Credentials receipt plus
its documentation commit. The current native catalog has one Luna model;
dispatch explicitly uses `gpt-6-luna`/`max`, no history fork, backend/tier
unexposed. Completion releases the worker after concise evidence-backed handoff.

### Credentials integration and OAuth client corrections (2026-11-07)

Root owns the enrolled member-40 Credentials SDK, API, app wiring and shared
manifests. Released builders v107/v108 and reviews v110/v113 are consumed;
root fixed platform-destructor lock reentry and retains the source's loaded
platform prerequisite for config-entry lookup. Root corrected schema-before-
authorization ordering and strict unsubscribe fields. Unicode owner v115 is
released; root integrated its corrected module/data/notices, dependency and call
site. The first full member-40 gate exposed an mDNS loopback port collision;
the read-only investigation found that fixtures release ephemeral reservations
and reuse-port sockets can overlap. Root owns deterministic fixture isolation
and the final release check. The final exact-source gate passes 998 tests over
114 targets (zero failed or ignored), formatting and strict locked offline
workspace all-target Clippy and the optimized workspace build. Seven optimized
CLI cases pass without extra files/database or ignored-secret echo. The macOS
binary SHA-256 is `6a517d876da5d337f80f8656e91ddb29b6a1f4f3a0604178db097c448bb17d57`.
Source `d9025ac2860539d05752990723d6b138f5c606a2` is published and passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42398882199),
including formatting, strict all-target Clippy, all tests and CLI validation.
Root independently resolved all 662 partial-row evidence paths and 373 Rust
function references, checked that only Credentials' three domain-row fields
changed, and retained all pinned source snapshots. Inventory correction v99 is
released with 45 partial / 1,520 unported / zero complete; root reviews its
semantic delta and evidence before publishing.

The primary's nine loopback API cases now pass, including malformed non-admin
requests, strict unsubscribe without removing the stream, quiet session
revocation, live removal and one terminal error on owner shutdown. The previous
full API run and strict workspace Clippy pass; they predate the final owner-stop
correction and Unicode integration and are not the final wave receipt. All four
mDNS backend fixtures pass with process-unique custom ports; the real join and
port-release assertions remain. The collision mechanism is consistent with
reuse-port fixture overlap; the original failure did not record its port, so a
particular competing holder was not identified. No production socket behavior
was weakened.

The first final-gate compilation was interrupted before any test target ran:
the host had only 6.9 GiB free. Root stopped its Cargo and the reviewer stopped
its isolated probe, verified no compiler remained, and used Cargo's scoped
debug-profile cleanup. Cargo reports 197.3 GiB of generated artifacts removed;
free space recovered to 154 GiB. Source, stores, optimized binaries and other
projects were retained. Subsequent gates use four build jobs with incremental
compilation disabled; this changes cache use, not test behavior. The interrupted
log is not a passing test receipt.

Correction `/root/oauth_client_corrections_v116` is released: root consumes the released v111 SDK and
v112 review. Exclusive write scope is `crates/hass-oauth2-client/**`, still an
isolated future workspace outside member 40. Correct fresh-token validation,
reserved authorization query parameters, transport policy and cancellation-safe
refresh persistence without overwriting newer account credentials. Verify with
owned loopback fixtures and injected stores/clocks; no real providers, tokens,
credentials, root manifests, shared docs or Git changes. Base is
`5ea6c85f6f257e558a467f70ade512632f8ec87d` plus the released isolated SDK.
The live native catalog exposes one Luna member, `gpt-6-luna`, at maximum
supported effort `max`; dispatch requests both explicitly with no history fork.
Backend identity and tier remain unexposed. No children. Release on a concise
handoff with changes, checks, lifecycle proof and residual decisions; root
reviews and integrates independently.

v116 reports 21 isolated loopback/store tests plus strict Clippy and formatting
passing. Its session-bound authorization carries a factory account revision;
unbound tokens can create only empty, untouched accounts. Admitted refresh/save
commits have tracked ownership and an explicit shutdown barrier. Actual store
failure after provider rotation remains an explicit error. No member-41
acceptance or root enrollment is implied. Released `/root/oauth_commit_review_v118`
reads only the frozen isolated crate to challenge cancellation, admission,
job joining and account-revision races. Same explicit `gpt-6-luna`/`max` native
policy and base; no children or source edits. Root consumes a concise evidence-
backed handoff and performs integration checks; completion releases the reviewer.

v118 is released with three confirmed concerns: token-save permits lacked
factory/client identity, an unreachable pending job could retain its factory,
and pruning finished job handles could hide a panic from shutdown. The loopback
probe accepted a cross-factory save; its follow-up overwrite probe did not run
because of a compile error. Root owns corrections and focused regression tests.
Primary review also found valid expiry coercions and receipt-time expiry need
correction. These findings remain outside member 40.

Root corrected those three findings with factory/client-bound permits, a
separate last-owner cancellation guard and retained panic results during join
handle reaping. Successful shutdown joins admitted commits; last-owner Drop
closes admission and aborts retained work as a cleanup fallback. Token loads now
share the owned job path. Refresh expiry uses response receipt time; valid
Python integer coercions, Unicode 16 decimal digits and finite floats normalize
to integer lifetimes, while bool/null/invalid/zero/overflow remain typed errors.
All 27 isolated tests, strict all-target Clippy and formatting pass. Queued
`oauth_lifecycle_review_v119` reads the frozen corrected SDK to challenge the
new owner/abort/coordinator path and permit binding. No source changes,
providers, children or Git; same explicit native `gpt-6-luna`/`max` policy.
Root consumes the evidence and performs final integration checks; completion
releases the reviewer. This is still outside the member-40 receipt.

Released `/root/oauth_flow_contract_v117` owns only an additive host-flow section in
`docs/OAUTH2-CLIENT-PORT.md`. Root will consume its pinned-source contracts for
external-step/done serialization, callback routing, private flow proof state,
config-entry creation and reauthentication. It must distinguish provider OAuth
from native login and identify current Rust seams and concrete loopback
falsifiers. Read-only source analysis plus that document; no SDK/API/app or
manifest edits, code execution from upstream, real providers or child agents.
Same base and explicit native `gpt-6-luna`/`max` policy as v116; backend/tier
unexposed. Release after evidence-backed handoff; root integrates the contract.

Released inventory correction `/root/usb_inventory_v99` owns only PARITY.md and
upstream-parity.json for Application Credentials. Its native subset evidence
must keep OAuth engines/providers and full upstream acceptance open. Root supplies
the final build/hash/CI receipts; no worker Git mutations or inferred receipts.
This intentional same-scope follow-up retains the already configured native
`gpt-6-luna`/`max` worker; root verifies semantic row deltas and all evidence.

### USB native boundary and next Hardware audit (2026-11-07)

Root integrates the released USB SDK, Linux/Darwin backend, admin API adapter,
actual config-entry usage provider and owned Tokio timer scheduler. SDK,
backend and API/usage reviews have independent scopes; root corrected lost
debounced diagnostics, provider-only lookup, callback/stop races, dangling
symlink identity and typed flow provenance. USB flow data has no persisted
discovery key. Native init-data provenance is separate from user JSON source
and survives form steps; removals match typed original data and exact domains,
including protected flows. Normal SSDP/Zeroconf protection remains unchanged.

The final 37-crate gate passes 929 tests over 105 targets with zero failures or
ignored cases, including all 143 app-bin cases. Strict locked offline
workspace all-target Clippy, formatting, optimized workspace build and CLI
validation pass. The optimized macOS binary SHA-256 is
`98e4f4a2e30ce750d6b5b83805c0649d399a5768afcbe84fb54e24abf55d7da3`.
No live USB tree, physical serial port, IOKit inventory or device command was
exercised. USB source `eb4f8da2e331ae6a83c036ef14eb514fd8eb3a39` is published;
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42380709474)
passes, including native temporary-tree inotify tests. Hardware work is not
part of that immutable receipt. Root resolved all 554 partial-row evidence
paths and 304 symbol references, confirmed only USB's three integration-row
fields changed, and reconciled the stale policy summary to all 43 partial
domains (1,522 unported; zero complete).

Completed USB owners and reviewers are released. The Hardware source and
dependency audit is complete in `docs/HARDWARE-PORT.md`; root selected direct
Linux proc/Mac Mach metrics to preserve the pinned available-memory formulas
and explicit failure handling. The released SDK is now workspace member 38.
Root corrected receiver closure, zero-total-memory acceptance, canceled info
request ownership, timestamp precision and interval cadence; all 16 injected
SDK cases and strict all-target Clippy pass. The released API adapter's seven
initial loopback cases pass after those corrections, and its full API
regression run passes. API review found revoked sessions could retain silent
failed-metrics streams. Root added bounded poll-time revalidation and an eighth
protocol case proving close and sampler release without any status event.
Strict all-target API Clippy passes; the lag case also checks another public
event subscription survives terminal Hardware removal.

The native metrics owner is released and `hass-hardware-metrics` is member 39.
Root corrected Python decimal-rounding behavior after independent review;
all 15 backend fixture cases and strict Clippy pass, and 17,007 pure Python
rounding oracles agree with the helper. Linux conditional code passes
supplementary cross-target Clippy. App/YAML wiring now passes its targeted
configuration case and actual compiled-process hardware/info plus joined
idle-sampler shutdown. No host metrics subscription was made.

Root owns all released SDK/API/backend code, app/YAML wiring, root
manifests/lock, index, checks and Git. The final 39-crate gate passes 970 tests
over 110 targets (zero failures/ignored), formatting, strict locked offline
workspace all-target Clippy and release build. The optimized macOS binary
SHA-256 is `dfa016496a5979e7956843e3c494632d1c9ae3a12335c8b126848125d32c51c9`.
Example/empty/Hardware-empty/false/ignored CLI configs pass without extra
files or secret echo; invalid root keys fail. Inventory reconciliation to
Hardware is partial with 44 partial / 1,521 unported / zero complete domains.
Root resolved all 615 partial-row evidence paths and 345 symbols, confirmed
only Hardware's three domain-row fields changed and retained all source
snapshots. Source `4d875e379b9a91d2adcbcec5b2e3279e55dbe48a` is published;
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42385397159)
passes formatting, strict all-target Clippy, all workspace tests and
CLI validation. This source excludes the next Credentials SDK/API drafts.

The Application Credentials source audit is released in its indexed contract.
One owner implements only the isolated next `hass-application-credentials`
SDK; it is outside this 39-crate acceptance and publication. Root chose to
protect imported credential aliases from in-use deletion and will document
that intentional correction to the pinned source behavior.
All workers use explicit native
`gpt-6-luna` with `max`; backend/tier are unexposed. No worker children or live
hardware/Supervisor probing is authorized by these fixture assignments.

### Sun next-port investigation (2026-11-06)

Queued `sun_native_plan_v10`: read-only pinned Sun/astral contract and native algorithm investigation for the primary's next executable port. Base `831dce75694a2bdba0777aaeef985614895eacae`, shared tag/composite/recording changes present. Exclusive deliverable `docs/SUN-PORT.md`; no source, manifest, Git or live-service changes. Resolve entity, config-entry, observer, timestamp, condition and trigger behavior with source anchors and deterministic astronomical falsifiers. Native catalog has one Luna member, `gpt-6-luna` with `max`; explicit selection, backend/tier unexposed. Release on concise evidence-backed handoff; root integrates the result.

Registry composite correction: independent `registry_migration_review_v9` completed read-only review and is released. It found startup parent cleanup, deleted-device splits, first-registration identifier replacement and composite-parent alias resolution gaps. `registry_composite_v9` reopens exclusive `crates/hass-registry/**` except `provider.rs` to correct them against pinned source, with regression tests. Primary retains all manifest/API/app/docs ownership. No other registry source writer; release after focused checks and correction handoff, followed by primary integration.

Queued `tag_inventory_v9`, same explicit native `gpt-6-luna` / `max` selection and unexposed backend/tier: exclusive `docs/PARITY.md` and `docs/upstream-parity.json`. Update the existing inventory from tag unported to partial with actual current Rust evidence and remaining boundaries, and record history-only attribute projection without declaring recorder complete. Do not claim the pending composite correction is accepted. Root consumes reconciled 1,565-domain / 32-batch inventory; no implementation or Git mutations. Release on concise reconciliation handoff.

Queued read-only `tag_lifecycle_review_v9`, explicit native `gpt-6-luna` / `max`, backend/tier unexposed. Consumer is primary acceptance of tag collection write/release/restart behavior; scope new tag manager and its API adapter, especially failed writes, removed/disabled leases, lock reentry and name ownership. No source edits or duplicated broad tests; root is the sole correction writer. Release on concrete findings or a bounded no-findings report with inspected limits.

Queued next-wave `logger_native_v10`, explicit native `gpt-6-luna` / `max`, backend/tier unexposed. Exclusive new `crates/hass-logger/**`; it remains outside the 24-crate manifest/publication until primary opens integration. Consumer is the app's actual tracing subscriber plus authenticated logger adapters. Implement real mutable level and message filtering, typed YAML, source settings persistence/once/permanent semantics and owned admin services; no simulated sink or Python bridge. Root owns global subscriber, manifests, auth/API and shared integration. Worker may create an isolated temporary compile harness for its new crate, not another checkout; keep generated output out of deliverables. Release on source-backed package handoff; primary independently verifies actual transport/process logging.

Sun investigation completed/released with `docs/SUN-PORT.md`; primary read its contracts and deterministic ordinary/polar event fixtures. Same owner reopens next-wave implementation exclusively `crates/hass-sun/**` for the pure Rust astronomical algorithm, date scanning and reference tests; no app/automation host yet. Source algorithm must come from exact Astral 2.2 source and retain its license/attribution, rather than treating current stable documentation as that version. Native model/effort remain explicit Luna/max, backend/tier unexposed. New crate stays outside the 24-crate workspace gate; release after formula/fixture handoff, with primary owning runtime integration and final numerical judgment.

### Durable runtime metadata

One worker owns `crates/hass-core/**` and `crates/hass-storage/**`. Extend Store with `get_data(&self, key: &str) -> Result<Option<Value>>`, `set_data(&self, key: &str, value: &Value) -> Result<()>`, `delete_data(&self, key: &str) -> Result<bool>`. Runtime delegates through public same-named methods, serializes writes with its existing write-order lock, persists before success. MemoryStore implements real data storage; no default no-op trait methods. Preserve existing runtime/store ownership locks, timestamps, history, error variants and APIs. SQLite migrates owned schema v1 -> v2 with a `metadata(key TEXT PRIMARY KEY NOT NULL, value_json TEXT NOT NULL)` table; atomic migration and rollback; v1 states/history survive; refuse unknown schemas/versions before changing them. Regressions exercise metadata restart/delete/error and v1 migration. All downstream workers call Runtime metadata; no independent database files or direct SQLite writers.

### Registries

`crates/hass-registry/**`: public cloneable `Registries::new(runtime: Runtime) -> Result<Self>`; `handle_ws(&self, command: &Value) -> Result<Value>`; implement native persistent entity/device/area/floor/label registries matching pinned upstream command names/payload fields. Provide typed public CRUD helpers and validated stable IDs, duplicate/foreign-reference checks and cleanup on deletion. Registry updates persist before in-memory mutation and emit the corresponding HA registry_updated event after persistence. Unknown fields/commands fail explicitly. Get/list/create/update/remove commands must return native shapes expected by upstream API. Focused tests must exercise restart, identity/unique-ID collisions, foreign keys, error atomicity, event payloads. Read actual upstream registry/websocket code. Do not treat bare JSON passthrough or a command table as implementation.

### Authentication

`crates/hass-auth/**`: public cloneable `AuthManager::new(runtime: Runtime) -> Result<Self>`, native password users/admin roles, Argon2 password hashes, refresh/session/token persistence and revocation, JWT access tokens with expiry/signature validation, constant-time secret checks. Expose typed operations for bootstrap/create user, password auth, issue/refresh/revoke tokens, validate_access_token -> user identity, and public `handle_ws(&Value, caller_user_id: &str) -> Result<Value>` for actual supported auth commands. Return a serializable public `TokenSet` with access_token, token_type, expires_in, refresh_token. Expose a cloneable serializable `User { id, name, is_owner, is_active, group_ids, ... }` compatible with auth/current_user; never serialize password hashes/secrets in public objects. Configuration bootstrap is local only; HTTP adapters owned by primary. Inspect upstream auth provider/login flow and tests; authenticate disabled users/revocation and persist failures correctly. Explicitly document implemented vs remaining upstream auth behavior.

### Templates

`crates/hass-template/**`: public cloneable `TemplateEngine::new(runtime: Runtime)` and `render(&self, template: &str, variables: &Map<String, Value>) -> Result<String>`; `render_value(...) -> Result<Value>` for native typed template consumers. Use Minijinja with fuel/resource bounds and real HA-compatible states/is_state/state_attr/is_state_attr/has_value helpers, core filters/tests, variables, numeric fallback semantics, timestamps/datetimes and safe error reporting. Read upstream template helpers and extract representative fixture expectations. No arbitrary file loading, shell execution, or unsandboxed access. Missing/invalid values follow explicit upstream behavior, not invented zeroes. Tests use actual state contexts and native rendering, including malformed templates/resource exhaustion.

### Scripts and scenes

`crates/hass-script/**`: public strict Deserialize/Clone/Debug `ScriptConfig` and `SceneConfig`; cloneable `ScriptEngine::start(runtime: Runtime, scripts: BTreeMap<String, ScriptConfig>, scenes: Vec<SceneConfig>) -> Result<Self>`; `shutdown(self).await -> Result<()>`. Register native script.<object_id>, script.turn_on/turn_off/toggle, scene.turn_on/create and persistent scene snapshots where supported. Execute real typed sequences: service/action, delay, condition, choose, repeat/count, variables, stop; enforce bounded work, cancellation and loop detection. Support single/restart/queued/parallel modes with actual scheduling semantics and bounded queues. Scene application restores state/attributes through available entity services; do not claim physical device control via state injection. Read upstream script/scene tests and document unsupported constructs explicitly. Dependency on hass-template allowed; templates worker owns that crate.

### MQTT

`crates/hass-mqtt/**`: strict Deserialize/Clone/Debug `MqttConfig`; `MqttIntegration::start(runtime: Runtime, config: MqttConfig).await -> Result<Self>` and shutdown(self).await -> Result<()>. Native rumqttc client, authenticated connection with optional TLS, broker subscriptions, state/availability topics, sensors/binary_sensors/switches/lights and HA discovery, service command publication, reconnect/resubscription, bounded retained/discovery payloads, unique/entity identity and cleanup. Command publication must go through the broker; do not manufacture reported state when optimistic behavior is not explicitly configured. Public config shape and sample YAML required early. Test with a real local MQTT wire fixture or broker and prove state input -> runtime, service -> outgoing command, QoS/reconnect/availability, malformed discovery and shutdown. No external broker or live device traffic.

### Native helper lifecycles

`crates/hass-native/**`: strict Deserialize/Clone/Debug/Default `NativeConfig` with timer/input_button/input_datetime/schedule maps; `NativeIntegrations::start(runtime: Runtime, config: NativeConfig).await -> Result<Self>` and shutdown(self).await -> Result<()>. Real timer services/countdown state/events, cancellation/pause/restore; button press timestamp/events; datetime validation/set service; timezone-aware schedule transitions. Use Runtime metadata for restart state. Read corresponding pinned integration behavior. Validate all configuration before starting tasks or mutation; no detached tasks on shutdown. Expose exact root-compatible config syntax and errors. Focused tests use paused Tokio time or short timers, persist/restart and invalid dates/timezones.

### Exhaustive source and parity inventory

`docs/upstream-parity.json`, `docs/PARITY.md` only: inventory every pinned upstream manifest, dependency graph, source/test files and exported surface by processing git objects. Record native candidates and exact evidence fields; unported defaults remain unported. Use all 1,565 manifests, not a hand-selected subset. Classify coherent port batches by shared native protocols/dependencies and generate ordered, actionable remaining work. No invented completed counts, no automatic source translation claimed as behavior. This inventory is used by the primary to drive subsequent waves.

### Config entries, flows and integration lifecycle

`crates/hass-config/**`: cloneable `ConfigEntries::new(Runtime) -> Result<Self>`, `handle_ws(&Value) -> Result<Value>` (inner payload), typed real entries/flow/lifecycle APIs. Persist config-entry data/options and disabled/reauth/reload state through Runtime metadata; expose native integration driver registration with setup/unload/config-flow handlers that execute real registered driver code. Missing drivers fail explicitly; never mark setup successful for unimplemented integrations. Match pinned config entry/flow adapters and validate unique IDs, source/version transitions, options, removal cleanup and error state. Secret entry data does not appear in ordinary public list responses. Core config persistence (name/location/timezone/units/URLs) may live here with validated read/update methods for primary API adapters. Tests must invoke a focused native fixture driver to prove setup/unload/retry/failure/flow behavior and restart state. Primary connects real drivers and owns HTTP routing; worker owns only crate files.

### RESTful sensors and commands

`crates/hass-rest/**`: strict Clone/Debug/Deserialize `RestConfig` and `RestCommandConfig`; `RestIntegration::start(Runtime, Vec<RestConfig>, BTreeMap<String,RestCommandConfig>).await -> Result<Self>`, `shutdown(self).await -> Result<()>`, and pure configuration validation. Match pinned HA `rest:` resources/sensor definitions, timeouts/polling, templates/JSON attributes/availability and `rest_command` services with actual reqwest network IO. Use hass-template renderer for state/body/URL expressions. Preserve errors and missing readings as unavailable, never numeric zero. Header/password/token values must not leak via Debug/logs/state. Bound bodies, retries and concurrent polls; cancel/join work on shutdown. Loopback fixture tests prove actual requests/responses and service payloads, HTTP/JSON/timeouts/recovery, and restart behavior. No production/external network calls; own crate only, root owns adapters/config.

### Automation trigger and execution parity

`crates/hass-automation/**`: extend the real engine to native numeric-state, template, time/time-pattern, startup/shutdown and MQTT event triggers in addition to existing state/event. Accept pinned modern/legacy aliases and trigger lists; support `for` durations, state attribute triggers and variables/conditions using the template renderer. Pure validate and `start`/shutdown APIs remain compatible; add timezone-aware start variant. Reuse `hass-script::validate_sequence(&Value) -> Result<()>` and `pub async run_sequence(Runtime, Value, Map<String,Value>, Context, watch::Receiver<bool>) -> Result<()>` rather than a second action executor. The scripts owner supplies those exports. Support actual single/restart/queued/parallel run modes and enable/disable/trigger services, backed by persisted state. Preserve causal loop prevention, bounded queues and honest lag/error reporting. MQTT source messages use native events only when supplied by the real integration; do not invent broker/device activity. Unsupported triggers must reject configuration explicitly until their real provider is ported. Tests must prove time/numeric thresholds/template updates, modes/conditions/variables/delays/cancellation/restore and action execution through the shared script executor.

### Network origin and local-user policy

`crates/hass-api/src/network.rs` only: public Clone/Debug/Default `NetworkPolicy`, `NetworkPolicy::new(use_x_forwarded_for: bool, trusted_proxies: &[String]) -> hass_core::Result<Self>`, `resolve(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> Result<ClientOrigin>`, with cloneable `ClientOrigin { pub client_ip: Option<IpAddr>, pub is_local: bool }`. Correctly parse IP/CIDR proxy lists, validate known socket peer before trusting forwarding, reject unconfigured/untrusted/malformed forwarded chains, peel trusted hops from the right, and classify actual local addresses. Unknown peer is not local. Read pinned HA forwarded middleware/network helpers. Tests cover trusted chains/spoofed left hops, IPv4/IPv6, malformed or multi-value headers and missing peer. Primary owns adding the module export, HTTP middleware/ConnectInfo, local-only user gates and app configuration. No edits to other files; root ipnet dependency is declared.

### Auth session identity, enrollment and MFA

`crates/hass-auth/**` is reassigned after auth_v2 completed: preserve current APIs and passing tests, expose `validate_access_token_identity(&str) -> Result<AccessIdentity { pub user:User, pub session_id:String }>` and `handle_ws_session(&Value, caller_user_id:&str, current_session:Option<&str>)->Result<Value>` so is_current and delete-all-except-current are accurate. Implement actual local credential enrollment/update/password change and signed-path capability APIs using pinned auth/provider adapters. Add TOTP MFA enrollment/verify/disable with persisted secrets and replay/time-window handling; do not equate password verification with MFA completion or issue tokens after an incomplete MFA challenge. Send exact additive methods and HTTP login-flow adapter requirements before making MFA available. Primary owns API transport and current-session/signed-path scope enforcement. Use real Argon2/JWT/TOTP fixture checks, never plaintext passwords in persistence or credential material in public responses.

### Native frontend subscriptions

`crates/hass-api/src/subscriptions.rs` only: implement `pub(crate) NativeSubscriptions::new(Runtime, Option<ConfigEntries>, Option<TemplateEngine>)`, `handle_command(&mut self, &Value)->Result<Option<SubscriptionCommandResult>>`, `drain_events(&mut self)->Result<Vec<Value>>`, `len/is_empty`. `SubscriptionCommandResult { pub result:Value, pub events:Vec<Value> }` separates result payload from initial event envelopes; primary sends ACK then events. Handle actual pinned subscribe_events/unsubscribe_events, subscribe_entities compact initial/update/remove protocol, config-entry and config-flow progress subscriptions, and render_template subscriptions where implemented. Read actual pinned source; unsupported names reject explicitly. Manager owns fresh receivers and max32 subscriptions, real cleanup, bounded draining, lag errors, and never acknowledges a subscription without storing its actual receiver/transform. Runtime::subscribe_with_states atomically returns initial states plus a fresh receiver to avoid stale queued-event rollback. Root calls manager in spawn_blocking and drains periodically; it owns socket/identity/shutdown and module export. Private MQTT traffic is never public subscribe_events data. Tests must prove actual source-compatible payloads, initial snapshot/update/remove, unsubscribe/lag and secrets/limits using Runtime mutations.

### MQTT platform completion

`crates/hass-mqtt/**` is reassigned after mqtt_v2 completed: preserve config/start/shutdown/signal/subscription/metadata APIs and extend actual wire-driven platform behavior. Prioritize full light brightness/color-temperature/RGB/HS/XY/effect/JSON-schema commands and reported state; then cover/fan/button/number/select/text platform services and discovery. Use exact pinned MQTT schemas and fixture payloads, real broker publication/state consumption, bounds/features/availability and removal. Reject unknown capabilities, never count a published command as a reported device state. Add focused actual local wire tests for every added command/state mapping. Root owns registry/config-entry adapters, no edits there.

### Executable native config-entry driver host

`crates/hass/src/integration_drivers.rs` plus narrowly required additive MQTT serialization/registry-rebind APIs in `crates/hass-mqtt/src/lib.rs` (MQTT platform owner completed): implement `NativeDrivers::start(runtime: Runtime, entries: ConfigEntries, registries: Registries, mqtt_topics: Vec<String>, yaml_mqtt: Option<MqttConfig>).await -> Result<Self>` and `shutdown(self).await -> Result<()>`. Own actual MQTT config-entry driver setup/unload/config/options flows, YAML import/update, restart restoration and retry scheduling. Callback setup/unload run in blocking context and use captured Tokio Handle to await the actual client. Never mark ready without actual CONNACK/subscriptions. Host owns all MQTT handles, subscribes real automation topic filters after the automation listener is active, and links actual discovered metadata into native registries with correct config-entry identity, devices and entity IDs. Add only necessary safe `MqttConfig: Serialize` and `MqttIntegration` entity-ID rebinding method; preserve existing public APIs and all wire behavior. All callbacks/tasks cancel and join before shutdown. Primary owns main/module export, root manifest/dependencies, HTTP adapters and other crates. Focused tests prove real local MQTT driver setup/registry linkage, failed setup retry (no fake loaded), reload/unload/removal and restart, not merely mock callback invocation.

## Dispatch ledger

Next independent port scopes: YAML includes/secrets and package merging are owned by a worker in `crates/hass/src/yaml.rs` plus its own fixtures, with root retaining Config::load wiring. Recorder/history storage extensions are owned by one worker in `hass-core` and `hass-storage` after the primary's metadata/privacy edits; retain every current API and ownership guarantee. Reactive native template entities are owned by a worker in new `crates/hass-template-entities/**`, with root owning manifest membership/config/startup. Each needs pinned-source behavior, executable restart/cancellation/error checks and a bounded handoff; configuration syntax alone is not acceptance.

Shared entity services must dispatch by instance ownership and actual entity identity. The core owner adds owner-scoped registration/unregistration and prevalidates target routing; MQTT and template-entity owners adapt their handlers and unload cleanup. A separate bounded API target resolver owns `crates/hass-api/src/targets.rs` only, using native registries for actual device/area/floor/label selection. Root owns HTTP/WebSocket call-site wiring and authorization.

Primary recorder maintenance owns app `recorder.rs`: admin purge, optional actual repack, and joined timezone-aware nightly retention with pinned defaults. Auth callbacks hold a weak manager reference to avoid a Runtime/service/auth ownership cycle. Template startup receives the same registry manager used by MQTT and API; creating a second independent registry cache is disallowed in the integrated app path. A read-only lifecycle review checks remaining handler/task ownership cycles and restart release; it does not duplicate behavior porting or own edits.

Lifecycle review confirmed REST command/TemplateEngine cycles and template-provider drop cleanup gaps. The core owner is reassigned to add owned legacy services and a guarded target-resolver hook, preserving old APIs. REST has a separate owner for registration/drop/failure cleanup; the template-entity owner handles its Drop fallback. The target resolver moves to `hass-registry::targets` so internal scripts/automations and external calls share actual registry selection through the Runtime hook. Primary holds resolver guards through shutdown; guards use weak Runtime ownership and remove only their own installation.

Primary followup ownership: native config-entry HTTP adapters are assigned to a new worker in `crates/hass-api/src/config_entries.rs` and its separate HTTP protocol fixture only. The primary retains router wiring and all other API files. The adapter must run real registered driver callbacks in blocking context, match pinned flow schemas, enforce admin roles, and prove secret-safe list/flow/reload/removal through actual loopback HTTP.

Worker policy reread for this wave: newest callable Luna in live native catalog is `gpt-6-luna`, maximum effort `max`. Every spawn explicitly selects both with context-free dispatch. Backend observed pair and service tier are not exposed by native status, so remain unverified. Completion releases native capacity. Consumer of each handoff is the primary and listed downstream crates. Base revision is the published commit above plus this primary-owned contract.

| Task | Exclusive scope | Focused acceptance | Status |
| --- | --- | --- | --- |
| /root/metadata_v2 | hass-core + hass-storage | durable metadata and v1 migration | accepted/released; 26 actual workspace tests; primary now owns additive runtime signals/timezone/snapshot |
| /root/registries_v2 | hass-registry | CRUD/restart/identity/events | accepted/released; 6 tests + strict lint; primary owns subsequent integration |
| /root/auth_v2 | hass-auth | password/session/JWT/revocation/restart | accepted/released; 7 tests + actual HTTP/WS round trip; scope reassigned below |
| /root/templates_v2 | hass-template | pinned expression fixtures and bounds | accepted/released; 6 fixtures; primary added configured-timezone regression and fixed 5 lints |
| /root/scripts_v2 | hass-script | modes/actions/scenes/cancellation | accepted/released; 7 tests and shared action executor; root wires lifecycle |
| /root/mqtt_v2 initial | hass-mqtt | actual local MQTT wire round trip | accepted/released; 3 wire/config tests; same actor given intentional feature followup below |
| /root/native_v2 | hass-native | timer/datetime/schedule lifecycle | accepted/released; 15 tests + strict lint; YAML reload/editable schedules remain open |
| /root/inventory_v2 | upstream-parity.json + PARITY.md | exhaustive pinned source coverage | accepted/released; all 1,565 manifests/32 batches reconciled, 0 complete |
| /root/config_entries_v2 | hass-config | real registered-driver flows/setup/unload/restore | accepted/released; 9 tests; primary adds HTTP forms/current-result storage/signals/real drivers |
| /root/rest_v2 | hass-rest | loopback HTTP polling/commands/errors | accepted/released; 7 loopback tests + strict lint |
| /root/automation_parity_v2 | hass-automation | threshold/time/template triggers and shared executor/modes | accepted/released; 14 tests; primary wired configured timezone and MQTT topic subscriptions |
| /root/network_policy_v2 | API network.rs only | origin/proxy chains and local-only identity source | accepted/released; 9 tests; primary wired and proved real HTTP/WS local-only denial |
| /root/auth_sessions_mfa_v2 | hass-auth, reassigned after completed builder | current-session accuracy, enrollment, MFA/signature checks | accepted/released; 14 backend tests; primary added 2 security regressions and real MFA transport acceptance |
| /root/frontend_subscriptions_v2 | API subscriptions.rs only | exact state/config/template streams and cleanup | accepted/released; 7 fixtures; primary wired live streams/session revalidation and roleless-state regression |
| /root/mqtt_v2 followup | hass-mqtt, followup after completed builder | advanced light and additional real wire platforms | accepted/released; 6 actual wire tests; MQTT scope reassigned to host owner below |
| auth_durability_review | read-only auth/core/storage | key/session/grant/persistence/serialization failure modes | accepted/released; primary corrected cross-challenge MFA retry bypass, private file modes/durability, peer-admin session revocation |
| native_driver_host | integration_drivers.rs + MQTT service ownership | actual MQTT entry/import/registry/retry lifecycle | accepted/released; 6 host + 6 MQTT wire fixtures, including provider coexistence, rename and cleanup |
| /root/config_http_v2 | API config_entries.rs + separate transport fixture | native flow/options/setup/reload/remove and secret-safe lists | accepted/released; actual HTTP fixture + all-target check + strict lint |
| /root/yaml_loader_v3 | app yaml.rs + own fixtures | actual include/secret/package semantics and cycles | accepted/released; primary verified all 11 tests in actual workspace and package/secret-error typed boundary |
| /root/recorder_history_v3 first pass | hass-core + hass-storage | boundary history/purge/repack plus safe shared entity service dispatch | accepted/released; 22 core + 26 storage tests and strict lint; primary wired HTTP/WS flags and retention |
| /root/recorder_history_v3 lifecycle followup | hass-core only | owned legacy callbacks and guarded shared target resolver | accepted/released; 26 core tests + strict lint |
| /root/template_entities_v3 | hass-template-entities | reactive states and native control action execution | accepted/released; 10 tests + strict lint/fmt; primary compiled-process reactive sensor/crash/restart passed |
| /root/service_targets_v3 | registry targets.rs + module export | actual registry selection shared by internal/external services | accepted/released; 8 resolver fixtures + strict lint; primary internal/HTTP/WS target transport passed |
| /root/rest_lifecycle_v3 | hass-rest only | remove callback cycles and failed-start/drop handler leaks | accepted/released; 11 total REST tests + strict lint/fmt; shutdown/drop/start failure and actual same-process lease release |
| /root/lifecycle_review_v3 | read-only native integrations/tasks | same-process writer lease release and startup-failure cleanup | accepted/released; two confirmed findings assigned to owners |
| primary | root, app, API adapters | integration and cross-module checks | running |

Cargo-generated lockfile resolution is permitted during scoped checks; workers may not hand-edit root manifests or Cargo.lock. Primary performs final lock acceptance and `--locked` validation after manifest stabilization. Keep valid crate targets present while implementing so unrelated packages can compile.

## Primary integration receipts

- Actual shared-workspace metadata/storage tests passed after dependency resolution, including migration/restart/rollback. Runtime now also has a private bounded dispatcher for actual integration signals (MQTT data never enters the public event/recorder bus), validated in-memory configured timezone, and atomic state snapshot/subscription.
- Template tests passed against the actual workspace (6 existing + configured Tokyo/New York live-update regression). All five inherited template Clippy issues were fixed in the primary-owned completed scope.
- The actual executable compiles with auth, registries, scripts/scenes, native lifecycle integrations, optional MQTT/REST, core config persistence, and templates.
- A real HTTP/WebSocket acceptance test passed for onboarding, one-use authorization code, JWT identity in persisted states, admin registry access, non-admin denial, client-bound refresh rotation/replay/revocation, login retry, redirect validation, and local-only users through a trusted proxy with a spoofed private prefix.
- A second real HTTP/WebSocket test passed for private user-bound MFA setup/QR, denied cross-user flow access, enrollment, required TOTP login challenge, replay rejection, one-use authorization-code exchange, and current-session MFA disable. Backend regressions preserve retry budgets across new challenges and manager restarts and reject a peer administrator's cross-account session revocation.
- Native subscriptions are wired into the actual socket. Fresh entity snapshots, incremental state/config/template/event delivery, unsubscribe, limits, lag, live session revalidation and private dispatcher separation are exercised. Primary API validation passed 28 unit tests and both auth transport fixtures before the added roleless-state fixture.
- SQLite owned database/sidecar/lock files are private (0600 on Unix); new app-created database directories use 0700. Existing foreign databases remain untouched by schema rejection. Metadata commits use FULL synchronous durability. These settings do not constitute physical power-loss testing.
- The full integrated workspace test suite passed 228 tests, including all compiled-process and real wire fixtures. Strict full-workspace Clippy, formatting, optimized build and optimized example validation passed on 2026-11-06. Implementation `2250d4780f44b4837766fba31b2e150078575c39` is published. This accepted implementation wave does not complete the full rewrite; subsequent ports use [PARITY.md](PARITY.md) and the exhaustive inventory.

## Next native integration wave

The accepted implementation is published as `2250d4780f44b4837766fba31b2e150078575c39`; [Linux CI run 42165900082](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42165900082) passed fmt/Clippy and exposed the automation test timing issue recorded in validation. The primary corrected its condition-based wait and verified all 14 automation tests plus strict lint; follow-up Linux verification remains pending. The optimized macOS binary SHA-256 is `8fab64f37ed792540c7ffcfb67949f79c479422c68a0da92eefbe0612fa7e9f5`. Subsequent dirty work must not be attributed to that immutable CI revision.

The published test correction is `89ef0e3907a39a6152c883343ccf3721a9e08850`, and [Linux run 42167148335](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42167148335) passed every check. The following native group/control changes remain unaccepted until their own integrated checks complete.

One worker owns new app `homeassistant.rs` for real generic turn_on/turn_off/toggle delegation and cleanup. It uses existing entity services and reported states, expands actual group membership with cycle/bounds checks, preserves causal context, and never fabricates a physical state change. The primary owns module/startup and shared target-resolver wiring.

One worker owns new `crates/hass-group/**` for legacy YAML groups and native dynamic membership where supported by pinned source. It implements actual aggregate states, nested dependency ordering, strict validation, persistence, events, native group services, and joined lifecycle. The primary owns workspace/config/startup wiring. Modern platform group behavior remains open until independently implemented and accepted.

Primary integration decisions: generic services and groups share one `GroupLookup`. Expansion uses the complete pinned helper's active-group lookup followed by its explicit old-style `group.*` state-attribute fallback; unknown non-group IDs remain direct targets. Main installs this expansion in the guarded Runtime target callback, so scripts, automations, HTTP and WebSocket calls share it. Expansion is bounded before provider invocation. `homeassistant` all-target selection includes only actual domains exposing the requested service and excludes redundant group/self rows and ineligible registry entities. Original resolved YAML key order is captured before strict decoding and passed to group startup for its observable order attribute.

The new event inventory reports actual public wildcard receiver counts from the native bus, including receiver drop cleanup, while excluding private dispatcher receivers. Its focused authenticated HTTP test passes. Generic control tests passed 11 fixtures; group and integrated process checks remain pending. These dirty changes are not covered by the green `89ef0e39` Linux run.

Group/control integration now passes all 41 app unit tests and all four compiled-process tests. The process path proves group creation/removal, member aggregation, generic delegation to a native helper, public listener counts and crash/restart with YAML definitions. It caught the obsolete helper-only `homeassistant` registrations; the primary removed that shim so the generic component owns those service names once. Group startup preserves YAML order. Service-created groups and manual YAML-membership overrides are ephemeral across restart as in pinned source; metadata contains only owned IDs for cleaning stale restored rows, never membership restore.

One independent worker owns new `crates/hass-notifications/**` for native `persistent_notification` behavior. It implements the pinned service/data/event/subscription contract with bounded validation and real lifecycle cleanup. This is the application's internal notification feed; no messages to other people or external services are authorized or sent. The primary owns app/API/subscription wiring and transport acceptance. Restart semantics follow pinned source rather than inventing persistence from the integration name.

Notification transport is wired and passes an ordinary-user HTTP/WebSocket fixture for ACK/current snapshot, add/update/remove, ordered get, unsubscribe and private-bus separation. Primary source review found two remaining component edges: Home Assistant's `cv.string` coerces JSON boolean/numeric scalars, and the component exports a native automation trigger. Primary owns service coercion; the automation owner is reassigned to its trigger variant/private signal path and actual notification-service fixtures. The pinned callback registration does not emit a synthetic initial current notification event.

Trigger followup contract (`/root/automation_parity_v2`, owns `hass-automation` only): add `persistent_notification` trigger, optional notification ID (empty means unfiltered), scalar/list update-type filter (`current`/`added`/`updated`/`removed`), one run per matching changed notification, and `trigger.platform/update_type/notification` variables. Consume real private `persistent_notifications_updated` signals even when no MQTT trigger exists; preserve conditions/modes/causal loop protection and public-bus separation. Test through actual notification services with a dev dependency, retain the published queue-overflow CI wait fix, and do not seed an initial current event because pinned callback registration only connects.

The integrated 16-crate workspace passed 259 tests plus formatting and strict lint before the final scalar/trigger review. Scalar coercion now has a ninth notification fixture covering Python-compatible booleans, integer IDs and float strings. REST remote-event origin and actual public listener counts have focused API checks. Final wave validation/publication waits for the trigger followup; these receipts do not extend the immutable `89ef0e39` CI claim.

## Location and helper continuation

Next dispatch uses the same verified callable catalog policy (`gpt-6-luna`, maximum `max`, context-free native workers; observed backend settings remain unexposed). Base is `89ef0e3907a39a6152c883343ccf3721a9e08850` plus the primary-owned dirty wave above. Primary consumes both handoffs, owns shared manifests/config/startup/API wiring and final review, and keeps the current notification-trigger owner exclusive to `hass-automation`.

| Task | Exclusive scope | Outcome and focused acceptance | Lifecycle |
| --- | --- | --- | --- |
| zone_native_v5 | new `crates/hass-zone/**` | Pinned YAML/home/storage zones, real geometry, person occupancy, CRUD/restart and joined cleanup; crate tests against pinned boundary/selection expectations | accepted/released after owned-ID/collision corrections; 27 scoped cases and strict lint; integrated checks below |
| helpers_parity_v5 | `crates/hass/src/helpers.rs` only | Correct pinned restore, attributes and helper services for existing five domains, with regression fixtures; preserve caller interface and generic service ownership | accepted/released; ten scoped cases and strict lint; primary retains precision correction and final checks |

Zone work is internal state/location computation only. It must not access actual trackers, devices or external providers. The worker establishes a source-backed public interface before primary integration. Helper work must not reintroduce the removed helper-only `homeassistant.*` shim. Neither worker may alter unrelated scopes, hand-edit Cargo.lock, spawn children or perform Git publication.

Notification trigger owner completed 19 scoped tests, formatting and strict lint; primary compiled-process acceptance passed the actual HTTP notification → private trigger payload → templated helper service path. Same owner remains in intentional correction for a separately source-confirmed lifecycle gap: startup triggers currently synthesize a start immediately when the engine attaches. Pinned `homeassistant/triggers/homeassistant.py` listens once to `homeassistant_started`; root already emits that event after providers are ready. Correction owns only `hass-automation`, removes the synthetic initial run, and tests no premature/replayed/duplicate start while retaining shutdown-job behavior and notification/queue regressions.

Primary additionally owns `hass-config` core-update publication and root zone adapters. Pinned `core_config.py::async_update` persists before publishing `core_config_updated` with the requested fields. The native manager previously omitted that event; it now serializes updates, emits only accepted request fields after validation/persistence, and rejects unsupported fields without mutation/event. All nine config-manager fixtures pass, including the extended event/payload assertion. Zone root wiring consumes typed `update_home`/`reload`; the crate must not register a placeholder reload that reuses stale startup YAML.

Automation lifecycle correction now passes 21 scoped tests, formatting and strict lint. A bounded independent read-only review (`automation_signal_review_v5`, same worker policy, owns no writes) checks private-signal privacy/causal loop protection and startup first-receipt semantics at the corrected working-tree source. Consumer is primary acceptance; release on evidence-backed findings. Primary retains final judgment and integration checks.

Next queued domain is `person_native_v6`, exclusive new `crates/hass-person/**`, with the same resolved worker policy and base source. Its consumer is primary app/API integration after the current group/notification/location/helper checkpoint. Outcome is pinned YAML/storage person entities driven by actual tracker states, correct selection/restoration/zone membership and collection CRUD with lifecycle cleanup. It must use `person/entity.py` and listed tests, emit source-backed collection signals, and leave user validation/reload/transport authority explicitly contracted. Root will include the new package only after the current checkpoint is committed; no incomplete person source enters that checkpoint. Release follows scoped checks and source-backed handoff.

Current review receipts: independent automation review found no concrete signal/lineage/startup mismatch after comparison to the exact pinned object paths. Zone scoped checks passed 24 cases and strict lint; primary real WebSocket and compiled-process fixtures pass CRUD/authority/subscription, include reload, occupancy and saved home updates. Primary caught a stale-zone cache across changed-YAML restart; the zone owner is correcting owned-ID cleanup before final acceptance. Helper owner completed ten focused cases and strict lint. Primary review rejected fixed fifteen-decimal number formatting because pinned `number/entity.py` exposes native values unchanged without unit conversion; shortest round-trip formatting and a nonzero `1e-16` regression replace it. Final helper rerun waits for the active zone correction to compile.

Owned-ID correction is accepted after source review and 26 zone fixtures: metadata stores only IDs for stale-state cleanup alongside editable zone records; changed/removed YAML definitions come solely from current configuration. Foreign unreceipted states survive. Ordinary state-write failures attempt compensating state/metadata restore; these per-state writes do not form a multi-state database transaction or erase already-observed events. Primary host fixtures also pass internal reload authorization, component-only validation and same-process writer-lease release with retained manager handles. Independent automation reviewer is accepted/released after exact pinned comparison; no additional tests were claimed by that review.

Final checkpoint validation passed **304 tests**, workspace formatting, strict full-workspace Clippy, optimized build and optimized example `--check`. The 17-crate checkpoint includes 27 zone cases and the final helper precision correction. Optimized executable SHA-256: `1dabe8a70a716bc2df0d93f0d44a28a4eff78426633ccc4361391ebdd2dd432c`. Publication follows; `person_native_v6` remains the only running implementation owner, outside the workspace/checkpoint until its next integration gate. Full rewrite work continues after this checkpoint.

Published current checkpoint: `7e093fc2bdc5f0a521f932a46f65f9df9a828b5d`, pushed to the fork's `rust-rewrite` branch. Linux verification is pending. Person's integration gate is now open: root adds `hass-person` to the next dirty workspace and retains app/API/config/auth bridge ownership. No current checkpoint test or CI receipt includes those subsequent person changes.

Next parallel provider task `tracker_native_v6` owns only new `crates/hass-device-tracker/**`, uses the same resolved native Luna/max policy and published base above, and feeds the primary/person location pipeline. Outcome is actual native tracker ingestion/service/state/expiry and zone computation from pinned `device_tracker` source, with typed startup and joined cleanup, metadata restore and local Runtime fixtures. No scanner, vehicle, phone, network or hardware is contacted. Root owns shared manifests, configuration, registry and authorization wiring. Its source-derived interface and service authority must be agreed before adapters; release on scoped checks and a concise source-backed handoff.

Linux run [42180581797](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42180581797) passed every check for published `7e093fc2bdc5f0a521f932a46f65f9df9a828b5d`. Current person/tracker/auth-bridge changes are a separate dirty wave. Root auth bridge now emits pinned `user_removed`/`user_updated` after successful writes and provides a narrow active-user display-name method; its focused permission/persistence/event/token-revocation fixture passes. Full next-wave validation remains pending integration.

Person source review caught unique-ID/state-ID conflation before acceptance: YAML `id` is the unique identity, while `person.<object>` derives from the name and registry. Root supplies the shared native registry through an entity resolver, preserving current unique ID, generated/renamed entity ID, disable state and collision behavior. The person owner retains its crate; root owns the callback and app/API adapters. Source provider-ready transition listens to `homeassistant_start`, before the existing `homeassistant_started` automation event.

Person owner is accepted/released after 30 scoped fixtures, formatting and strict lint. Tracker owner reports 25 focused cases and strict lint; final handoff is pending. Primary real WebSocket fixture passes list/storage/config, ordinary-user mutation denial, caller-only profile naming/picture clearing, private feeds and actual account-deletion unlinking. The compiled process passes `device_tracker.see` → person location → zone occupancy, including hard restart and stable registry identity. Root configuration fixtures pass falsy/singleton person forms, duplicate unique-ID preflight, and secret-aware read-only known-device import. Root person host also releases the writer lease after shutdown with retained handles.

Next dependency task `image_upload_native_v7` owns only new `crates/hass-image-upload/**`, using the same resolved native Luna/max policy and current published code base `7e093fc2bdc5f0a521f932a46f65f9df9a828b5d` (documentation head `d5d77279`). Consumer is primary HTTP/WebSocket/person-picture integration after the 19-crate checkpoint. It ports actual image validation/storage/thumbnail/collection behavior from pinned `image_upload` source; all IO stays under a caller-provided fixture/app data directory. Root owns manifests, multipart transport/auth, startup and picture URL validation. No incomplete image source enters the person/tracker checkpoint; workspace inclusion waits until that checkpoint is committed. Release follows scoped source-backed tests/lint and a concise handoff.

Current profile transport accepts real name edits and picture clearing, while non-null picture uploads remain explicitly unavailable until the image collection is integrated. It does not accept arbitrary references as uploaded images. Person user removal follows the pinned normal update schema, which resets omitted tracker lists; the dedicated own-profile path explicitly preserves the current trackers.

Tracker acceptance followup retains its existing owner for pinned Gravatar mapping: the MD5 library is already locked transitively as `md-5 0.10.6`; root exposes that exact version rather than omitting the upstream field to avoid a new package. URL generation hashes lowercased UTF-8 email bytes locally and performs no external request. This followup remains outside the previously green checkpoint and must pass scoped checks before freezing the next wave.

Tracker owner is accepted/released after the followup: 26 scoped tests, formatting and strict lint pass; Gravatar wins over the picture fallback exactly as pinned and hashes ASCII-lowercased UTF-8 bytes locally. Root app/API strict lints pass and actual tracker/person/zone, own-profile authority and host-lease fixtures pass. The current 19-crate source is frozen for complete validation; only the excluded image-upload crate remains an active implementation owner. Inventory reconciles 28 partial native domains and 1,537 unported, with zero complete; full rewrite work continues.

Upcoming image transport research: Context7 lookup failed with `TypeError: fetch failed`; root used the official locked Axum 0.8.9 [Multipart documentation](https://docs.rs/axum/0.8.9/axum/extract/struct.Multipart.html) and [per-route body limit documentation](https://docs.rs/axum/0.8.9/axum/extract/struct.DefaultBodyLimit.html). Multipart requires its Cargo feature, must be the last body extractor, and needs an upload-specific limit while other API body limits remain in force. The image owner corrected its initial auto-orientation assumption against exact `image 0.25.10` source: decode requires explicit orientation application. Source MIME permits only JPEG/PNG/GIF; thumbnail behavior contains without cropping or upscaling. These are preparation findings, not a claim that the image component is integrated.

Final 19-crate checkpoint validation passes **366 tests**, no failures/ignored cases, all workspace formatting, strict all-target Clippy, optimized build and optimized CLI example validation. Executable SHA-256: `22fcac39e7f83aa7d2bc14b5445b2b6e49ffa52c244ccad4e9e7971f39124839`. Person/tracker owners are accepted/released; the only active implementation owner is image upload, which remains outside this checkpoint until publication opens its integration gate.

The 19-crate checkpoint is published as `76aa09afc13e4a1f36051e9f4e1107d2db6d807b` and passed [Linux run 42189652679](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42189652679). Image integration's gate is open in the subsequent dirty 20-crate tree. The image owner completed 25 codec/file/metadata/order/lifecycle fixtures, formatting and strict lint and is accepted/released. Primary now owns image source and adapters, adding validated prepared files for streamed HEAD/range/modified-date responses and source-style ETag preconditions. Actual HTTP/WebSocket acceptance passes authenticated upload above 1 MiB, public original/thumbnail, readonly-user profile binding, private image feeds, admin mutation, invalid MIME/extra-field rollback, oversized upload rejection and deletion. A compiled-process fixture verifies original data and metadata through hard restart and no thumbnail upscaling. Complete 20-crate validation is pending.

## Entity lifecycle and webhook continuation

New tasks use the same live-catalog-resolved `gpt-6-luna`/`max` native policy, context-free dispatch and unexposed backend/tier caveat. Base is published `76aa09afc13e4a1f36051e9f4e1107d2db6d807b` plus primary-owned image changes. They stay outside the 20-crate checkpoint until publication; root owns shared manifests and existing module declarations.

| Task | Exclusive scope | Consumer and focused acceptance | Lifecycle |
| --- | --- | --- | --- |
| entity_lifecycle_v8 | new `hass-entity/**`, new registry `provider.rs` only | Root adoption by reactive template sensors; stable identity, provider-info updates without clobbering user overrides, rename/disable/remove/restore and weak lifecycle cleanup tested against pinned entity/platform behavior | accepted/released after corrections, 19 lifecycle and 20 registry cases plus primary consumer checks |
| webhook_native_v8 | new `hass-webhook/**`, new API `webhooks.rs` and own protocol fixture; later granted automation scope | Root router/automation integration; actual inbound callback dispatch with registered methods/locality, secret-safe admin inventory, normalized body/query and no outbound traffic | accepted/released; actual process and source-authority correction checked by primary |

Entity scope begins with the reactive lifecycle used by existing template providers; unused polling/action abstractions are deferred until an actual caller needs them. Webhook callbacks are in-process function registrations and must not be reported as restored config entries. Neither task may mutate a shared existing file or create a second registry cache. Primary integrates source before counting either as acceptance.

Primary static-serving verification found that current Context7 examples include ETag support absent from locked `tower-http 0.6.11`. Inspection of the exact installed source and a failing actual HTTP fixture established the gap. The API now implements file-derived ETags and conditional-header ordering around the locked streaming responder; the corrected fixture passes matching/weak validators, failed If-Match, invalid range precedence, HEAD and ranged byte reads. No receipt relies on the newer library's behavior.

The frozen 20-crate image checkpoint passes **393 tests** across 52 targets, zero failed/ignored, formatting, strict locked all-target Clippy, optimized build and optimized CLI configuration check. Optimized macOS executable SHA-256: `d4943948c5626d3becda0026689db9a9eef6d897e4a58222d4b6296d9164ae48`. Inventory reconciliation accepts image upload as partial: 29 partial, 1,536 unported, zero complete across all 1,565 manifests and 32 disjoint batches. Publication follows; new entity/webhook workers remain outside this checkpoint.

Image checkpoint is committed and pushed as `33a67c42d782debc54046f32069b05edaf325d98`; [Linux run 42202324831](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42202324831) is in progress. Next-wave gates are open. Webhook owner's scope now additionally includes `hass-automation/**` for native trigger schema, callback registration, execution and cleanup; root retains API/app/shared manifests. Entity owner retains only its new crate and registry provider module; template consumer migration is root-owned.

Running follow-on `/root/system_clock_sensors_v8` uses the same resolved native Luna/max policy and published base `33a67c42`. Exclusive scope is new `crates/hass-system-sensors/**`; consumer is root config-entry/app integration through the shared entity lifecycle after the current two ports. It implements pinned `time_date` and `uptime` behavior from the actual host/runtime clock, with deterministic clock/timezone/DST fixtures, stable identity and joined cleanup. It must not introduce a parallel registry cache or fabricate a boot time. External devices/traffic and shared file writes are excluded. Release follows source-backed handoff and focused checks; none of its source enters the prior checkpoint.

The published 20-crate checkpoint passed [Linux run 42202324831](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42202324831), including all tests and CLI validation. Its acceptance is frozen at `33a67c42`; current dirty entity/webhook/clock source must earn separate receipts.

Root template-consumer regression reproduces the current rename defect: after registry rename, the provider never writes the new ID. It fails at the expected state-wait assertion before lifecycle migration. Additional root acceptance covers user display overrides, disable/re-enable/remove without ghost recreation, renamed action targeting and disabled-at-setup exclusion; these remain pending until the shared lifecycle is adopted. The clock owner confirmed pinned uptime captures `dt_util.utcnow()` in its entity constructor, once per setup; it is neither an elapsed duration nor OS boot time. Pinned time/date accepts seven option types and rejects stale `beat` configuration.

Entity implementation owner is accepted/released after 9 lifecycle and 19 registry cases plus strict lint/formatting. Primary's actual template consumer now passes all 13 cases, including the three new lifecycle/service regressions. A first integration run hung; a sample of the directly launched fixture binary proved `register_batch` called `EntityHandle::status` while holding the same records mutex. The owner corrected it using precomputed status; the accepted run terminates. Durable registry identity receipts replace unsafe platform-wide stale cleanup and isolate config-entry namespaces. Exact source removal/unload evidence is `helpers/entity.py:1516-1547` and `entity_platform.py:1219-1235` at the pinned revision. No full-workspace acceptance is claimed yet.

Queued independent read-only `/root/entity_lifecycle_review_v8` uses the same native Luna/max policy, base `33a67c42` plus current frozen entity/provider source. Scope is lifecycle/provider receipt and ownership failure modes only: failed metadata/registry writes, namespace isolation, foreign-state replacement, removed/disabled publication and shutdown/lease release. Consumer is primary acceptance before publication; no writes, no duplicated full-workspace runs. Release follows a concise finding/evidence handoff.

The clock source is ready for runtime integration, so the current open wave may include its 23rd crate rather than leave it idle behind an artificial checkpoint. Its owner's scope extends to new app `system_sensors.rs` only for actual time_date/uptime config-entry driver setup/unload/flow and YAML host. Root retains shared configuration/main/manifests and the device registry SERVICE-type extension. This does not change the frozen 20-crate receipt; clock acceptance still requires actual app/transport checks.

The independent lifecycle reviewer completed/released with three confirmed failure paths: registration receipt rollback after a registry commit could orphan ownership, read-then-remove cleanup could delete a concurrent replacement, and failed last-handle Drop cleanup could strand an active record. The entity builder is reopened in correction for those exact cases and failure-injection coverage. Root supplies an atomic `Runtime::remove_state_if_unchanged` under the existing write-order lock; review acceptance remains pending. No failed-path fix is yet included in a published checkpoint.

Webhook builder completed/released after 7 manager cases, 25 automation cases, actual API transport fixtures and strict lint. Root's compiled-process fixture exposed a separate template dependency: valid adjacent expression nodes are rejected by native typed rendering. Root retains that fixture unchanged and is extending the bounded shared renderer to handle full templates while preserving single-expression types, using pinned `helpers/template` result parsing and current MiniJinja documentation. Actual webhook process acceptance remains pending this fix.

Primary's unchanged compiled webhook fixture now passes after bounded composite rendering, including action variables, method rejection, hard restart and fresh callback registration. Shared checks pass 27 Core tests, 20 registry tests, template tests and strict Core/template lint. Pinned webhook API review corrected an over-restrictive adapter assumption: only `webhook/list` carries `require_admin` (`components/webhook/__init__.py:262-300`); `webhook/handle` accepts authenticated callers holding the callback ID. Actual read-only-user WebSocket dispatch now passes while inventory stays denied and payload remains private. Unknown inventory fields are explicitly rejected. Multipart webhook form decoding and complete callback response parity remain listed gaps.

Entity correction reports 16 scoped tests and strict lint. It tracks exact last-owned state snapshots for atomic compare-removal, retains committed/pending registry identity receipts, and makes failed last-handle release terminal/retryable rather than leaving an active unowned record. The same read-only reviewer is reopened for a narrow correction audit of the three reported paths; no new broad review or duplicate workspace run is requested. Acceptance waits for its source evidence and primary integration rerun.

Next dependency-ready port `tag_native_v9` uses the same resolved native Luna/max policy, published base `33a67c42` and the source-stable image/webhook contracts. Exclusive scope starts with new `crates/hass-tag/**`, new API `tags.rs` and its own protocol fixture. Consumer is primary tag scan/collection/QR and automation integration after the current 23-crate checkpoint; shared manifests, existing module wiring and existing automation remain root-owned until an explicit gate. Acceptance requires pinned collection persistence, scan event/state/context behavior, authority, real QR/transport and lifecycle cleanup in local fixtures. No NFC hardware, devices, deployment or external calls. Release follows a source-backed handoff; it stays outside current workspace acceptance until integrated.

Source review corrected that candidate boundary: pinned tag has no QR/HTTP scan endpoint, `tag.scan` service or `tag/scan` WebSocket command. `/root/tag_native_v9` is running and implements the native scan callback for actual provider callers plus only the source collection commands. It receives shared Registries and EntityLifecycle explicitly; names belong in the entity registry. `tag_scanned` is deliberately public and precedes collection persistence. Its future API fixture stays at `tags_protocol.rs.pending` so Cargo cannot auto-discover unintegrated tag imports during the current checkpoint.

Primary clock compiled-process acceptance passes YAML clocks, HTTP time/date and uptime flows, actual setup-time bounds, singleton rejection, hard restart with stable entry IDs and a fresh uptime timestamp, and deleting one entry while preserving others. Clock owner is finishing shared platform namespace/scan-interval schema and weak closed driver lifecycle before freeze. Core now also supplies atomic conditional state creation/update; 28 Core cases pass, including mismatched/newer observation and failed-write preservation. Entity correction audit resolved original receipt orphan and compare-delete races; it found last-good leases detached on a failed reconcile and missing automatic Drop cleanup retry. The owner is correcting those paths and using conditional writes for rename destinations and unavailable transitions.

Final entity correction is accepted/released after 19 entity and 20 registry cases, strict lint and formatting. Primary reviewed the replacement rollback, scoped commit evidence, guarded rename destination and paced pending-drop retry paths. Last-good leases survive precommit replacement failure; pending cleanup is canceled on identity reentry and persistent errors surface at shutdown. Primary fresh integration passes all 13 template cases, all 10 compiled-process cases and the actual webhook protocol fixture. The process suite includes namespaced YAML sensor creation, removal across changed-YAML hard restart, callback re-registration, actual clocks/config flows and SIGTERM cleanup. Current 23-crate full validation remains pending clock-owner freeze.

Primary host review found that synchronous operation-lock acquisition in async shutdown could deadlock against a blocking callback awaiting Tokio task cleanup. Clock owner moved close/Drop to an atomic gate and waits for operations on a blocking thread, with a concurrent current-thread runtime fixture. Partial startup also left a dead driver registration that prevented retry; root added exact-Arc `ConfigEntries::unregister_driver`, verified by all 10 config tests, and the host now owns rollback/unregister guards. Driver-domain conflicts must be resolved before YAML reconciliation can touch an existing provider. These corrections are pending the final clock handoff/full workspace gate.

Clock owner completed/released with 9 host cases, strict app lint and formatting. Primary added best-effort Drop cancellation and exact-driver unregistration so a retained manager can start another host; explicit async shutdown still unloads entries and joins work. Fresh native checks pass 13 system-sensor cases and all 10 host cases. Current 23-crate source is frozen for formatting, strict workspace lint, tests and optimized build; tag remains excluded. The inventory reconciles 32 partial native domains, 1,533 unported and zero complete across all manifests; this is partial acceptance only.

Queued read-only `recorder_attribute_policy_v9` uses the same native Luna/max policy and published base `33a67c42` plus the frozen 23-crate tree. Consumer is root's next tag integration: determine the exact pinned `_unrecorded_attributes` path and minimal native live-state/history/event separation, preserving tag_id in live state and deliberately public tag_scanned events. No edits or broad test runs; deliver source anchors, proposed bounded API and focused failure/restart fixtures. Tag additionally needs typed `home_assistant_error` mapping for duplicate IDs. Neither change enters the frozen checkpoint.

Complete 23-crate validation passes **462 tests** across 59 targets, zero failed/ignored, formatting, strict locked all-target Clippy, optimized build and optimized CLI validation. Optimized executable SHA-256: `d27d71f52a0777f56c713462152132417540b979f85eda66c2f288c9ee20f430`. Tag owner completed/released its new-path draft with 9 isolated tests; it remains unaccepted until native workspace/API/automation/recorder integration. Publication of the frozen checkpoint follows. Generated nested Cargo target caches are ignored without deleting them; they are not deliverables.

The 23-crate checkpoint is committed and pushed as `831dce75694a2bdba0777aaeef985614895eacae`. Tag's integration gate is now open in a separate 24-crate wave. The existing tag owner is intentionally reopened with exclusive `hass-automation/**` source/test/README scope for source tag triggers, including real native scan-to-action acceptance; shared manifests and every other existing file stay root-owned. Native Luna/max policy and external-action limits continue. Release follows scoped source-backed checks; no new tag changes belong to the published 23-crate receipt.

Recorder research completed/released, source-only with no tests or edits. Pinned `Entity` passes exclusions separately from live attributes; Recorder filters state-history attribute JSON, not public state or tag_scanned payloads. Native SQLite currently has separate full current/history rows and additionally records state_changed events, unlike HA's generic event table. Root will preserve the native full current/event contract while adding filtered state history, explicitly retaining that event-table parity gap. No removal of tag_id from live/public data is authorized by this policy.

Recorder owner is reopened in implementation: exclusive Core `lib.rs`, `hass-storage/**`, and `hass-entity/**` for typed per-entity history exclusions, full current/live events and atomic filtered history writes. Root has already added typed `Error::HomeAssistant` and its API mapping; the owner preserves that change. Root retains tag crate, app/API and shared manifests. The public builder contract is `EntitySpec::with_unrecorded_attributes`; concrete Memory/SQLite stores must honor the projection and existing failure-injection paths must still execute. Consumer is root's actual tag pipeline; focused live/history/reopen/failure fixtures gate acceptance. Same native Luna/max policy; release on source-backed tests and handoff.

Queued `registry_composite_v9` uses the same native policy and base `831dce75` plus the dirty 24-crate manifests. Exclusive scope is `hass-registry/**` except provider.rs (unchanged), focused on pinned persisted split/composite device mappings and list/read APIs required by legacy tag triggers. It must use the one shared registry, preserve existing identities/overrides, and provide source-backed setup/migration/split lookup tests rather than fake mappings. Root owns app/API/manifests; the tag trigger owner is its consumer. No network/devices/Git or children. Release follows focused checks and integration contract.

Published 23-crate head `831dce75694a2bdba0777aaeef985614895eacae` passed [Linux run 42270320657](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42270320657). Root opened the separate 24-crate tag workspace and its initial all-target compile passes. Typed `Error::HomeAssistant` now maps to the pinned WebSocket code, and tag duplicate create uses it. Recorder owner is implementing the typed history projection; registry owner `/root/registry_composite_v9` is running; automation owner receives the shared registry through a new optional features environment. These dirty changes are not part of that CI receipt.

Recorder implementation owner completed/released after Core 30, entity 20, SQLite migration 1 and SQLite integration 27 cases plus strict lint/formatting. The additive Store hook preserves legacy failure forwarding; concrete Memory/SQLite stores project history while keeping current/public/native-event rows full. Primary wired the actual tag entity policy and its real SQLite/reopen fixture passes. Tag API fixture is now active and passes current/live feeds, ordinary-read/admin-CRUD, exact duplicate/empty-ID errors, valid timestamps, unsubscribe and the absence of a scan command. Its first run exposed non-increasing fixture command IDs; root corrected the fixture to the already-enforced protocol. Primary also fixed registry-removed tags being revived by unrelated collection changes, with all 10 tag cases passing.

Registry composite owner completed/released after 25 cases and strict lint/formatting, including real v1→v2 split migration, persistence, refs and failure atomicity. Source list_composite_splits is ordinary-readable. Root added typed NotAllowed to preserve composite rejection code; owner corrected it. Tag automation owner completed/released after 27 cases and strict lint/formatting. Root now passes the actual shared registry and webhook manager through AutomationFeatures.

Queued read-only `registry_migration_review_v9` uses the same native Luna/max policy and base `831dce75` plus current 24-crate source. Scope is the new registry v1→v2 migration/split and reference/identity/persistence failure risks, not the unchanged provider module or full API. Consumer is primary acceptance before publishing destructive migration code. No writes or duplicated broad tests; release after concrete source-backed findings and limits. Root owns all corrections and transport/process verification.

Primary actual WebSocket acceptance also passes pinned scalar-to-string conversion for create IDs, descriptions and device IDs, strict names/update IDs and failed-update preservation. The new compiled-process tag fixture initially exposed orphaned restored state after collection deletion: reconciliation detached the lease before removing its registry entry. Root moved registry removal and owned-state deactivation before provider detachment. All 10 tag unit cases, the actual tag/SQLite reopen case and the compiled-process CRUD/trigger/hard-restart/delete fixture now pass. Public event injection drives the trigger without falsely updating the native scanner's collection timestamp. Full 24-crate verification waits for the migration corrections.

Independent tag failure review completed/released with three concrete rollback findings. Root corrections remove only newly created identities on failed create, restore the raw prior name on failed update, and compensate failed deletion with the exact unchanged registry tombstone before restoring the lease/state. The new provider-only restore method refuses stale tombstones and conflicting newer identities and never writes Runtime state. New fault fixtures cover failed name/collection/publication/removal writes, prior registry preservation, no aborted private deltas, exact stable identity recovery and subsequent successful retry. All 14 tag unit cases plus the real SQLite case pass; the guarded provider restore regression passes.

Composite owner completed/released correction after 28 registry cases and strict lint/formatting: deleted tombstones split per owner, copied identifiers replace once, self/dangling parents normalize after all splits, and aliases use config entry then integration domain then any split. Main now supplies the actual shared ConfigEntries resolver at registry construction. Explicit runtime split event order is remove, replacement creation, reference updates; startup migration emits none. Primary also corrected tag trigger EnsureList/cv.string coercion and explicit-null device filters, verified through real native scans. Null→empty-list behavior is confirmed in the [primary Probatio reference](https://probatio.frenck.dev/reference/); pinned validator version is 0.13.0. Current partial inventory is 33/1,532/0, with 1,565 domains and 32 batches reconciled by the inventory owner, now released.

Queued read-only `tag_registry_correction_audit_v9`, explicit native Luna/max and unexposed backend/tier. Consumer is acceptance of the corrected seven failure/migration paths, pinned to the current 24-crate source after root and builder corrections. It owns no edits or test reruns; primary owns final checks and any fixes. The exact provider compensation, tag failure guards, resolver callback and composite migration regressions are the bounded audit scope. Release on concrete correction findings or acceptance limits. Logger and Sun new crates remain outside this gate.

Correction audit completed/released with two further concrete findings. Root now retains each migrated owner's multi-subentry vector instead of discarding additional valid associations, in both live devices and tombstones. Failed create snapshots and restores a prior tombstone, including its original IDs/name, while preserving a state occupying the old entity ID. Registry name compensation now uses an atomic applied-value comparison, so a concurrent user edit wins. Focused checks pass 15 tag cases plus SQLite and all 30 registry cases, including provider compensation/compare guards. The first full run passed strict lint but exposed two existing IO failures: parent-side port reservation raced concurrent fixtures, and early auth rejection could reset a large image upload before delivering 401. Process fixtures now use the child's atomically bound port 0; rejected uploads drain a bounded stream without buffering. Both affected real-IO suites pass repeated checks. The corrected full gate is running.

Next-wave logger implementation owner completed/released with 8 isolated tests against actual formatted output and persistence; native Sun math owner completed/released with 10 isolated astronomical tests, exact Astral 2.2 sdist hash and Apache attribution. They remain outside the 24-crate validation. The Sun owner reopens exclusively `crates/hass-sun/**` for the native entity/config-entry runtime host and solar condition helpers, consuming its accepted math API and `docs/SUN-PORT.md`; root owns app/API/automation/shared manifests and any shared entity default-disabled extension. Same explicit native Luna/max selection, backend/tier unexposed. No physical providers, credentials or deployment; release on focused native host/lifecycle handoff before primary integration.

Corrected frozen 24-crate acceptance is green: **499 tests across 63 targets**, zero failed/ignored, formatting, strict locked all-target Clippy, optimized build and optimized CLI validation. Executable SHA-256 `1efc479a1bba8d8d07280c23f5c026eaacb92c2a45ad710b4bdfd6605888ad2f`. Primary accepts the scoped tag/recorder/composite corrections with explicit remaining parity gaps, publishes this source next, then opens logger/Sun workspace integration. The full rewrite remains active; no production deployment occurred.

Published 24-crate revision is `c1318a838c9ffeaa15ef0e57785861273c841adc`; exact [Linux run 42298490640](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42298490640) is running. Root opened the 25-crate logger workspace and all 8 logger cases pass there. The shared entity builder `with_enabled_by_default(false)` now disables only first creation and preserves user decisions on refresh/restore, with a passing focused regression; it belongs to the next source wave. Logger owner reopens exclusively `crates/hass-logger/**` for source validation/event/info refinements found in consumer review: pure preflight config validation, case-insensitive YAML/services with strict WebSocket levels, configured integration info membership and public logging_changed events. Root owns its actual subscriber, app configuration and API wiring. Same native Luna/max policy; release follows focused refinement checks.

Exact published 24-crate [Linux run 42298490640](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42298490640) completed successfully. Root has wired logger configuration, actual fmt-layer policy, weak-auth service authority, explicit native target catalog and WebSocket adapter into the separate 25-crate working tree; verification waits for the logger refinement handoff and real transport/process fixtures. Sun host remains independently owned in its new crate.

Logger refinement completed/released with 11 native tests. Primary actual WebSocket/HTTP acceptance passes strict fields/uppercase WS levels, ordinary reads/admin mutations, native output changes and exact regex suppression, logging_changed delivery and persisted settings. The compiled process passes five hard-start lifetimes covering permanent, once and session-only settings; all 12 process fixtures pass with the actual logger layer. Root fixed its initial weak-auth wrapper lifetime and proves internal user authority plus unregister cleanup in a host regression. Root is cleaning three strict-lint findings; source is not yet frozen.

Queued read-only `logger_policy_review_v10`, explicit native Luna/max, backend/tier unexposed. Scope actual logger filter/alias inheritance, message filtering, persisted lifetimes and host/API authorization wiring, pinned to current 25-crate source. Consumer is primary acceptance before publication. No edits, broad tests or live activity; root owns corrections and all shared integration. Release on concrete correctness findings or bounded acceptance limits.

Logger review completed/released with two findings; it inspected current Rust code but did not independently read pinned Python objects. Primary retains the pinned-source comparison responsibility. Root now assigns distinct logical native targets, including actual timer/schedule/scene and per-domain clock callsites, and tests independent effective/info levels plus catalog alias uniqueness. Startup notification failure restores previous metadata before returning, preserving a once-only override through a failed start; fault injection checks both notification positions, successful recovery and the subsequent default session. Both new correction regressions pass. The earlier 515-test full run passed before these changes; its binary/hash is superseded and will not be published as final acceptance. Sun host completed/released after 24 isolated cases and strict checks; app/automation integration is still root-owned and remains outside the 25-crate gate.

Queued read-only `sun_host_review_v11`, explicit native Luna/max, backend/tier unexposed. Consumer is primary integration of the new Sun crate after logger publication. Scope native host/config driver, actual scheduler, location changes, weak ownership/unload and pure condition/date-event boundaries against the pinned source. Read-only git show and public source reads are allowed; no mutations, broad tests or external activity. Root owns all corrections and shared integration; release on actionable evidence-backed findings or bounded review limits.

Final corrected 25-crate gate is green: **517 tests / 66 targets**, zero failed/ignored, formatting, strict locked all-target Clippy, release build and optimized example validation. Executable SHA-256 `35a81889ca1a243637136ef2bf1a7f20d4b313b6325730d9a4fe1372cc702d60`. Primary accepts the bounded logger port with explicit remaining gaps, publishes it next and then opens Sun workspace integration. Inventory counts are 34 partial / 1,531 unported / zero complete; this does not complete the full rewrite.

Published 25-crate revision `216bb1f1271e8f2cf7dd6ae4c4161cda4df34bff` passed [Linux run 42310430899](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42310430899). The first push received a transient GitHub 500; a retry succeeded. Root opened the separate 26-crate Sun workspace: all 24 Sun tests pass there. Runtime now exposes atomic validated `HomeContext { latitude, longitude, elevation, time_zone }`; ConfigEntries updates it only after persistence and restores it on startup, with focused Core/config checks. Sun review completed/released with one enabling-latency finding; root owns the Sun host correction and app wiring. The documented best-effort Drop/reuse limitation remains visible pending a primary fix or acceptance boundary.

Queued `sun_automation_v11`, explicit native Luna/max, backend/tier unexposed. Exclusive source/tests/README in `crates/hass-automation/**` and `crates/hass-script/**`, excluding manifests (root-owned). Consumer is executable Sun-triggered actions and Sun conditions in both engines. Use the native Sun library and atomic Runtime home context, actual clock/config-update scheduling and deterministic private test clocks; no public fake sunrise service/event. Root owns app/Core/config/Sun source, shared manifests, final checks and Git. Release on source-backed functional handoff with exact supported schemas and any remaining boundaries.

Sun enabling correction passes: status-transition watches refresh all ten values immediately and close the startup subscription window. The first reason-only watcher missed coalesced registry notifications; primary now tracks active/disabled transitions and verifies each fresh disabled position/rising entity publishes without waiting for a solar timer. Drop cancels/releases entry workers, and startup reactivates a loaded marker lacking a provider. A same-manager recovery test exposed synchronous reload inside async startup; restoration now runs on a blocking thread with explicit async cleanup. All 26 Sun cases pass in the actual workspace. App configuration preserves explicit `sun:`/`sun: {}` import presence, rejects other options and wires the shared host with joined shutdown. App compilation checks await the active automation/script owner completing its dependent source changes.

Published logger revision `216bb1f1271e8f2cf7dd6ae4c4161cda4df34bff` passed exact [Linux run 42310430899](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42310430899). The working tree has advanced beyond that 25-crate receipt. Root added atomic `Runtime::home_context` observer/time-zone mirroring from persisted core settings; core and config focused cases pass. The Sun compiled process passes empty user-flow creation, stable registry identity, disabled sensor enablement, nine-attribute history projection, antipodal saved location updates, hard restart and entry deletion. A second process fixture calculates a supported sunrise offset near the actual UTC clock and verifies the production timer executes real helper services with trigger metadata and causal context. These are loopback/tempdir fixtures, with no device or production actions.

Queued next-wave `system_log_plan_v12` completed the pinned contract `docs/SYSTEM-LOG-PORT.md`, then reopened exclusive `crates/hass-system-log/**` plus that contract for native implementation. `system_log_api_v13` exclusively owns API source/tests/README (manifests remain root-owned), using the manager's weak handle and RAII async backlog/raw subscription. Both use explicit native `gpt-6-luna` / `max`; backend/tier unexposed. Root owns manifests, app configuration, logging normalization and composition. Retain the actual console output layer and add filtered capture to the same subscriber; native write events carry `hass.target` and `hass.level` so dynamic logger identity and CRITICAL severity survive. No invented LLM endpoint, Python bridge or production telemetry.

Root logger normalization passes all 14 focused tests and strict package Clippy. Public `event_record` resolves native dynamic fields and genuine LogTracer original target/source fields; the common formatter retains structured output. Real LogTracer regression verifies a conflicting logger literally named `log` cannot override the dynamic logger's level/regex. Marked native critical output retains level 50, while ordinary errors remain 40. The new raw layer will share that normalization and must use the same `LoggerFilter` as console output.

Queued read-only followup `sun_host_review_v11` for the completed calculated time-event/condition path only; the active automation owner retains unfinished elevation triggers. Review targets pinned schema/polar/offset/context/rearm behavior, with no edits or broad tests. Root found the pinned singleton rounds elevation before state publication (`sun/entity.py:278-286`), so elevation triggers must consume actual rounded `sun.sun` old/new attributes; an unrounded private signal would change the source contract and is not being added.

### Active Sun correction contract

Owner `sun_automation_v11` retains exclusive source/tests/README in `crates/hass-automation/**` and `crates/hass-script/**`, excluding manifests. Root owns all shared integration/Git. Source pin remains `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The first handoff passed 35 automation cases, 8 script cases and the compiled actual-UTC sunrise action, but independent review found these required corrections before acceptance:

- Mirror pinned `NumericThresholdSelector(mode=IS)` / `EntityNumericalConditionBase` for `sun.elevation` conditions: above/below/between/outside, range `value_min`/`value_max`, entity threshold selector key `entity`, runtime degree-unit validation. Source `tests/components/sun/test_condition.py:1516-1529` has a between range. Native automation `validate_sun_threshold`/`resolve_sun_threshold` and script `validate_script_sun_threshold`/`script_sun_threshold` need correction. Recheck exact source forms; do not invent `is` as a type if absent.
- Both Sun duration parsers must accept valid `HH:MM` as well as `HH:MM:SS` and fractional seconds, preserving microseconds per pinned `helpers/config_validation.py:536-568`. Cover trigger and condition offsets with named regressions. The exposed legacy offset string must preserve microseconds and Python `timedelta` spelling for negative/day-bearing values; the previous millisecond-only signed-hours formatter changes the action template result. Typed timedelta template methods remain part of the wider template scope.
- Sun triggers retain generic optional `id` and `alias`, and expose `trigger.id`, `trigger.idx`, `trigger.alias` to actual actions. Defaults and explicit values follow pinned `helpers/trigger/__init__.py:595-600,711-721`; propagate the trigger index to calculated and elevation runs.
- Legacy Sun accepts event/offset plus generic metadata. Recheck and reject legacy `offset_type` if the source schema excludes it; the new namespaced trigger options retain their own source `offset_type` semantics. Replace the prior invalid legacy test accordingly.

Keep elevation changed/crossed triggers on actual rounded `sun.sun` old/new state attributes, including absent/unavailable and `for` cancellation cases. Reviewer found calculated polar event choices, -0.833-degree horizon, offset anchor/local date scan and core-config cache reset correct. Finish corrections, both full package test suites, strict package Clippy and formatting, then update owned READMEs with the source contract/checkpoint and release. No children, live device/provider activity or unrelated edits. Large source reads must be processed in context-mode with focused output to survive worker context resets.

### Active System Log correction contract

Owner `system_log_plan_v12` retains `crates/hass-system-log/**` and `docs/SYSTEM-LOG-PORT.md`; `system_log_api_v13` owns API source/tests/README only, excluding manifests. Root owns all integration, logger normalization, shared manifests and Git. Workspace now has 27 crates. App compilation and 66 app unit tests passed; the first compiled System Log stream/list/restart fixture passed after the API unsubscribe dispatch-order fix. Further source corrections below must pass before final acceptance; do not reuse the older two-row assertion as source parity.

- Exact config schema: `ALLOW_EXTRA` belongs to the outer domain mapping, not the nested System Log options. Retain nested `deny_unknown_fields`. Pinned `cv.positive_int` is `Coerce(int)` followed by `NonNegative`, so `max_entries: 0` is valid and retains zero rows. Implement common Python scalar integer coercions, and `cv.boolean` numeric and stripped/case-insensitive string coercions. Root pre-DB configuration regression now includes string zero and `' yes '`.
- `system_log.write` message and optional logger use pinned `cv.string`: reject null/array/object, coerce scalar numbers and booleans to Python spelling; level remains the exact lowercase enum. Empty logger is valid. The native events carry `hass.target` and `hass.level`, with real shared output and filtered capture.
- All service write levels must share the real native service caller source, matching Python's one `getattr(logger, level)(message)` callsite. Explicit `hass.file`/`hass.line` from a common actual native caller are resolved by `event_record`. Repeated service writes to the same logger across severity levels must deduplicate, preserve the first level, count every occurrence, and keep distinct message order. Root process fixture now emits critical first, then warning, expecting one count-two CRITICAL row and both messages.
- Optional public event queue overflow and persistence failures must leave an observable diagnostic and surface at shutdown; logging stays nonblocking, retained records remain captured, and the documented bounded event queue must not be described as lossless.
- Raw initial/backlog/live race handling, span details, bounded queues and last-subscriber cleanup stay source-backed. `EventRecord.spans` and `format_line(SystemTime)` share the actual console representation; capture uses the same original span name/fields. Root passes the full installed native catalog for raw integration lookup, separately from logger info's loaded catalog; an installed but unconfigured native MQTT integration is a valid filter and performs no network activity.
- Final native anti-echo acceptance: API owner `system_log_api_v13` instruments **socket I/O future polling only**, both reads and all sends/close (the common send helper includes subscription ACK/initial/raw frames), with `tracing::debug_span!("hass_raw_stream_delivery")`. Do not wrap the whole handler or service action: legitimate DEBUG writes through WebSocket must remain capturable. At TRACE, tokio-tungstenite emits records on idle read polling too, so outgoing-only instrumentation is insufficient. `LoggerFilter` deliberately retains this named context span even when the API's own logger has a higher threshold; the capture layer suppresses DEBUG/TRACE only in that transport scope and the pinned connection logger's DEBUG. Console output remains original. Root added production tracing dependency to API. Existing API protocol/full-suite checks passed before this final instrumentation; finish the scoped quiet-stream test and strict API Clippy/checks, update its README and hand off. Root process test `real_process_raw_log_stream_does_not_echo_its_own_frames` uses actual child TRACE output, expects an initial array that may contain genuine pending ACK activity, then a real warning and service ACK, and requires 1.1 seconds of quiet within four seconds. It has not yet been accepted after the I/O correction.

Independent capture reviewer `system_log_capture_review_v16` is read-only, targeting distinct dedup, backlog race, lifecycle and event failures. Inventory owner `parity_sun_logging_v15` owns only PARITY.md/upstream-parity.json and records current source gaps provisionally. Repairs investigation `repairs_plan_v14` owns only its next-port source contract. Root formatter passes 15 focused tests including actual LogTracer dynamic targets, critical versus error, and original span/event-field preservation. After correction handoffs, primary runs the complete frozen workspace gate before publication and continues the full rewrite.

Repairs source contract completed at `docs/REPAIRS-PORT.md` and is indexed by README. The same explicit native Luna/max owner reopens exclusively new `crates/hass-repairs/**` plus that contract for the issue registry and registered native fix-flow implementation. The new crate remains outside the 27-crate workspace/publication while Sun/System Log corrections close; isolated temporary compile harnesses may verify its owned source. Root owns all shared manifests, integration/API/config-entry lifecycle wiring and Git. Persist source storage 1.4 and active/session-shell identity semantics; use actual native callbacks and private update signaling, with no invented service or Python loader. Missing subentry lifecycle and fine-grained policy/frontend support remain concrete source dependencies, not fabricated completion. Freeze the public manager/flow contract early for the next API assignment, save ownership/checkpoint in its README, and release after focused source-backed tests and lint.

Final 27-crate local gate is green: **578 tests / 71 targets**, zero failed/ignored, formatting, strict locked all-target Clippy, optimized build and optimized example `--check`. macOS executable SHA-256 `16acb74232a8dd6f01cb30b97ea1b1e187923ba92640011faf63aeb0527cd485`. All 16 process cases pass; automation has 39 cases, scripts 10, Sun 26, System Log 9 and logger 16. Actual TRACE-enabled child output becomes quiet after delivery while genuine DEBUG WebSocket service output remains capturable. Root corrected source CV assumptions, common-caller cross-level dedup and anchored multiline filtering before acceptance. All inventory evidence reconciles: 1,565 domains in 32 disjoint batches, 36 partial/1,529 unported/zero complete, 228 named references and 75 evidence paths present. Sixty-three local document links resolve.

The context-mode MCP call timed out after fmt/Clippy/test logs were retained; doctor confirms MCP/database/host operation, with an unrelated available tool update left untouched. Ownership audit found no surviving compiler. Primary resumed only the optimized build through a persistent native command session and verified its zero exit, optimized CLI and binary hash. The finalized source is published next; exact source SHA and Linux CI follow. Repairs code and standalone checks remain outside this gate, and its planning document is temporarily frozen for publication. The full rewrite continues after this checkpoint; no production deployment occurred.

### Active 28-crate Repairs integration contract

The 27-crate checkpoint is published as `d58d19af5eca3fd5f1dd686ffa0d1826f6f33626` with [green exact-head Linux CI 42327563748](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42327563748). Its 578-test/71-target macOS receipt and executable hash remain immutable. Later source is a separate wave.

`repairs_plan_v14` completed/released new `crates/hass-repairs/**` with 13 standalone tests, strict Clippy and formatting. Root now owns that crate, manifests and app integration. It has been converted to workspace member 28 with normal app/API dependencies; Cargo refreshed the shared lock, but compile checks currently wait for the active config model implementation. Root will wire a strong app owner, weak API manager, actual parent/subentry resolvers and scoped lifecycle observer. Repairs uses the pinned target compatibility version `2026.12.0.dev0` for issue version semantics, independently of native package/API version `0.1.0-rust`.

The original API owner `system_log_api_v13` was interrupted after repeated context recovery selected its obsolete System Log task. Its completed code and Repairs drafts were preserved. Successor `repairs_api_v21` finished the existing Repairs transport work: two real protocol cases, strict API Clippy, formatting and scoped diff checks pass. The router now preserves authenticated read-only DELETE for the inherited Repairs abort resource while keeping create/read/submit admin/edit checks. Root owns common API wiring again. No invented issue-creation service/endpoint or LLM surface was added; the completed System Log I/O-only anti-echo scope is preserved.

`config_subentries_v17` exclusively owns `crates/hass-config/**` excluding Cargo.toml, plus new `docs/CONFIG-SUBENTRIES-PORT.md`. Implement real typed child CRUD/query/update and registered native create/reconfigure flows, preserving current APIs/data/redaction/home context. Publish pointer-owned lifecycle observations after committed parent/subentry removal and disable changes, outside manager locks, for actual Repairs cleanup and refresh. Freeze the public query/flow/observer contract early for root/API wiring; no fake subentry IDs or unrelated edits.

Primary verified event semantics against the pinned bus and Recorder: `async_fire_internal` does not mean private or unrecorded. Repairs updates are not in the match-all exclusion set (only close/state_reported); default Recorder wildcard listening records them unless user-excluded. Existing `Runtime::fire_event` with only action/domain/issue_id is appropriate. No new Core bus channel or transient-event method is needed. The native separate metadata/event-write failure edge is real and documented without inventing a non-persistence requirement.

Root is correcting the native fallback confirmation form against `repairs/issue_handler.py`: it has an empty schema, forwards issue translation placeholders and completes on submitted `{}`. The previous invented confirm-checkbox requirement is not source behavior. Source initial context/data issue-ID precedence and first-step input must also be carried into the native flow contract. Crate corrections belong to root after the former owner handoff; the API owner sends findings to root.

Flow review owner `repairs_flow_review_v18` found and corrected live-child proxy semantics: `next_flow` is metadata on an already-terminal outer create/abort result, linked only through actual typed manager metadata. Outer create deletes the issue; outer abort retains it; both remove the Repairs ID and leave child lifetime independent. Finished GET/abort is unknown flow. Source context/initial data and empty confirmation form are corrected. All 18 Repairs tests and strict Clippy pass; the scoped formatting changes and README contract are saved. The worker was interrupted after a context reset following that handoff; root owns its source again.

Config model owner `config_subentries_v17` completed/released 18 tests, formatting and scoped checks; primary strict all-target config Clippy also passes. `EntryRemoved` snapshots the original domain. The app now owns Repairs before driver setup, supplies actual parent/subentry resolvers and uses the pointer-owned RAII observer. Its two focused tests pass linked issue visibility/cleanup, actual sibling entity references, retained foreign Runtime state and same-manager owner drop/restart. The compiled-process fixture passes actual native SDK issue storage, minimal public notification and persisted event history, dismissal, empty-schema confirmation, terminal eviction and deletion across three binary starts.

Registry cleanup owner `registry_subentry_cleanup_v19` completed/released typed `clear_config_subentry(parent, child)` and `clear_config_entry(parent, domain)` with one snapshot commit, device-before-entity notifications and source tombstone orphaning. All 32 registry tests and strict Clippy/formatting pass. Runtime states are untouched. Native pending-device-move state and its ownership-update API do not exist yet; that source behavior remains open rather than being fabricated.

### Active Repairs registry correction contract

`repairs_registry_v24` implements the confirmed audit findings, exclusively `hass-repairs/src/registry.rs`, built-in constructor/subentry-resolver portions of `lib.rs`, the tiny nullable-fixability guard in `flows.rs`, and README registry checkpoint. Previous owner `repairs_registry_review_v20` was stopped after context recovery reverted its assignment to the obsolete read-only review; its partial changes are preserved for v24 to finish. Root will not edit those sections while v24 runs. Correct Python start-matching regex semantics; keep outside a refresh scope must not touch other refreshes; defer only when the latest owner has not touched the key; canceling the newest refresh must not wait for older scopes. Add exact overlapping/disjoint deferred/keep/cancel falsifiers. Default missing storage minor version to 1; built-in startup uses actual ConfigEntries child resolution; validate nonempty breaking CALVER values; restore session shell fixability as null while keeping active draft booleans. Preserve correct dismissal, unknown severity, privacy and public event behavior. Full Repairs package tests/strict Clippy/format and a concise handoff are required before primary gate.

### Active subentry transport contract

`config_subentries_api_v22` owns only new API `config_subentries.rs`, `tests/config_subentries_protocol.rs`, and `docs/CONFIG-SUBENTRIES-PORT.md`; root owns all common API files and README. Frozen handlers are post_flow/get_flow/submit_flow/delete_flow and async `handle_ws(&AppState, &Value) -> Option<CoreResult<Value>>`. Root already wired the module, flow routes and dispatch before generic config commands; admin classification already covers the exact three subentry WS commands. A path-scoped authenticated read-only DELETE exception matches the inherited resource behavior. REST create handler is `[entry_id, subentry_type]`, optional child ID selects reconfigure; extras ignored. WS list/update/delete expose only source public fields and preserve private data/unique IDs. Normalize HTTP form/abort fields to pinned view output while retaining the backend's actual flow context for typed linked-flow resolution. Run real driver callback transport tests and strict checks after wiring; no fake child IDs.

Source contract `system_health_plan_v23` completed/released `docs/SYSTEM-HEALTH-PORT.md`; README indexes it. Successor `system_health_manager_v25` owns only new `crates/hass-system-health/**` as an isolated draft, outside the 28-crate acceptance gate. It implements explicit native providers, weak API handles, concurrent callback collection, dates, deferred values and cancellation, plus loopback reachability fixtures. No Python platform discovery, frontend or fabricated runtime fields. Root owns shared manifests and future app/API integration. Both workers use explicit native Luna/max; backend/tier controls are unexposed. The full rewrite remains active; no milestone here is a full parity or deployment claim.

Core now carries typed `Error::InvalidData { schema_errors: Map<String, Value> }` so native flow callbacks can preserve source field-validation payloads. The subentry adapter owns the HTTP 400 `errors` response; ordinary HTTP/WS fallbacks remain generic. Read-only `config_lifecycle_review_v26` examines committed observer ordering, lock/reentrancy boundaries, cleanup isolation and owner teardown; it does not mutate files or overlap the active Repairs refresh implementation.

Lifecycle review completed/released. Pinned `config_entries.py:2414-2430` clears device/entity/issues before dispatching parent `REMOVED`; root corrected native parent removal to run committed cleanup observers before `config_entry_changed: removed` and tightened the ordering falsifier. Callbacks still run outside manager locks. The reviewer also confirmed the absence of a durable replay journal for post-commit registry write failures; this remains an explicit limitation, while Repairs startup prunes its own linked orphan issues. Flow authorization now preserves the pinned administrator decorator's HTTP 401 for ordinary/read-only Repairs and subentry GET/POST; the generic mutation gate defers only on those exact flow paths. Authenticated resource DELETE remains available to read-only identities.

### Current acceptance and next-wave ownership

Repairs registry owner v24 completed/released all assigned source corrections, 23 package tests, strict Clippy and formatting. Subentry API owner v22 completed its real loopback HTTP/WS/restart case and strict all-target API Clippy, including typed field-validation errors and the source HTTP 401 boundary. Explicit WebSocket close handshakes and graceful fixture shutdown release the old Runtime before reopening its store. Root now owns all 28-crate source and is running the frozen workspace gate. Inventory owner `repairs_parity_v28` writes only PARITY.md and upstream-parity.json, recording partial Repairs and actual subentry evidence without claiming exhaustive acceptance.

System Health SDK owner v25 completed/released its isolated draft with 10 tests, strict Clippy and formatting. Consumer `system_health_transport_v27` completed/released unreferenced API `src/system_health.rs` and nested `tests/drafts/system_health_protocol.rs`, with formatting only; root will enroll and run its actual protocol cases after the 28-crate checkpoint is published. Root review added a provider-panic boundary so callback/deferred panic completes generically instead of silently terminating its transport task; 11 isolated SDK tests and strict Clippy pass. The transport uses an owned bounded frame channel, cancellable joined task and weak SDK handle, so callback collection does not block socket reads and no public Runtime event listener is created. Draft System Health files remain outside the 28-crate publication and gate. All implementation/review/inventory workers use native Luna/max; backend/tier controls are unexposed. The full rewrite continues after the checkpoint.

Frozen 28-crate gate passes 617 tests across 75 targets, zero failed/ignored, formatting, strict locked workspace all-target Clippy, optimized build and optimized example CLI validation. macOS executable SHA-256 is `2fbc9762b76a6f94536953cc1f124062b9cda2e36c8c2666c2a814fc4090ac51`; `/tmp/home-assistant-native-28-{fmt,clippy,tests,release}.log` retain local output. Source publication and exact Linux CI follow. No deployment or production provider activity occurred.

### Active 29-crate System Health acceptance (2026-11-07)

Repairs/subentries source is committed and pushed as `9149318a0b6ff83056355adc7c6916dcce609cb5`; [exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42340433764) passes. The source and 28-crate executable receipt remain separate from the following candidate.

Root enrolled `hass-system-health` as member 29, using shared workspace dependencies and the root lockfile, and moved its API fixture into normal test discovery. The app retains the strong owner, supplies only actual native version/current Runtime timezone through the `homeassistant` provider, and drops the owner after the server has joined socket cleanup. API streams use four-frame bounded queues, share the 32-subscription limit, retain command authority checks and remove/join completed streams before exposing finish. Unsubscribe and socket/server exit cancel and join their collectors. No new public Runtime receiver, HTTP route or service is added.

Four real health protocol cases, two app core-provider/YAML cases and the compiled-process version/timezone/restart case pass. The process fixture initially used a nonexistent HTTP core-update route; root corrected it to the existing `config/core/update` WebSocket command. The isolated SDK's original 11 cases include native panic recovery; independent source review found that raw Tokio JoinHandle drop detaches child work. Root added `SystemHealthValue::from_task`, an abort-on-drop transfer adapter, and a new SDK task completion/cancellation case passes. Real API unsubscribe, socket close and shutdown now exercise spawned tasks and observe their drop guards. Ordinary deferred futures own and cancel any work they start; collector joining and transferred-task abort are distinct guarantees.

Pinned `cv.empty_config_schema` accepts arbitrary domain values and logs for truthy parameters; it does not reject nonempty mappings or scalar input. Native `system_health` YAML preserves that acceptance/truthiness behavior and emits a generic startup/check diagnostic without parameter values. No provider configuration is inferred. Root now owns all candidate source; frozen formatting/strict Clippy/full tests are running. Inventory owner `system_health_parity_v30` writes only PARITY.md/upstream-parity.json and will promote only System Health to partial after the gate. Next source-contract owner `ssdp_contract_v31` writes only docs/SSDP-PORT.md, with no network probes or source mutation. All workers use native Luna/max; backend/tier controls are unexposed. The full rewrite continues after this checkpoint.

Frozen 29-crate local acceptance passes: formatting, strict locked workspace
all-target Clippy, 636 tests over 78 targets with zero failed/ignored, optimized
build and optimized example CLI. macOS executable SHA-256:
`a43914e4fb97becfdcc2d3f198b135c96c32b0730b8136e4ffc5957e40f6364e`.
The optimized CLI also verifies source-accepted truthy health parameters with a
generic diagnostic, no sentinel value disclosure and no database creation.
`/tmp/home-assistant-native-29-{fmt,clippy,tests,release}.log` retain output.
Source publication and exact Linux CI follow; no deployment occurred.

Inventory patches require a unique domain/component anchor and a semantic
record-by-domain comparison against immutable HEAD. Identical unported
boilerplate can otherwise promote the wrong integration while leaving total
counts correct. Restore accidental records from HEAD, then verify only the
intended domain changes status.

Next-wave `cleanup_replay_v32` owns only `crates/hass-config/src/lib.rs` and
that crate's README. Its durable pending-removal cleanup journal is outside the
staged 29-crate candidate and must remain unstaged until separate acceptance.
The journal is queued atomically with parent/child deletion, retains minimal
typed IDs/domain rather than private entry data, and acknowledges only after
idempotent observer cleanup. Root will wire its replay method after observer
registration in the app owner. Do not use a blanket staging command while this
future writer runs. Current 29-crate receipts still explicitly retain the
absence of registry-cleanup replay in that published source.

System Health source is published as `2724b8e4a68b8b8cd29124afa0f5da42c2b51db1`;
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42342899022)
passes. Root wired the future replay call after the actual composed cleanup
observer is installed and added a real SQLite fault/reopen test for both parent
and child removals. These app edits and v32's config-model implementation are
unaccepted next-wave work, outside the published 636-test source and its hash.
The fault test requires stale registry rows to survive the injected write error,
Repairs cleanup still to run, and a fresh file-backed runtime to clear the
correct rows while preserving siblings, other parents and foreign current state.

Cleanup model owner v32 completed/released 24 package cases, strict Clippy and
formatting. Root's actual SQLite fault/reopen app case passes both parent and
child removals, and app all-target strict Clippy passes. Primary review then
reproduced a reentrant public-event bug: child cleanup removing its own parent
produced `[removed, updated]`. The new falsifier failed before correction.
Root now reads the current raw parent snapshot after callbacks, computes driver
capabilities outside the state lock and checks that snapshot under the state
lock through publication; absent/superseded parents do not emit stale updates.
All 25 config cases pass after correction. No operation lock is acquired around
the driver callback or cleanup callback. Root owns all journal/app source and
is running the frozen workspace gate before updating inventory/publication.

The cleanup candidate passes the complete local gate: 644 tests across 78
targets, zero failed/ignored, formatting, strict locked workspace all-target
Clippy, optimized build and optimized example CLI. macOS executable SHA-256:
`6b0114feba68c1aeef564c1e3a76fe494d79e41c3270e8c6e579d6c642b6897f`.
Inventory integration changes are limited to config and Repairs; all 1,565
statuses are unchanged at 38 partial, 1,527 unported and zero complete. Eight
new named evidence references resolve. Policy export/line-count snapshots are
retained. `difflib` patch generation uses `lineterm=''` and wrapped Base64 so
native patches retain exact lines without a spurious empty context line.
Published cleanup source `56986ed1fa5176b002b948d2a5e68c2b0c43b5b0` passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42345536763).

## SSDP and discovery lifecycle integration

The 30th native crate is `hass-ssdp`. Root owns shared manifests, the manager,
pinned matcher registry, app discovery dispatcher/host and durable acceptance
records. Bounded native Luna/max owners separately implement the protocol,
description cache/charset handling, UDP transport, UPnP server and API adapter.
Completed owners release their files; no second writer takes an active scope.
Backend/tier controls remain unexposed. `docs/SSDP-PORT.md` records the pinned
adapter and verified async-upnp-client 0.49.0 sdist provenance.

The SDK exposes weak discovery lookup and guarded sync/async callback APIs,
shared tracking, strict HTTP/XML description conversion, and owned work.
Native networking uses actual reusable UDP sockets, bounded observations and
joined shutdown. UPnP serves device XML, responds to search, rotates alive
records and sends byebye. API subscriptions acknowledge before cached replay,
require an administrator and use bounded event queues with explicit overflow
termination. All fixtures use synthetic packets and loopback HTTP/UDP/WS;
no production/LAN/device probe is authorized by these checks.

Configuration now persists typed discovery keys and preserves original flow
init data for matching/dismissal. Its removal observer carries only entry ID
and typed keys. Independent review found stale key snapshots and notification
loss after cleanup failure. Two red/green cases now use the actual deletion
commit snapshot and deliver the live private notification after a failed
cleanup attempt, while returning the cleanup error. The minimal cleanup
journal is unchanged and never replays discovery notifications. All 31 config
cases pass. The app queues discovery until the real startup event, limits
initialization to 20 and joins admitted synchronous callbacks at shutdown.

The candidate remains outside the earlier immutable 29-crate receipt until its
frozen workspace gate and publication complete. Native SERVER/model/version
fields describe the real Rust runtime. Frontend icons are not advertised until
their assets exist. Parser/codec edges, network preference configuration,
frontend consumers and exhaustive pinned acceptance remain open. The entire
rewrite continues after acceptance of this component.

The frozen SSDP candidate passes 717 tests across 82 targets, zero failed or
ignored, formatting, strict locked workspace all-target Clippy, optimized build
and optimized CLI validation. Its macOS binary SHA-256 is
`6eb8e640cca3e3b7315bfc2ac22e26ed84afd50e7315b257a06d238615b9c821`.
The optimized truthy-SSDP check prints a generic warning with no parameter echo
or database. Root reproduced/fixed latest-header selection after concurrent
HTTP and a full-run queue-idle accounting race. The ledger's earlier receipt
remains immutable. Inventory owner `ssdp_inventory_v46` writes only PARITY.md
and upstream-parity.json; only SSDP may gain partial status. The next source
owner writes only NETWORK-PORT.md, outside this publication. No deployment,
LAN/device or external provider activity occurred.

Published SSDP source `b424ecc6f040aebb95232b67ab8babe46bbdc807` passed the local
717-case gate. Exact Linux CI exposed a scheduling assumption in the
invalidation fixture: its waiter could see a completed replacement, which is
valid behavior. Test-only source `7de268dc2960425fa518436b5a000fc23ee0ed93`
polls both futures to their defined states before release, preserves assertions,
and passes [exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42357576357).
No production cache behavior changed. Root independently checked the inventory:
only SSDP changed status; config/API/WebSocket evidence changed, protected
records and pinned source/coverage stayed unchanged, and every new reference
resolved. The inventory remains 39 partial, 1,526 unported and zero complete.

## Network integration and next mDNS boundary

The 31st workspace crate is `hass-network`. Its isolated owner v48 and API draft
owner v49 completed; root now owns both implementations and shared wiring.
Fourteen SDK cases pass, including injected enumeration/routes, real SQLite,
atomic write failure, provider reentry and shutdown commit serialization.
Independent review v51 found no requested correctness issue. Four actual
loopback WebSocket cases pass the three source commands, roles, strict input,
stored preferences, saved core URLs and weak-owner teardown. URL reporting uses
actual core configuration; unavailable cloud data is null. Root added detailed
internal error logging while keeping client failures generic.

Main strongly owns Network and exposes a weak manager. SSDP consumes its
selected address snapshot before its own eligibility filter, with a dedicated
app falsifier showing that disabled adapters are never re-enabled. The HTTP
proxy/local-origin `NetworkPolicy` remains separate. Normal main startup reads
OS interface/route metadata; its UDP route resolver connects and reads the
chosen source without sending a datagram. Focused API/SDK fixtures use injected
providers. Owner v54 adds a separate actual-process SQLite restart case over
loopback. The current full gate passes 736 cases across 86 targets, formatting,
strict Clippy and optimized build; its final process check/publication follow.

Next mDNS implementation is explicitly separated from this gate. Contract v47
pins Python zeroconf 0.152.0. Investigation v50 verified native Rust `mdns-sd`
0.21.7 and its limits: constructors initialize interfaces before selection,
the worker handle is discarded, and incoming TXT normalization loses data.
Root chose a narrow local vendor patch instead of a new DNS engine. Owner v52
writes only `vendor/mdns-sd/**`: explicit interface snapshots/custom ports before
joins, true thread ownership, raw TXT and cache-ordered scoped addresses. The
per-family wildcard socket remains necessary; selected memberships and packet
filtering define the boundary. Explicit mode never enumerates other interfaces.
Owner v53 writes only the future Zeroconf model/matcher/metadata files and tests
them in an isolated temporary harness. Root owns future manager/app/API work.
All workers use native Luna/max; provider speed/tier controls are unexposed.
No mDNS test may touch local Bonjour port 5353 or a production interface.

Final Network local gate passes 737 tests across 87 targets, zero failed or
ignored, formatting, strict locked workspace all-target Clippy, optimized build
and optimized CLI checks. Its macOS binary SHA-256 is
`d53d9d1caa008998e671d2f9bc78616bfc0870b4a77a93d44fe752b6e158dffc`.
The actual-process case uses native providers, loopback transport and temporary
SQLite, persists adapter/URL settings, then gracefully stops/reaps and restarts
the child. API and SDK fixtures remain provider-injected. Inventory owner v55
writes only PARITY.md/upstream-parity.json and may promote only Network. Root
owns final semantic reconciliation, source publication and exact CI.

Published Network source `cc222d0f556ac06cfe203bac6877632a5c0b902b` passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42360066554).
The inventory promotes only Network: 40 partial, 1,525 unported, zero complete.
Root verified the four changed records, new test references, untouched protected
records and unchanged pin/coverage/batches. Later mDNS work is not included in
that receipt.

Zeroconf model owner v53 completed 13 pure cases and exact generated metadata
(120 types, 174 matchers, 124 domains). HomeKit classifier v57 adds the exact
75-entry model registry, source prefix/glob/case/paired behavior and a retained
HomeKit admission before a valueless-status diagnostic. Primary source audit
corrected the initial notes: special browser types are `_hap` TCP and UDP, and
HomeKit flows target the registry-mapped integration, with source `homekit`.
Manager v56 owns only the new SDK manifest/lib/manager/README and manager tests;
it uses cache-first lookup, owned ten-second resolution, weak listener guards
and synchronous flow admission. Root owns the model/glob helper visibility and
raw TXT decoder required by update consumers before address filtering.

Backend v52 completed six focused checks, isolated formatting/strict lint, raw
TXT/cache-order APIs and an explicit provided-interface constructor returning
the real thread handle. Independent reviewer v58 found no blocking patch
defect and verified archive hash/provenance; downstream struct-literal
compatibility is explicitly documented. Wire owner v59 writes only backend.rs,
its loopback test and optional BACKEND.md, with selected snapshot/custom ports,
private cache mirror, ordered scoped data, registration and joined close.
API draft owner v60 writes only its transport and nested protocol test; it must
register both listeners before ACK, then send the generated-type cache snapshot.
Root retains all shared manifest/app/API wiring, HomeKit policy, duplicate-ID
Repairs and config-flow lifecycle integration. No production mDNS traffic or
Bonjour port 5353 is part of these checks.

### Active 32-crate Zeroconf integration

Root enrolled the native SDK and vendored wire dependency, and owns shared
app/manifests/discovery wiring. The SDK passes 17 pure/model/HomeKit cases,
11 manager cases and four actual loopback wire cases, including an idle empty
selection that binds no UDP socket. API owner v64 activated six passing
WebSocket protocol fixtures. Repairs guard owner v63 added exact weak platform
registration ownership with three passing stale/current/dropped-owner cases;
the Repairs package passes 26 tests and strict Clippy.

Main owns a shared discovery queue for both protocols and one canonical UUID.
Root added three passing persistent identity/migration/concurrency cases and
five passing queue lifecycle cases. Restart owner v61 supplies the real service
and re-exec helper; fixture owner v65 verified the compiled Unix process keeps
its PID and listener, closes the old WebSocket, and retains SQLite/config state
across the actual restart, then stops and reaps normally. No test process
enables production discovery.

Config owner v66 owns only the typed legacy YAML schema and tests. Network
projection owner v67 owns only zeroconf_network.rs; its injected tests check
selected families, prefixes, routes and announcement order. Repairs host v62
owns only zeroconf_repairs.rs, with raw listener/factory guards and the real
identity/restart action. Host fixture owner v68 owns only zeroconf_tests.rs.
Independent v69 reviews interface/source semantics; v70 reviews callback,
repair and restart lifetimes. Primary integration runs the full workspace gate
after these focused checks and fixes, then reconciles the inventory before
publishing an exact-source receipt. None of this advances a domain to complete.

The host's actual packet fixture found command-queue saturation when starting
all 120 generated types against the daemon's 100-command queue. Root fixed
explicit `Again` retry with a bounded deadline and moves initial browse setup
onto the blocking pool. The three host fixtures now pass, including real
advertisement/goodbye and HomeKit/ordinary flow admission with protected-flow
preservation. Review v70 found name removal must cover both HomeKit and
ordinary Zeroconf flow sources; root fixed that and added pre-start stale-data
removal coverage. Review v69 found the empty single-stack selection must use
V6Only and remain idle; root fixed it with a focused test. Its proposed change
to announcement ordering was rejected after checking the exact pinned source
indentation: a truthy default route is prepended even when absent from the
selected address list. The mapped CPython 3.14.2 address ranges were confirmed.
The initial full gate found only a fixture expectation that incorrectly
dropped the non-global documentation IPv6 prefix; root corrected the expected
four selected addresses and is rerunning the full gate.

The fresh 32-crate full gate now passes 814 tests across 93 targets with zero
failed or ignored, all formatting, strict locked offline workspace all-target
Clippy, optimized build and optimized configuration checks. macOS binary
SHA-256: `b18e3006af3628d797c2a59b735e4f39a5c38663b4eb860986d6411a79fd94be`.
The optimized CLI accepts empty/deprecated-option configuration, rejects an
invalid legacy boolean and opens no database. Inventory v72 owns only PARITY.md
and upstream-parity.json, promoting only Zeroconf to partial. Root reconciles
its per-domain changes and publishes the exact source before inspecting CI.

The next DHCP wave stays outside this receipt. Source contract v71 completed;
engine owner v73 audits only pinned package archives/provenance and extends
DHCP-PORT.md, without importing dependency code or capturing/scanning a LAN.
SDK owner v74 owns only the future hass-dhcp crate, isolated from this workspace
gate until root separately enrolls it. Root retains app/API/provider wiring.

Published 32-crate source is `0b859507ce02a5a8d5be8da50c80285c62fe4949`.
[Its exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42366215802)
passes at that exact head. Root verified only Zeroconf's three status/evidence fields changed
in the JSON, all 61 test symbols and 76 paths resolve, original source snapshots
and other records are unchanged, and the 41 partial / 1,524 unported / zero
complete counts agree with Markdown. The vendor's original lock and manifest
are retained; only two upstream trailing-whitespace occurrences were trimmed
and documented before clean staged whitespace validation. The final optimized
build retains the recorded binary hash. Future DHCP files were not staged.

## DHCP implementation and 35-crate acceptance

The next wave enrolled three separate native owners: `hass-dhcp` for cache and
matching, `hass-dhcp-wire` for Ethernet decoding and joined passive capture,
and `hass-dhcp-discover` for cancellable PTR/neighbor scanning. Source-contract
owner v71 and engine audit v73 inspected exact pinned HA/PyPI source without
importing or executing package code. SDK v74, wire v77 and scanner v78 released
their files to root after focused checks. API v75 integrated five passing real
WebSocket cases. Host fixtures v80 add four Runtime/registry/key-replay cases;
provider owner v82 adds six actual-frame/injected-provider ownership and
anchored-scheduling cases. Every source owner has released; root owns further
integration changes. Inventory v87 owns only PARITY.md/upstream-parity.json.

Independent SDK review v79 found the optional `registered_devices` flag is
selected by presence even when false; root retains that presence, rejects null
and adds the exact custom-matcher regression (nine SDK cases pass). Review v81
found the IDNA UTS46/IDNA2003 mismatch and missing requested-address traffic
without an IPv4 header. Decoder owner v84 fixed Nameprep and Python's roundtrip
fallback, preserves ASCII case, and supports bounded IPv6 UDP extension parsing
with requested-address-only acceptance. All sixteen wire cases pass. Explicit
empty capture remains idle by native design; default input is projected from
selected Network adapters, and userspace filtering remains a documented gap.

Scanner review v83 found resolver failure caching, stable host order and the
terminating DNS root byte were missing. Root added success-conditioned caching,
resolver mtime/size and list invalidation, the independent 24-hour expiry timer,
ordered hosts and the exact 255/256-byte boundary. Nineteen scanner cases pass.
Capture callbacks enter a bounded queue before runtime cache/flow work. The
private tracker publisher carries genuine integration observations only.
Producer shutdown and queue draining complete before dropping the weak-handle
cache owner; accepted in-flight SDK callbacks are documented separately.

The complete 35-crate gate passes 874 tests across 100 targets, zero failed or
ignored, with a final 126-case app-bin run. Formatting, strict locked offline
workspace all-target Clippy, optimized build and optimized CLI checks pass.
Optimized macOS binary SHA-256 is
`3d7db1362393f27d411694fdb6c7b7c6a4a7bfa60375f2aed0591caa93e6b817`.
No live capture, real resolver, LAN scan or device traffic was exercised.
Root reconciles the inventory and publishes this source before exact Linux CI.

The 35-crate receipt is published at
`996f0e42fa7dce04cdff59ca7d0c4cd60b78cd0d`. Root independently resolved all
79 DHCP evidence paths and 60 test symbols, verified only its three JSON
status/evidence fields changed, and retained all source snapshots and other
domain records. Status counts are 42 partial / 1,523 unported / zero complete.
The first Linux run found a Linux-only `filter_next` lint. Source
`3f97a2d00c21daa1adfa8b205adb14e07244a69b` replaces it with `find` and exercises
the selector in portable fixtures; all six provider tests and strict app
Clippy pass. [Exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42374354822)
passes at that corrected head. USB files and work were excluded from both publications.

USB source-contract v86 is outside this receipt. Root corrected its speculative
key proposal: pinned USB supplies no discovery key, so the native shared bridge
will need no-key admission rather than invented persistence/reload behavior.
The USB consumer unique-ID helper is not the scanner cache key and can collide
when metadata is absent; path-specific records must remain distinct.

### Active USB implementation contracts

SDK owner v88 writes only the isolated future `crates/hass-usb` model, matcher,
provider/scanner registry and cancellable scan/debounce owner. Source and engine
contracts v86/v89 are complete in USB-PORT.md; no dependency package code was
imported or executed. Backend owner v90 writes only `crates/hass-usb-backend`,
aligning its records with the SDK and retaining enumeration/watcher resources
through joined stop. API owner v91 writes only the new USB adapter and draft
protocol fixture; these are three admin request-response commands, not streams.
Root owns all shared manifests, existing app/API wiring and usage provider.
Every USB fixture must avoid live USB/serial enumeration, opens and commands.

Root generalized shared flow admission to optional keys while preserving the
existing keyed callers. USB removal matches exact service data and only the
most-specific domains; source USB aborts even protected flows, unlike ordinary
SSDP/Zeroconf dismissal. A new trusted ConfigEntries abort helper shares the
existing matching/recheck path without changing ordinary protection. Eight
dispatcher cases pass, including two no-key/USB-removal cases; all 31 config
SDK cases pass after that refactor. USB remains unaccepted and unported in the
inventory until its own implementation, checks and receipt are complete.
