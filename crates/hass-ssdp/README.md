# Native SSDP

The crate implements the pinned Home Assistant SSDP adapter and its UPnP
transport in Rust. The source contract and exact dependency provenance are in
[SSDP-PORT.md](../../docs/SSDP-PORT.md).

`SsdpIntegration` strongly owns a shared tracker, description cache, private
callbacks, and scheduled work. `SsdpManager` is weak. All `SsdpScanner`
interfaces feed that one manager. `observe` accepts decoded packets and an
injected clock; getters return discovery info by UDN/ST, ST, or UDN.

`register_callback` replays matching cached services as ALIVE before attaching
live delivery. Its guard removes exactly its own registration. Filters use
case-insensitive header names, exact values, and `*` for presence. Async
callbacks use `register_async_callback` and transfer their owned future into
the integration scope. `owned_discovery_task` adds abort-on-drop behavior to
a spawned task; raw Tokio handles otherwise detach when dropped. Explicit
integration shutdown cancels and joins the scheduled wrapper tasks. A
transferred child receives an abort request when its wrapper is dropped.

The exact-URL description cache coalesces concurrent GETs and caches success or
absence until invalidated. The production client uses ten seconds and accepts
unverified TLS, following the pinned adapter. HTTP charset decoding is strict;
XML conversion preserves device mapping shapes. The tracker retains discovery
order, per-location expiry, original header names, and typed metadata. Public
timestamps use the dependency's naive local microsecond format, while native
tracking keeps UTC instants.

`DiscoverySink` receives typed create/dismiss work. Matching integrations come
from 94 pinned matchers across 44 domains; those metadata records do not imply
that each integration has a native driver. Rediscovery after config removal
uses only ready, nonempty cached descriptions, performs no HTTP request, and
does not invoke ordinary callbacks. Discovery does not publish Runtime events.

`SsdpScanner::start` opens actual search and advertisement UDP sockets. Its
defaults send multicast then IPv4 limited broadcast immediately and every ten
minutes, with scoped IPv6 support. Queues and concurrent observations are
bounded. Zero usable interfaces produce an idle owner. Explicit shutdown stops
receivers and joins owned observation work; Drop provides cancellation fallback.

`UpnpServer` keeps a bound HTTP socket through startup, serves escaped
`/device.xml` through GET/HEAD, receives multicast M-SEARCH on a wildcard
listener with separate source-bound outbound sockets, cycles one alive record every
30 seconds, and sends every byebye record at explicit shutdown. Its SERVER
header and app-supplied metadata truthfully identify the native implementation.
The caller supplies stable identity and real presentation/icon URLs.

The application supplies sources from the shared Network manager. Standalone
callers can still use `enabled_sources`; `select_sources` filters an already
selected snapshot without enumerating or enabling other adapters.

Tests use synthetic packets and actual loopback HTTP/UDP only. The API fixtures
add real loopback WebSocket traffic. Full parser edge parity, uncommon Python
codecs, complete platform address metadata, frontend assets, and
exhaustive upstream acceptance remain open. The app currently emits an empty
icon list because native frontend icon assets are not served. No LAN/device
probe or production deployment is part of these checks.
