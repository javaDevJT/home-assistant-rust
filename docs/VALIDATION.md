# Validation evidence

## Accepted switch, button and awaited MQTT (48 crates)

The frozen candidate passes **1,250 tests across 140 targets**, zero failures
or ignored tests, workspace formatting and strict locked offline all-target
Clippy. The optimized workspace build passes. Thirteen owned-temporary-directory
CLI cases pass, including valid mapping durations and rejected type/string-
duration schemas for both binary-sensor and switch device triggers. No CLI
check creates a database or extra file or echoes the ignored fixture secret.
Optimized macOS binary SHA-256:
`6f755a88bc40f437ff41fc7f377e41dc098dff59552a6738aa629bc2bd443bca`.

Switch and button descriptors now run through the application's existing
shared lifecycle in MQTT. Focused checks pass 11 switch, nine button, 26
entity, 46 Core, 14 generic-control, 53 automation, 43 config, six registry
helper, three registry-host and 16 MQTT cases. Core owns concurrent service
calls through cancellation, scope propagation and shutdown, retaining completed
unexpected join failures. Generic controls preserve concurrent domain start,
first-error sibling continuation, explicit parent cancellation and contexts.

The compiled MQTT process case proves discovery, registry rename, direct and
generic commands over the actual wire, HTTP reporting of actual changed
states, entity/config-entry disable and re-enable, joined shutdown and SQLite
restart identity. The 22-case end-to-end target includes both binary-sensor and
switch device YAML trigger/condition/action paths. Three driver cases exercise
same-manager restart and canceled startup/shutdown. These use synthetic
providers, owned SQLite and numeric loopback brokers, not physical commands.

MQTT completion cases cover QoS 0 write, QoS 1 PUBACK, QoS 2 PUBCOMP, the
source-compatible ten-second soft timeout, late acknowledgments and retained
packet-ID ownership, caller cancellation, button timestamps and overlapping
presses, and offline completion without reviving availability. Registry
disablers follow source ordering around unload/setup; startup repairs entity
flags while preserving persisted device history. Light, cover, fan and unit
SDK work remain outside this workspace receipt. Published source
`2c9d9e5bca6b8b95d13c06e745640fa369d49a3a` passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42453600318):
formatting, strict all-target Clippy, full workspace tests and example CLI
validation. Root independently verified the successful head and conclusion.

## Accepted native sensor platforms (46 crates)

That frozen workspace passes **1,186 tests across 134 targets**, zero failures
or ignored tests, formatting and strict locked offline all-target Clippy. The
optimized workspace build and ten isolated CLI cases pass. The CLI accepts a
device trigger with mapping duration and rejects an unknown device type and
string duration, as required by the pinned device schema; the seven prior
general/credentials cases still pass without extra files, databases or secret
echoes. Optimized macOS binary SHA-256:
`9ff135c40fd11f90a9d1dd84dda102285aee5f704a52f2f80dbab7503f88390e`.

The new SDKs publish actual Sun, time/date and uptime entities through the shared
lifecycle. Focused evidence includes four binary SDK cases, two sensor table
plus 11 projection cases, 26 Sun cases, 15 system-sensor cases and 47 automation
cases. The reduced-feature system-sensor build passes another 11 cases.
Compiled-process coverage now includes 21 end-to-end cases, including device
YAML trigger/condition/template action and second-precision clock restoration;
the in-place WebSocket/HTTP restart case remains green. Device fixtures are
synthetic; no real sensor/device/account traffic was generated.

Source comparison corrected an erroneous contract assertion that enum options
were mandatory. Regressions preserve the actual early return for missing values,
integer/boolean state strings, 16 independent built-in float-format vectors,
all 57 native-unit rules and 13 native-unit spelling aliases. Raw provider units
remain available independently of the compatible display spelling.

Source `aade9aa33ecccce1b627359f7fd1023353bb5f49` is published and passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42438729634):
formatting, strict all-target Clippy, full tests and CLI validation. Subsequent
switch SDK and live-attribute work is outside this immutable 46-crate receipt.

## Accepted native media and services (44 crates)

The current candidate passes **1,158 tests across 129 targets**, with zero
failures or ignored tests, workspace formatting and strict locked offline
all-target Clippy. This includes 176 app-bin cases, 20 end-to-end process cases,
the compiled restart case through both WebSocket and HTTP, and the native
media-service integration case. Nine Spotify runtime cases use synthetic tokens
and numeric-loopback HTTP, covering restored/retried entries, authorization
failures, retained token fields, post-command state, held-request cancellation,
canceled startup/shutdown callers and abandoned reauth recovery. No real account
or playback-device traffic was used.

The initial full run exposed an obsolete Core response assertion and a restart
acknowledgment race after blocking-pool dispatch. Unrequested response data is
now suppressed at the request-aware Core boundary; the native restart command
finishes its admitted result before the connection closes. The updated compiled
test verifies both transports, restart in place and persisted state. The final
full run and fresh strict lint/format checks pass after these corrections.

The optimized workspace build and seven owned-temporary-directory CLI checks
pass. They accept the example, empty config, and Application Credentials
empty/null/false/ignored inputs, reject an unknown root key, create no additional
files or database and do not echo the ignored fixture secret. Optimized macOS
binary SHA-256 is
`8f8805e48b951a112eecdaf41a7fa56f0a8d5d66316d76d68b995d3e4afd829f`.
Source `90fd6b15e01c176ce2e65ceb992705843da2b089` is published and passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42431349045):
formatting, strict all-target Clippy, full workspace tests and CLI validation.
Future isolated sensor SDK work is excluded from this 44-crate receipt.

## Accepted local external OAuth foundation (42 crates)

The final workspace passes **1,070 tests across 121 targets**, with zero failed
or ignored cases, including 148 app-bin and 20 compiled-process cases.
Formatting, strict locked offline workspace all-target Clippy and the optimized
workspace build pass. The optimized macOS `hass` SHA-256 is
`3f812bd98da78ad8d115a6f44e8ab6a96d03d654f7fc7311da3c078acddb9747`.
Source `eefd57e453c74229ad6f53fb02ab9f9734f424a0` is published and passes
[exact-source Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42415440469):
formatting, strict all-target Clippy, all workspace tests and CLI validation.

New evidence includes 30 external-client cases, 39 Core library plus four public
lifecycle cases, one host unit plus 18 integration cases, eight actual HTTP
callback cases and three callback-origin cases. The compiled credentials fixture
also proves public callback errors do not echo query/secret values, other API
routes still require authentication, private SQLite records survive restart,
and SIGTERM joins owners with a subscribed socket open. Both existing config
and subentry HTTP protocols and the MQTT-backed process flow pass unchanged.

The external client separates provider tokens from Home Assistant login tokens,
binds signed state/PKCE and account revisions, coalesces refreshes, rejects
redirects, persists before publication and owns admitted jobs through shutdown.
The host stages callback proof without exchanging a code, resolves one exchange
on authenticated GET, and atomically commits private entry data before deferred
activation. Reauthentication preserves identity/options and unrelated fields.
Cancellation, shutdown, provider replacement, TTL and event-delivery failures
have gated regressions; completed host cleanup receipts are reclaimed. Legacy
synchronous child flows obey shutdown admission, and synchronous activation
callbacks run in retained blocking tasks.

Two auth request timeouts occurred during concurrent optimized compilation;
the unchanged auth target subsequently passed in 0.98 seconds, and the final
complete workspace run passed with heavy builds held. No authentication policy
or test timeout was weakened. Seven optimized CLI cases accept the example,
empty and Credentials null/empty/false/ignored configurations and reject an
unknown root key, without extra files, database creation or ignored-secret echo.

The inventory remains 45 partial / 1,520 unported / zero complete. Root resolved
all 780 partial-row evidence paths and 476 Rust symbols and verified unchanged
pinned source snapshots and other domain records. This generic foundation adds
no production OAuth provider, provider autoload, static catalog, Spotify runtime
or real-provider acceptance. The isolated Spotify wire and media-player work
remain outside the 42-crate acceptance.

## Accepted local Application Credentials source (40 crates)

The final exact-source workspace passes **998 tests across 114 targets**, with
zero failed or ignored cases, including 145 app-bin cases and 20 compiled-process
cases. Formatting, strict locked offline workspace all-target Clippy and the
optimized workspace build pass. The optimized macOS `hass` SHA-256 is
`6a517d876da5d337f80f8656e91ddb29b6a1f4f3a0604178db097c448bb17d57`.
Source `d9025ac2860539d05752990723d6b138f5c606a2` is published and passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42398882199):
formatting, strict all-target Clippy, all workspace tests and CLI validation.

New proof includes 12 credential SDK cases, five slug cases, nine loopback API
cases, one YAML-presence case and one process restart case. Storage is private
Runtime metadata; persistence precedes events and failed writes leave state
unchanged. Factory destructors release nested guards outside registry locks.
Imports protect stable IDs and effective aliases used by matching-domain entries.
The exact text-unidecode table and 252 HTML4 entity entries were independently
hash/AST checked; Unicode 16 normalization, legacy numeric-pass rollback,
surrogates, whitespace and collision suffixes have focused coverage.

API fixtures cover schema validation before authorization, exact unsubscribe
fields without losing the stream, quiet session revocation, added/removed
events, shared limits, lag and one terminal error on owner shutdown. Admin
CRUD deliberately includes client secrets for source compatibility. The catalog
is secret-free and lists actual registered providers only. A compiled process
restores synthetic private SQLite records and streaming after restart and exits
on SIGTERM with an open socket. No real credential or provider was used.

Seven optimized CLI cases accept the example and empty configurations, null,
empty, false and ignored Credentials parameters, and reject an unknown root
key. They create no database or extra files and do not echo the ignored secret.
The mDNS backend retains actual joins and port-release assertions; fixtures now
claim process-unique custom ports after a prior overlap-compatible failure.

The inventory is 45 partial / 1,520 unported / zero complete. Production OAuth
platforms, autoload, the full provider catalog, code exchange, callback flows,
refresh and real-provider acceptance remain open. The isolated next OAuth SDK
and its checks are outside this 40-crate acceptance.

## Accepted local Hardware source (39 crates)

The final workspace passes **970 tests across 110 targets**, with no failed
or ignored cases, including 144 app-bin cases and 19 compiled-process cases.
Formatting, strict locked offline workspace all-target Clippy and the
optimized workspace build pass. The optimized macOS `hass` SHA-256 is
`dfa016496a5979e7956843e3c494632d1c9ae3a12335c8b126848125d32c51c9`.

New focused evidence includes 16 Hardware SDK cases, 15 native-metrics fixture
cases, eight loopback API cases, one YAML case and one real process case.
The latter loads Hardware, returns the actual empty loaded-provider list,
and joins the idle sampler on SIGTERM with an open WebSocket. It does not
subscribe to host metrics. SDK tests cover shared/delayed sampling, canceled
read ownership, channel closure, zero-memory rejection, recovery, exact guard
replacement and interval timing. API tests cover ordinary-user access, silent
stream session revocation and terminal lag without removing another stream.

Independent metrics review exposed a decimal-rounding difference. The native
helper now agrees with Python's built-in `round(x, 1)` for 17,007 percentage,
tie and adjacent-f64 values; CPU 69/2000 becomes 3.5%, then 4 in the API, and
memory 51/2000 becomes 2.5%. No HA or dependency package was executed.

The optimized CLI validates the example and empty, empty-Hardware,
false-Hardware and ignored-parameter configs, rejects an unknown root key,
does not echo an ignored fixture secret, and creates no database or extra
files. Linux conditional metrics code passes supplementary cross-target
Clippy using the installed Rustup 1.92 and manifest MSRV override. Source
is published at `4d875e379b9a91d2adcbcec5b2e3279e55dbe48a`;
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42385397159)
passes formatting, strict all-target workspace Clippy, all tests and CLI
validation. Physical CPU/memory
collection, Supervisor providers, Windows and full upstream parity are not
verified by these fixture-only checks.

## Accepted local USB source (37 crates)

The final 37-crate workspace passes **929 tests across 105 targets**, with
zero failures or ignored cases. Its app binary passes 143 cases, including
the injected USB host bridge, joined fallback polling, shared scheduler and
actual config-entry usage projection. Formatting, strict locked offline
workspace all-target Clippy and the optimized workspace build pass.
The optimized macOS `hass` SHA-256 is
`98e4f4a2e30ce750d6b5b83805c0649d399a5768afcbe84fb54e24abf55d7da3`.

Focused USB proof includes 23 SDK cases, eight native backend fixtures on
macOS, seven loopback WebSocket cases, four app-host cases, five usage cases
and five scheduler cases. The shared discovery module's eight cases and all
31 configuration SDK cases pass, including protected USB removal and
keyless creation. Native init-data provenance survives form submissions and
keeps arbitrary JSON with source `usb` out of usage, duplicate matching and
USB removal. Separate source reviews exposed and prompted the corrected
diagnostic, lookup, symlink and shutdown behavior.

The final optimized executable validates the example plus empty, empty-USB,
false-USB and ignored-USB-parameter configurations, rejects an unknown root
key and creates no database or other extra files. Validation starts no
provider. No live USB tree, IOKit inventory, serial device, Supervisor app or
device command was exercised. Linux-only watcher code and fixtures pass a
conditional cross-target Clippy check with the installed Rustup 1.92 compiler
and manifest MSRV override; this is supplementary compile evidence, separate
from the workspace's Rust 1.99 checks and pending native Linux CI execution.
USB source `eb4f8da2e331ae6a83c036ef14eb514fd8eb3a39` is published on the fork.
[Exact-head Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42380709474)
passes formatting, strict all-target workspace Clippy, all tests including the
native temporary-tree inotify cases, and CLI validation. This receipt excludes
the subsequent Hardware work.

## Accepted local DHCP source (35 crates)

The native DHCP candidate passes 874 tests across 100 targets with zero failed
or ignored. A final app-bin run passes all 126 cases. Formatting, strict locked
offline workspace all-target Clippy and the optimized workspace build pass.
The macOS optimized `hass` binary SHA-256 is
`3d7db1362393f27d411694fdb6c7b7c6a4a7bfa60375f2aed0591caa93e6b817`.
The local receipt is published at `996f0e42fa7dce04cdff59ca7d0c4cd60b78cd0d`.
Its Linux run exposed one `filter_next` lint in a Linux-only selector.
`3f97a2d00c21daa1adfa8b205adb14e07244a69b` uses the equivalent `find` expression
and compiles/exercises that helper in portable fixtures. Its six provider cases
and strict app Clippy pass locally. [Exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42374354822)
passes at that corrected head.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline --no-fail-fast
cargo test -p hass --bin hass --locked --offline
cargo build --release --workspace --locked --offline
target/release/hass --config examples/configuration.yaml --check
```

Optimized CLI checks also accept empty configuration, ignored DHCP parameters
and `dhcp: false`, reject an unknown root key, and open no database or data
directory. Configuration validation never starts capture or scanning.

New focused proof includes nine DHCP cache/matcher cases, sixteen wire/capture
cases, nineteen scanner cases, five real WebSocket protocol cases, four
Runtime/registry/flow host cases, six provider ownership/scheduling cases and
one parameterized YAML compatibility case. Wire fixtures use actual Ethernet
IPv4/IPv6 DHCP bytes with requested-address precedence, IDNA2003 and malformed
options. Scanner fixtures include actual loopback UDP DNS with recursion
disabled, a 2,048-address bound, named-only neighbor work, failure caching,
configuration revision/24-hour expiry, deterministic host order and DNS name
bounds. No test opens live capture or probes a LAN/real resolver/device.

The app proves tracker snapshot/add-only rules, typed private connected-device
publication without public events, same-MAC registered devices, persisted
discovery-key identity and forced cache rediscovery after removal. Capture
callbacks enter a bounded serial runtime queue; startup follows the real
started event. Shutdown joins capture, drains/joins its consumer and joins
cancelled scans before the cache SDK closes. Long scans skip missed hourly
ticks rather than starting a late immediate scan.

The Linux system scan provider and explicit selected capture path are native.
macOS system scanning is explicitly unsupported; injected providers remain
portable. Kernel BPF, other link layers, custom integration loading, live
interface rebinding, frontend consumers and full upstream fixture acceptance
remain open. See [DHCP-PORT.md](DHCP-PORT.md) and [PORTING.md](PORTING.md).
Future USB work is outside this immutable receipt.

## Accepted local Zeroconf source (32 crates)

The current native Zeroconf candidate passes the complete workspace gate:
814 tests across 93 targets, zero failed and zero ignored. Formatting, strict
locked offline workspace all-target Clippy, and the optimized workspace build
pass. The optimized macOS `hass` binary SHA-256 is
`b18e3006af3628d797c2a59b735e4f39a5c38663b4eb860986d6411a79fd94be`.
Published source is `0b859507ce02a5a8d5be8da50c80285c62fe4949`.
[Exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42366215802)
passes at that exact head.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline --no-fail-fast
cargo build --release --workspace --locked --offline
target/release/hass --config examples/configuration.yaml --check
```

Optimized CLI checks also accept empty configuration and the deprecated
Zeroconf booleans, reject an invalid boolean, and create no database or data
directory. They validate configuration without enabling discovery sockets.
The initial full run found one incorrect new test expectation: Python keeps
the documentation IPv6 prefix because it is non-global. The fixture now checks
all four retained addresses, and the fresh full run passes.

Evidence includes 17 Zeroconf model/matcher/HomeKit cases, 11 manager cases,
four actual loopback wire cases, six WebSocket protocol cases, three app host
cases, ten duplicate-instance Repairs host cases and exact weak platform
registration ownership. The actual host fixtures cover the generated browser
set exceeding the bounded daemon command queue, source startup gating, typed
keys, HomeKit vendor flows and alternative-domain context, name removal across
ordinary/HomeKit flows, protected-flow preservation, UTF-8/TXT bounds, stable
shared identity, native version, bound HTTP port, and goodbye. Empty interface
selection binds no UDP socket; all packet fixtures use explicit loopback
interfaces and custom ports rather than Bonjour port 5353.

Three identity tests cover real SQLite, legacy native SSDP identity migration,
concurrent initialization and malformed/future-data preservation. Six restart
unit cases and the compiled Unix process fixture verify the actual owned
service, authorization/context, WebSocket close, re-exec with the same PID and
loopback listener, persisted state/config, and normal signal/reap afterward.
Discovery stop joins browser/resolver work before other integrations stop;
the shared daemon then sends goodbye and acknowledges shutdown before its
real thread is joined. Source-level reviews and fixes are in
[PORTING.md](PORTING.md); remaining compatibility gaps are in
[ZEROCONF-PORT.md](ZEROCONF-PORT.md). No full upstream domain is accepted as
complete, and later DHCP work is outside this receipt.

## Accepted local Network source (31 crates)

The frozen workspace passes **737 tests across 87 targets**, zero failed or
ignored, formatting, strict locked workspace all-target Clippy, optimized build
and optimized example CLI validation on 2026-11-07. Optimized macOS executable
SHA-256:
`d53d9d1caa008998e671d2f9bc78616bfc0870b4a77a93d44fe752b6e158dffc`.
The optimized empty-config check creates no database. Published source
`cc222d0f556ac06cfe203bac6877632a5c0b902b` passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42360066554).
Unaccepted mDNS/Zeroconf work is outside this immutable receipt.

Fourteen Network SDK cases use injected interface/route providers and cover
selection, unknown/duplicate names, scopes/order, MemoryStore and actual SQLite
restart, write failure, provider reentry, concurrent reload/configure and owner
deactivation serialized against commits. Independent source review found no
requested correctness issue. Four actual loopback WebSocket cases cover the
three source commands, administrator versus ordinary/read-only access, strict
fields/defaults, persisted preferences, saved core URLs, missing features and
stale weak handles. Unknown cloud data stays null; internal errors retain native
log detail while client failures remain generic.

The compiled-process case launches the actual native OS providers with a
temporary SQLite database, selects one returned adapter through the real API,
saves core URLs, closes the socket, gracefully stops and reaps the process, then
restarts and verifies the saved selection and URLs. Its only service traffic is
loopback HTTP/WebSocket. OS enumeration and route-source selection are read-only;
the UDP route resolver calls connect/getsockname without sending a datagram.
No OS adapter configuration or LAN discovery is changed. The app selection
falsifier separately proves that SSDP consumes only enabled Network addresses,
then applies its own filter, without enabling other adapters.

The native main owns Network strongly; API and protocol consumers hold weak
handles. It emits no invented Runtime event, service, periodic poll or HTTP
route. Interface-friendly-name fidelity, unavailable native IPv6 flowinfo,
hostname route targets, Supervisor reload/debounce, frontend settings and full
pinned acceptance remain open. `--check` returns before OS provider setup and
database initialization. No production deployment occurred. The complete
rewrite continues after this checkpoint.

## Accepted local SSDP and discovery source (30 crates)

The frozen workspace passes **717 tests across 82 targets**, zero failed or
ignored, formatting, strict locked workspace all-target Clippy, locked optimized
build and optimized example CLI validation on 2026-11-07. Optimized macOS
executable SHA-256:
`6eb8e640cca3e3b7315bfc2ac22e26ed84afd50e7315b257a06d238615b9c821`.
The original local source is `b424ecc6f040aebb95232b67ab8babe46bbdc807`.
Its cache-invalidation fixture assumed a spawned waiter had reached its await;
Linux CI exposed that timing assumption. Test-only correction
`7de268dc2960425fa518436b5a000fc23ee0ed93` explicitly polls the old waiter and
replacement lookup before releasing the HTTP response and passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42357576357).
It preserves the original cache assertions and runtime behavior. Later Network
integration is outside this immutable 30-crate receipt.

SSDP passes 47 unit and eight manager integration cases. Actual loopback UDP
and HTTP cover M-SEARCH/advertisement reception, device XML, response matching,
alive cadence, byebye, cache coalescing/negative caching, strict common charset
decoding, port release and shutdown. Multicast M-SEARCH is exercised on the
explicitly selected loopback interface; the wildcard listener and separate
source-bound outbound sockets have a red/green regression. Device XML also
supports HEAD with query parameters, correct content length and no body.
Typed metadata, matcher rules, discovery
order, getter privacy, guarded callbacks, async work cancellation and weak
manager teardown have focused coverage. A red/green concurrent lookup case
proves that live callbacks combine the latest device headers after HTTP returns,
while retaining device identity across updates/removal. Pinned matcher metadata
matches the upstream generated table exactly: 44 domains and 94 matchers.

Four real WebSocket cases cover administrator access, strict fields, ACK before
cached/live callbacks, source add/remove shapes, combined capacity accounting,
observable queue overflow, unsubscribe, close and shutdown. Four dispatcher
cases use actual native config callbacks for startup/dedup, entry creation and
private key association, byebye/alive ordering, 20 concurrent admissions and
joined blocking callbacks. The full run exposed an idle-accounting race; work
reservation now shares the queue-pop lock, and the corrected full suite passes.

Three host cases cover native instance identity, malformed-data preservation,
no-interface startup/shutdown and real SQLite cold reopen without retaining
the previous writer. The configuration suite has 31 passing cases, including
latest-commit key snapshots and private removal notification after a cleanup
failure. The cleanup journal remains minimal and never replays discovery
notifications. One config case checks explicit enablement and ignored-value
truthiness. The optimized CLI additionally accepts ignored truthy parameters,
prints a generic warning without the sentinel value and creates no database.

Native SERVER/model/version metadata identifies the real Rust runtime; icon
URLs are omitted until native assets exist. Parser/codec edges, configured
adapter preferences, frontend consumers and exhaustive pinned acceptance keep
SSDP partial. Tests perform no LAN discovery, device/provider command or
production deployment. The complete rewrite remains open.

## Accepted cleanup-journal source (29 crates)

The frozen workspace passes **644 tests across 78 targets**, zero failed or
ignored, formatting, strict locked workspace all-target Clippy, optimized build
and optimized example CLI validation on 2026-11-07. Optimized macOS executable
SHA-256: `6b0114feba68c1aeef564c1e3a76fe494d79e41c3270e8c6e579d6c642b6897f`.
Published source `56986ed1fa5176b002b948d2a5e68c2b0c43b5b0` also passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42345536763).
Later SSDP and discovery-key work is outside this immutable receipt.

All 25 config cases pass. Removal and a minimal cleanup record share the same
metadata commit; registered cleanup consumers run outside manager locks, and
only successful cleanup durably acknowledges its UUID. Failure/reopen, child
scope and private-data exclusion, parent coalescing, failed acknowledgement,
overlapping/reentrant replay and precommit failure have focused coverage.
Construction/registration alone do not acknowledge records. App startup installs
the actual composed registry/Repairs consumer before replay.

Three app host cases pass. The real SQLite fault case injects a registry write
failure after parent or child config deletion, verifies that Repairs cleanup
still runs, drops every old owner/writer, and reopens the file to prove startup
recovery. Sibling/other-parent registry rows and foreign current state survive.
A separate regression reproduced `[removed, updated]` during reentrant parent
removal; the corrected path checks the current parent snapshot through event
publication while keeping driver callbacks outside the state lock.

The journal retries cleanup, without backfilling public config-entry events or
making writes across documents atomic. Full pinned acceptance and the complete
rewrite remain open; no production deployment occurred.

## Accepted 29-crate System Health source

The frozen workspace passes **636 tests across 78 targets**, with zero failed or
ignored tests, formatting, strict locked workspace all-target Clippy, optimized
build and optimized example CLI validation on 2026-11-07. Optimized macOS
executable SHA-256:
`a43914e4fb97becfdcc2d3f198b135c96c32b0730b8136e4ffc5957e40f6364e`.
Published source `2724b8e4a68b8b8cd29124afa0f5da42c2b51db1` also passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42342899022).
Later cleanup-journal and SSDP work is outside this immutable receipt.

The 12-case System Health SDK suite covers concurrent five-second callback
timeouts, generic transport errors, immediate/deferred dates, internal-helper
conversion, registration replacement, weak ownership, cold-future cancellation,
transferred task completion/abort, and actual loopback HTTP reachability,
including 5xx responses and timeout. Native provider panics become generic
client failures while preserving details for native logs/internal reporting.
The four real WebSocket cases verify source event ordering, administrator,
ordinary and read-only access, strict fields, authentication, generic errors,
finished subscription removal and cancellation on unsubscribe, close and shutdown.
The cancellation cases use real spawned tasks with drop guards.

Two app cases verify native version/current timezone reporting and the source
YAML validator's acceptance/truthiness rules. The real compiled process changes
the saved timezone through `config/core/update`, obtains a fresh health report,
hard-restarts and verifies the saved value. Only native version and timezone are
reported by the core provider. The optimized CLI additionally accepts ignored
truthy health parameters, emits a generic diagnostic without a sentinel value,
and creates no database during `--check`.

Provider registration is explicit. Python platform discovery, frontend/LLM
consumers, unknown host/runtime metadata and exhaustive pinned acceptance remain
open. A transferred task is aborted on session drop; collector cancellation is
joined separately. There is no new public event-bus subscription, HTTP route or
service, production deployment or external health probe.

## Accepted 28-crate Repairs and configuration-subentry source

The frozen workspace passes **617 tests across 75 targets**, with zero failed or
ignored tests, `cargo fmt --all -- --check` and strict locked all-target Clippy
on 2026-11-06. The locked optimized build and optimized example CLI `--check`
also pass. Optimized macOS executable SHA-256:
`2fbc9762b76a6f94536953cc1f124062b9cda2e36c8c2666c2a814fc4090ac51`.
Published source `9149318a0b6ff83056355adc7c6916dcce609cb5` also passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42340433764).
The isolated System Health SDK and its nested API draft are outside this gate.

Repairs has 23 package cases covering storage migration, restored identity shells,
actual parent/child validation, scoped refresh/keep/defer/cancel ordering,
dismissal, native callbacks and independent terminal child-flow metadata.
Real HTTP/WS fixtures verify minimal public updates, field privacy, authentication,
source 401 administrator checks and inherited authenticated DELETE. The compiled
process checks temporary SQLite issue storage, public event history, dismissal,
confirmation and deletion across three hard starts. Two app host cases verify
disable visibility, parent/child cleanup, sibling and foreign-state isolation,
and observer teardown before restart.

Configuration has 18 cases, including actual generated subentry IDs, per-parent
unique IDs, persisted private data, native create/reconfigure flows and lifecycle
observer lock/reentry boundaries. The real subentry API fixture verifies role
checks, public list/title-only update/delete, typed schema errors with successful
retry, independent sibling flows and store reopen after graceful teardown. All
32 registry cases pass, including exact parent/child cleanup and tombstone rules.
Parent and child cleanup precede their removal/update announcement; observer
failures are reported after commit. There is no cross-document transaction or
durable registry-cleanup replay journal. Exhaustive pinned-source acceptance and
the full rewrite remain open; no production deployment occurred.

## Published 27-crate source receipt

Published 27-crate Sun/System Log source `d58d19af5eca3fd5f1dd686ffa0d1826f6f33626` passes **578 local tests across 71 targets**, zero failed/ignored, formatting, strict locked all-target Clippy, optimized build and optimized example CLI validation on 2026-11-06, followed by [green Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42327563748). Optimized macOS executable SHA-256: `16acb74232a8dd6f01cb30b97ea1b1e187923ba92640011faf63aeb0527cd485`. Later Repairs/config-subentry work is outside this immutable receipt.

## Accepted 27-crate Sun and System Log source

All 16 compiled-process cases pass, including real UTC sunrise-offset actions with explicit id/idx/alias, empty Sun flow setup, immediate disabled-sensor enablement, live saved observer updates, nine-attribute history projection, hard restart and removal. The native Sun package has 26 cases; automation has 39 and scripts 10. Bounded independent review corrections cover source range/entity-unit selectors, HH:MM and microsecond offsets, normalized timedelta strings and generic trigger metadata. Typed timedelta template methods and exhaustive source fixtures remain open.

System Log has nine native cases and real API/process coverage for scalar configuration/service coercions, zero retained entries, same-caller cross-level dedup preserving the first level and ordered messages, optional event loss/persistence diagnostics, anchored multiline backlog filtering and RAII subscription cleanup. The process verifies actual console/raw/list output, CRITICAL identity, exact regex suppression, generic unsubscribe, an installed but unconfigured native MQTT filter and memory-only restart. Actual TRACE-enabled child output becomes quiet after a burst; the API fixture proves a genuine DEBUG service write remains visible while socket I/O polling alone carries the raw anti-echo scope.

All 16 logger cases pass actual LogTracer target/source normalization, the literal `log` logger trap, distinct critical/error filtering, original span/event/TRACE preservation and retained transport context under independently configured logger levels. Atomic runtime home context tracks persisted observer/time-zone settings. Inventory checks retain 36 partial domains, 1,529 unported and zero complete. Full upstream parity, System Log LLM consumers and Python traceback reconstruction remain open; no production deployment or physical provider activity occurred.

The context-mode transport timed out after successful formatting, Clippy and the complete 71-target test output were retained. An ownership audit found no surviving compiler process. Primary resumed only the optimized build through a persistent command session; it exited zero, followed by a successful optimized `--check` and the hash above. This tooling interruption did not become a test failure or a second build writer.

## Published 25-crate logger source

Published 25-crate logger source `216bb1f1271e8f2cf7dd6ae4c4161cda4df34bff` passes **517 workspace tests** across 66 targets, formatting, strict locked all-target Clippy, optimized build and CLI validation on 2026-11-06, followed by [green Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42310430899). Optimized executable SHA-256: `35a81889ca1a243637136ef2bf1a7f20d4b313b6325730d9a4fe1372cc702d60`. Later Sun/System Log changes remain outside that historical receipt.

## Accepted 25-crate logger source

Actual authenticated WebSocket/HTTP controls change the filter attached to a real captured tracing layer. Fixtures verify ordinary reads, administrator mutations/services, strict WebSocket schemas, source config/service case rules, exact message filtering, public empty change events and persistent settings. The compiled process survives five hard starts with correct permanent, once-only and session lifetimes. The app retains authorization while using weak callbacks, then unregisters cleanly.

Independent Rust review found coupled aliases and Once consumption on failed startup. Corrections use distinct integration targets, including actual timer/schedule/scene/clock callsites, and restore prior settings if either startup notification fails. Regression tests prove independent effective/info levels, alias uniqueness, failure recovery and the next default session. The reviewer did not independently read pinned Python objects; primary source comparisons remain separately recorded in the execution ledger. The shared default-disabled entity builder preserves user enablement across refresh/restore, supporting subsequent Sun integration.

All 517 tests pass with zero failures/ignored cases, including 12 compiled-process cases and 12 logger unit cases. The inventory reconciles 34 partial native domains, 1,531 unported and zero complete over 1,565 manifests and 32 disjoint batches. Python-only regex syntax, distinct critical severity, remaining attribution/repairs/frontend behavior and full parity remain open. No production deployment or physical provider activity occurred.

## Published 24-crate source

Published 24-crate tag/recording/composite source `c1318a838c9ffeaa15ef0e57785861273c841adc` passes **499 workspace tests**, formatting, strict full-workspace Clippy, optimized build and CLI configuration validation on 2026-11-06 with Rust/Cargo 1.99.1 on macOS, followed by [green Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42298490640). Later logger/Sun changes are outside that receipt. Earlier checkpoints remain separately pinned below. Full Home Assistant parity remains open. No deployment to the user's Home Assistant instance was performed.

`cargo test --workspace --locked --no-fail-fast` passes 499 tests across 63 unit/integration/doc-test targets, with zero failed or ignored cases. Formatting, strict locked all-target Clippy, optimized build and optimized example `--check` pass. Optimized macOS executable SHA-256: `1efc479a1bba8d8d07280c23f5c026eaacb92c2a45ad710b4bdfd6605888ad2f`.

Tag acceptance exercises native reader scans, causal automation context, scalar/list and null filters, private current/live WebSocket subscriptions, ordinary-user reads and administrator mutations, strict fields and pinned errors. The actual compiled process verifies names, stable registry identity, HTTP current/history separation, tag-triggered automation, hard restart and deletion cleanup. Injected failures cover create/update/delete, prior live and removed identities, concurrent name edits, foreign states, aborted private deltas and retry. The actual SQLite reopen retains live `tag_id` while recorded state history excludes it; full native event rows remain deliberately unchanged.

Composite migration tests cover active and deleted owner splits, complete multi-subentry association retention, copied identifier replacement, self/dangling parent cleanup, same-entry/domain/fallback parent aliases, actual shared config-entry resolution, persistence failure and event ordering. Public WebSocket fixtures verify readable split inventory and exact `not_allowed` mutation errors.

The first broad run found a process-fixture port reservation race and a connection reset during early rejection of a large image upload. Fixtures now use the child's atomically bound port 0; authentication rejection discards a bounded upload stream without buffering before returning its response. Both affected IO targets passed repeated focused checks before the complete green rerun.

The inventory reconciles all 1,565 unique manifests over 32 disjoint batches: 33 partial domains, 1,532 unported, zero complete. Logger and Sun draft crates and their isolated checks are outside this workspace receipt. Generic platform, recorder/statistics/event routing, physical transports and the other listed gaps remain open.

## Published 23-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 462 tests across 59 unit/integration/doc-test targets, with zero failed or ignored cases. Formatting, strict locked all-target Clippy, optimized build and optimized example `--check` passed. Optimized macOS executable SHA-256: `d27d71f52a0777f56c713462152132417540b979f85eda66c2f288c9ee20f430`.

Actual template providers now use the shared entity lifecycle for identity, registry overrides, rename/disable/remove, service targeting and restore. Primary consumer cases reproduce and then verify the fixed registry-rename defect. Independent failure-path review found receipt orphaning, unsafe cleanup and dropped-lease failures; corrections preserve last-good leases on precommit failure, retain committed identity receipts, guard state writes/removals, retry dropped cleanup with backoff and surface persistent failures. Entity and registry tests include storage faults, namespace isolation, target collisions and reentry.

Actual HTTP/WebSocket and compiled-process tests verify webhook method/locality handling, JSON/form/query metadata, administrator-only inventory, read-only capability dispatch, private automation delivery, cleanup and fresh registration after hard restart. The process fixture retained its valid adjacent template expressions; the shared renderer now supports bounded composite/block output instead of weakening the fixture.

Clock acceptance covers all seven time/date formats, timezone/DST deadlines, actual clocks, namespaced YAML platforms, validated non-polling scan intervals, live timezone refresh and config-entry flows. Uptime records a fixed setup timestamp, uses a registry service device, rejects duplicate entries and gets a fresh timestamp after restart. Host fixtures cover failed startup retry, duplicate-host preservation, exact-driver cleanup and current-thread concurrent shutdown. All ten compiled-process cases pass, including changed-YAML stale cleanup and SIGTERM with an open WebSocket.

The inventory reconciles all 1,565 unique manifests over 32 disjoint batches: 32 partial domains, 1,533 unported, zero complete. Tag source and its pending fixture are excluded from this checkpoint. Full platform/coordinator semantics, translation, multipart webhook fields, complete callback serialization and other inventory gaps remain open. No physical providers or production credentials were used.

## Published 20-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 393 tests across 52 unit/integration/doc-test targets, with zero failures or ignored cases. Workspace formatting, strict all-target Clippy, `cargo build --release --locked` and optimized example `--check` passed. Optimized executable SHA-256: `d4943948c5626d3becda0026689db9a9eef6d897e4a58222d4b6296d9164ae48`.

The actual HTTP/WebSocket fixture covers authenticated upload larger than 1 MiB by a read-only user, private current/live collection feeds, administrator mutation, public original/thumbnail/HEAD/range responses, ETag/cache preconditions, caller-bound person picture selection, deletion, oversized/invalid multipart rollback and private/public isolation. The compiled process proves original bytes and metadata through hard restart and no thumbnail upscaling. The image crate contributes 25 validation, codec, EXIF, file, metadata, order and lifecycle cases. The primary caught missing ETag behavior in the locked streaming dependency and verified its wrapper through actual requests.

This checkpoint's inventory reconciled all 1,565 unique manifest domains across 32 disjoint batches: 29 partial, 1,536 unported, zero complete. All accepted evidence paths existed. Image media-source integration, import of upstream storage, exact Pillow resampling, complete static-response parity and exhaustive upstream acceptance remain open. Subsequent entity-lifecycle, webhook and clock source is excluded from that checkpoint and its checks.

## Published 19-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 366 tests across 49 unit/integration/doc-test targets, with no failed or ignored tests. Workspace formatting, strict all-target Clippy, `cargo build --release --locked`, and optimized executable example `--check` passed. Optimized executable SHA-256: `22fcac39e7f83aa7d2bc14b5445b2b6e49ffa52c244ccad4e9e7971f39124839`.

The compiled process now verifies actual `device_tracker.see` reports flowing through registry-backed person selection to zone occupancy and surviving a hard restart. A real authenticated WebSocket fixture verifies person config/storage lists, ordinary-user administration denial, private current/live updates, caller-only profile edits, tracker preservation on profile changes, and actual auth deletion/unlinking. Auth regression covers persisted self-name updates without privilege changes, disabled-account rejection, user event payloads and revocation. App fixtures prove secret-aware read-only known-device import, empty/default YAML, duplicate person identity preflight and writer-lease release after person host shutdown. Gravatar URLs are hashed locally with pinned precedence and require no external request.

This historical checkpoint partitioned all 1,565 manifests over 32 batches: 28 partial native domains, 1,537 unported, zero complete. Its non-null person picture uploads were unavailable until the subsequent image component integration. Source conditions, translations, scanner platforms, complete registry/provider lifecycle and other listed gaps remain open.

## Published 17-crate acceptance

`cargo test --workspace --locked --no-fail-fast` passed 304 tests across the 17-crate workspace and every doc-test target, with no failures or ignored tests. `cargo clippy --workspace --all-targets --locked -- -D warnings` passed. Cargo.lock pins all resolved dependencies. The exhaustive upstream inventory reconciles 1,565 unique integration manifests across 32 disjoint batches: 26 partial native domains, 1,539 unported, zero claimed complete. Every partial-domain evidence path exists.

`cargo fmt --all --check` and `cargo build --release --locked` also passed. The optimized executable successfully validates `examples/configuration.yaml` with `--check`.

This checkpoint's optimized macOS executable SHA-256 is `1dabe8a70a716bc2df0d93f0d44a28a4eff78426633ccc4361391ebdd2dd432c`. Person was outside the workspace and excluded from that checkpoint.

The new wave includes legacy group aggregation and provider delegation; process-local notifications with real authenticated live feeds and private automation triggers; provider-ready, one-shot startup triggers; source-correct helper initial/restore, select options and numerical precision; and native YAML/home/editable zones with WGS-84 geometry, collection CRUD, occupancy, actual reload and live persisted core location updates. The executable fixtures now cover both HTTP notification-to-automation execution and zone behavior through hard restart. Internal zone reload authorization and same-process writer-lease release also pass. Independent automation review found no confirmed discrepancy in the requested private-signal/causality/startup paths against the exact pinned source.

Primary review caught and corrected stale owned zone states after changed-YAML restart, collisions with foreign manual zones, and fixed-decimal formatting that could erase small helper values. The first full run found an invalid minimum-step value in the added precision fixture; the fixture was corrected while retaining the pinned step constraint. The final full run above passes. Zone state/metadata failures attempt compensation; individual state writes do not provide a multi-state transaction or retract events already observed by subscribers.

Actual transport fixtures cover native onboarding/password login, one-use authorization codes, JWT identity, refresh rotation/revocation, user-bound TOTP enrollment/QR and replay rejection, signed paths, proxy origin/local-only policy, public/private subscription separation, config/options flows and actual lifecycle callbacks. History fixtures exercise boundary snapshots, significant/minimal/no-attribute flags and compressed WebSocket output. Internal calls, HTTP and WebSocket services use the same live registry selector resolution.

MQTT tests use local TCP protocol peers to prove discovery, reported state/availability, outbound service commands, reconnect, config-entry setup/retry/reload/unload/restart, registry renames and shared-provider cleanup. REST tests use real loopback HTTP to verify polling/commands, templates, auth/error/recovery and startup/shutdown/drop cleanup. No production credentials, live brokers, physical devices or third-party API activity are used.

Compiled-process acceptance includes an actual YAML include, reactive template sensor updates through native automation, persistent identity/state across a hard process termination, history responses, and SIGTERM exit with an open authenticated WebSocket. The process test caught and corrected a YAML core-setting seed mismatch and static template-name rendering before acceptance.

Storage tests verify owned v1→v2 migration, rollback/foreign-schema rejection, single writer/cache ownership, private file modes, FULL synchronous commits, historical key/time validation, bounded multi-entity/event queries, purge preservation of current state/private metadata and actual repack/reopen. Lifecycle review found REST/TemplateEngine and template-action ownership cycles; owner-scoped cleanup, guarded target callbacks, failed-start rollback and same-process lease-release regressions correct those findings. Drop requests cancellation; explicit async shutdown joins owned work.

These receipts establish the implemented subset. They do not verify complete integrations, frontend/browser use, upstream database compatibility, custom authorization policies, hardware operation, or full Home Assistant parity. Synchronous handlers already running must terminate before joined shutdown can complete.

## Linux checkpoint CI

Checkpoint `831dce75694a2bdba0777aaeef985614895eacae` passed [Linux run 42270320657](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42270320657), including formatting, strict all-target Clippy, all tests and CLI configuration validation. It excludes subsequent tag, device-composite and recording-policy changes.

Checkpoint `33a67c42d782debc54046f32069b05edaf325d98` passed [Linux run 42202324831](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42202324831), including formatting, strict all-target Clippy, all tests and actual CLI configuration validation. It excludes subsequent entity-lifecycle and webhook work.

Checkpoint `76aa09afc13e4a1f36051e9f4e1107d2db6d807b` passed [Linux run 42189652679](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42189652679), including formatting, strict all-target Clippy, all tests and CLI example validation. The separate documentation commit `d5d7727905c895acdbd47213c9ab300eca7db074` also passed [run 42181402458](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42181402458). Neither run contains subsequent image integration work.

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
