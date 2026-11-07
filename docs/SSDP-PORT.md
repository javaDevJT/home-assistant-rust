# Native SSDP port contract

## Pinned references

Home Assistant Core reference:
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`,
`homeassistant/components/ssdp/` and `tests/components/ssdp/`.
The component pins `async-upnp-client==0.49.0`.

The exact published dependency sdist was downloaded without installation or
execution and verified against PyPI metadata. SHA-256:
`b2306b2277471f3d3544d688159aa98ee656306af7f203a9e48386e9a94bfbd0`.
[Published source archive](https://files.pythonhosted.org/packages/be/b3/289d447e1e393dd03a5bae0847886687553ab1eae87842cd8996205a8b57/async_upnp_client-0.49.0.tar.gz).
Temporary read cache: `/tmp/ssdp0.49_verified.tar.gz` and accompanying metadata;
durable provenance is the version, URL and hash above.

## Confirmed dependency behavior

- `SsdpDeviceTracker.devices` keys by UDN parsed from USN. Cache-Control
  `max-age` matching is case-insensitive; absent/unmatched values default to
  900 seconds. Validity is packet timestamp plus that duration. Locations each
  have expiry. Observations purge expired devices; byebye immediately removes
  a known device. New devices/types/locations or changed relevant headers
  propagate search changes; unchanged searches are `SEARCH_ALIVE`.
- `DescriptionCache` keys by exact location, coalesces concurrent requests,
  caches successful results and failures until explicit uncache, and has no
  independent TTL. GET accepts status 200 only. Empty/malformed XML, conversion
  errors, unexpected root and fetch failure produce cached absence. Conversion
  selects `root.device`; embedded devices stay nested at this layer.
- Search packets use `M-SEARCH * HTTP/1.1`, HOST/MAN/MX/ST and CRLF with a final
  blank line. Decoding normalizes line endings, uses case-insensitive headers,
  and recognizes a case-insensitive `uuid:` prefix while preserving the UDN
  spelling before `::`. Original LOCATION is retained beside its adjusted form.

Evidence is in dependency `ssdp_listener.py:36-516`,
`description_cache.py:24-137` and `ssdp.py:104-236`.

## Pinned adapter rules

The scanner shares one device tracker across all interfaces. Matchers index
nonempty manufacturer, st, deviceType, nt and manufacturerURL values; remaining
keys form an exact subset with case-insensitive names. Integration matcher `*`
is literal. Callback `*` means presence, including typed internal fields.
Initial registration replays cached services as ALIVE before attaching, without
assigning matching integration domains. Repeated SEARCH_ALIVE callbacks also
have no matching domains and do not create config flows. Live change callbacks
run before flow admission. BYEBYE dismisses flows by original ST/location and
creates none. Getters likewise leave matching domains empty.

Descriptions are fetched before matching, using a ten-second requester with
unverified TLS. Cache failures remain cached. Config-entry removal reprocesses
only ready, nonempty cached descriptions using generic SEARCH and skips ordinary
callbacks; it never refetches. Its key is provider `ssdp`, string UDN, version 1.
The bundled native metadata matches the pinned generated registry: 44 domains,
94 matchers. Integration metadata is independent of native driver availability.

The private startup dispatcher queues by domain/source, deduplicates equal
initial data, starts after `homeassistant_started`, and limits startup flow
initialization to 20. Active flow matching uses source, a context subset and
original init data. User-protected, options and subentry flows are excluded
from discovery dismissal. Native callbacks run outside config state locks.

The only SSDP WebSocket command is administrator-only
`ssdp/subscribe_discovery`, accepting just id/type. ACK with null precedes cache
replay. ALIVE/UPDATE emits `add` with friendly-name-or-null plus every service
info field. BYEBYE emits `remove` with only ST/location. There is no empty
initial event, finish event, public event-bus subscription, HTTP API or service.

The source server waits for `homeassistant_started`. Stable raw 32-hex instance
identity becomes uppercase `UUID:` plus uppercase hyphenated UUID. Device type
is `urn:home-assistant.io:device:HomeAssistant:1`; friendly name uses the saved
location plus `(Home Assistant)`. Source model metadata describes its actual
runtime. The native server likewise reports its actual version/install type.
It binds and retains the dynamic TCP socket through startup, serves device XML,
and owns reusable per-interface UDP sockets. M-SEARCH validates request line and
quoted MAN, matches root/UDN/type/all, and preserves requested type spelling.
One alive record is sent immediately, then the three single-device records
rotate every 30 seconds. Shutdown sends all three byebye records. Source does
not emit update advertisements. Valid MX delays one response batch, capped at
five seconds. Native invalid negative MX is rejected without reproducing the
dependency's invalid random-range failure.

Primary references: `scanner.py`, `websocket_api.py`, `server.py`,
`homeassistant/helpers/service_info/ssdp.py`,
`homeassistant/helpers/discovery_flow.py`, `homeassistant/config_entries.py`,
and dependency `server.py:462-939,1188-1194,1287-1290`, `ssdp.py:401-459`.
Dependency `ssdp.py:231` uses naive local `datetime.now()`; native wire metadata
preserves its no-offset microsecond format rather than changing it to UTC JSON.

## Native integration candidate

`crates/hass-ssdp` provides protocol/tracker, description cache, manager, UDP
scanner and UPnP server. Its [README](../crates/hass-ssdp/README.md) is the native
API reference. `hass/src/discovery.rs` owns startup admission and joins admitted
synchronous driver callbacks at shutdown; aborting a blocking JoinHandle would
detach that work. `hass/src/ssdp.rs` owns the scanner, weak key-removal observer,
native identity, server and dispatcher. Explicit `ssdp` YAML enables the host;
its empty-config validator accepts ignored values and warns for truthy ones
without printing parameter contents. Main starts advertisements only after
the real startup event. The API owns bounded, cancellable callback subscriptions.

The frozen 30-crate workspace passes 717 tests across 82 targets, with zero
failed/ignored cases, formatting, strict locked all-target Clippy, optimized
build and optimized CLI validation. SSDP has 47 unit and eight manager cases,
four actual WebSocket cases, four app dispatcher cases and three host cases.
Configuration has 31 passing cases, including latest-commit key snapshots and
private notification after a cleanup failure. Actual concurrent HTTP lookup
has a red/green case for latest device headers. A full-suite queue-idle race
was corrected by reserving work under the same lock that removes it from the
queue. Source publication and exact Linux CI remain separate from the local
gate; see [validation](VALIDATION.md) for immutable receipts.

Open scope: complete header-parser edge parity, uncommon Python text codecs,
complete platform address metadata, frontend assets/consumers and full
pinned upstream acceptance. The app advertises no icon URLs until the native
assets are served. Native SERVER/model/version values intentionally describe
the running Rust implementation. SDK transferred child tasks receive abort
requests; wrapper cancellation is joined separately. These limits keep SSDP
partial rather than complete.

All implementation falsifiers use synthetic datagrams and loopback HTTP/UDP
fixtures. No physical-device commands, LAN discovery or live provider probes
were performed. Python package execution and a Python runtime bridge are outside
the native implementation.
