# DHCP native port source contract

Pinned upstream source: `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` (the supplied object resolves to this full commit). This is a behavior contract for native implementation planning; upstream tests were inspected, not run.

## Component boundary and providers

`dhcp` is a system integration with a `network` dependency and `local_push` classification. Its YAML schema is empty. `__all__` exposes `DOMAIN` and `async_discovered_service_info`; the address cache getter and callback registration helper are explicitly internal APIs. The WebSocket API is admin-only. There are no DHCP services or user-configurable YAML options in this component. ([manifest](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/manifest.json#L1-L21), [component setup and exports](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L67-L76), [internal helpers](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/helpers.py#L13-L50), [WebSocket](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/websocket_api.py#L20-L35))

Every provider enters the same `async_process_client(ip, hostname, mac, force=False)` path:

- **DHCP packet callback:** after `homeassistant_started`, call `aiodhcpwatcher.async_start(_async_process_dhcp_request, if_indexes)`. Each `DHCPRequest` supplies `ip_address`, `hostname`, and `mac_address`; the HA adapter forwards those fields unchanged. If only the default network interface is enabled, it passes `None`. Otherwise it selects enabled adapters with an index, IPv4 addresses, and at least one IPv4 address that is neither loopback nor global. ([packet adapter](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L411-L448))
- **Device tracker states:** before startup, subscribe to existing and newly added `device_tracker` states. Accept only `home` states whose `source_type` is `router`; require IP and MAC, and default a missing hostname to `""`. ([state watcher](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L350-L386))
- **Device tracker registration events:** subscribe to `CONNECTED_DEVICE_REGISTERED`; require IP and MAC, and turn a missing hostname into `""`. ([registration watcher](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L388-L408))
- **Active host scan:** create `aiodiscover.DiscoverHosts`, scan immediately after startup and every 60 minutes, and forward each result's IP, hostname, and MAC. Do not start a second scan while one is running. ([scanner](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L72-L76), [network watcher](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L296-L348))

These are separate observation sources, not merely ways to decode DHCP packets. Keep each provider behind an observation callback; matcher/cache/flow logic should not own packet capture or active network scanning.

## Dependencies and unaudited engine boundary

The pinned manifest requires:

- `aiodhcpwatcher==1.2.7` for DHCP capture and parsed request callbacks.
- `aiodiscover==3.3.2` for active host discovery.
- `cached-ipaddress==1.1.2` for cached address parsing.

The HA component source only calls `aiodhcpwatcher.async_init()` and `async_start(callback, if_indexes)`, then stores the returned unsubscribe callable. Its tests use `aiodhcpwatcher.make_packet_handler` with Scapy Ethernet fixtures and patch Scapy interface/filter operations, but the package source is not part of this pinned component tree. Before choosing a native packet engine, audit `aiodhcpwatcher==1.2.7` for packet/request parsing, interface selection, permission/filter failures, callback thread/event-loop semantics, and unsubscribe behavior. Audit `aiodiscover==3.3.2` separately if native active scans are in scope. Do not infer a Rust crate or claim capture parity from the HA adapter tests alone. ([manifest](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/manifest.json#L17-L21), [packet tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L148-L169))

## Normalization, cache, and matching

`DHCPAddressData` is `{hostname: str, ip: str}` stored in an in-memory map keyed by canonical MAC. Processing rules:

1. Parse the IP with `cached_ip_addresses`. Ignore invalid, link-local, loopback, and unspecified addresses. Store the compressed IP form.
2. Normalize MAC with `format_mac`, then remove colons. This lowercase, colonless string is the cache and discovery-key value; use the colon-formatted MAC for device-registry lookup and uppercase colonless MAC for matcher/OUI comparison.
3. For an existing MAC, suppress an observation unless `force` is set when the compressed IP is unchanged and the old hostname starts with the incoming hostname. The prefix check is case-sensitive. A longer hostname can therefore replace a truncated one; a shorter/truncated hostname is ignored. An IP change is accepted even with the same hostname.
4. Store the original hostname and compressed IP before matching. Match hostname patterns against `hostname.lower()`. A matcher with `macaddress` is indexed by its first six characters and matched against uppercase colonless MAC; otherwise a hostname matcher is indexed by the first character of its pattern lowercased. Patterns use `fnmatch` glob semantics. If a hostname pattern is present, it must match. A matcher with `registered_devices` is indexed only as a registered-device rule; it matches when a device registry entry for this formatted MAC belongs to that integration domain. Check all matching registry entries so multiple domains/config entries sharing one MAC are retained. A set of target domains means multiple matching rules for one domain still produce one flow.
5. Invoke registered internal callbacks with a one-entry map `{canonical_mac: {hostname, ip}}` after an accepted observation, even when no integration matcher hits. Do not emit a callback for a rejected or deduplicated observation.

There is no lease timeout, stale-address sweep, or cache deletion in this code. Updates overwrite by MAC; cache lifetime is the process lifetime. The accepted update callback and WebSocket surface are add/update-only. ([matcher index](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L81-L114), [normalization/cache/matching/flow](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L160-L294), [data model](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/models.py#L14-L41))

## Generated matcher registry and discovery-flow contract

The generated `DHCP` list contains `{domain, ...manifest matcher fields}` for integrations in sorted domain order. The pinned loader copies that list and appends DHCP matchers from loaded custom components before the component indexes it. The matcher fields are `hostname`, `macaddress`, and `registered_devices`; registered-device rules are handled through the device registry, not packet fields. A native implementation must carry the generated matcher data and preserve the custom-component append seam if custom components are supported. ([generated registry](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/generated/dhcp.py#L6-L35), [matcher type](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/loader.py#L222-L238), [registry loader](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/loader.py#L638-L649), [generator](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/script/hassfest/dhcp.py#L7-L22))

For every matched target integration domain, emit one discovery candidate with:

- `domain`: the matched target integration domain.
- `source`: `dhcp` (`SOURCE_DHCP`).
- `discovery_key`: `DiscoveryKey { domain: "dhcp", key: <lowercase colonless MAC string>, version: 1 }`.
- `context`: source and typed discovery key.
- `data`: DHCP service info `{ip, hostname, macaddress}`; flow data uses the incoming IP string, lowercased hostname, and lowercase colonless MAC. The stored cache IP is compressed, so the flow IP and cached IP can have different text forms.

Keep the discovery-key domain `dhcp` separate from the target integration domain. The native shared `DiscoveryHandle::create` already accepts target domain, source, typed key, context, and data; use that one queue so DHCP flow starts share the existing startup buffering and 20-callback admission limit. The upstream typed key is serializable and permits a string or tuple key. ([flow call](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L273-L294), [key type and flow context](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/discovery_flow.py#L21-L60), [native handle](../crates/hass/src/discovery.rs#L81-L149))

When a config entry is removed, inspect only its `discovery_keys["dhcp"]`; accept version 1 string keys, look up the cached MAC, and force the cached observation through the normal path so current matcher/registry state can rediscover it. Ignore another discovery domain, another key type, or a future key version. ([rediscovery watcher](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L450-L479))

## Read and event APIs

`async_discovered_service_info` returns one service-info row per cached MAC with the stored compressed IP, lowercase hostname, and lowercase colonless MAC. The internal getter exposes the backing map; the internal callback registration returns an unsubscribe function. The admin-only `dhcp/subscribe_discovery` WebSocket command registers a callback, sends success, then emits a snapshot as `{add: [...]}`. Each later accepted observation emits another `{add: [one row]}` event. WebSocket MAC values are formatted uppercase with colons; rows contain `mac_address`, `hostname`, and `ip_address`. There is no remove event. ([service info](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/service_info/dhcp.py#L8-L20), [snapshot API](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/helpers.py#L13-L50), [WebSocket schema](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/websocket_api.py#L20-L61))

## Startup, stop, and failure lifetime

Setup installs state and device-registration watchers immediately so existing and early events are not missed. On `homeassistant_started`, it initializes `aiodhcpwatcher`, starts the immediate/periodic host scan, awaits packet watcher startup, then subscribes to DHCP config-entry removal. The stop event unsubscribes all started watchers; the network watcher cancels its current scan task, while the packet watcher invokes the unsubscribe callable returned by the packet package. The DHCP cache is not cleared by watcher stop. ([setup/stop order](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L116-L157), [watcher stop](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L160-L177), [network cancellation](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/dhcp/__init__.py#L296-L316))

The upstream tests set up the integration before the start event, then require capture failure logging for root without usable capture permission, non-root without `CAP_NET_RAW`, and broken/missing libpcap filter support. HA awaits the dependency's startup without a local `try/except`; the dependency owns the exact catch/log/partial-start/unsubscribe behavior. Audit that package path before claiming failure or cleanup parity. ([failure tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L699-L794))

## Narrow native ownership boundaries

1. **Provider adapters:** packet capture and active host scanning own their external resources and emit `{ip, hostname, mac}` observations. State/registration adapters map the existing device-tracker events to the same observation type. Do not let providers mutate discovery flows directly.
2. **Observation/cache and matcher core:** a deterministic component validates/canonicalizes IP and MAC, applies the exact update/dedup rule, stores the MAC record, computes matched target domains from generated plus supported runtime matchers, and emits the accepted-update callback record.
3. **Discovery adapter:** translate each `(target_domain, observation)` to the shared `DiscoveryHandle` candidate contract above. A separate config-entry removal observer reads the DHCP discovery key and forces the cached observation through the same path.
4. **Read surfaces:** expose the service-info snapshot and admin WebSocket snapshot/add-update feed over the cache; do not invent deletion notifications absent from upstream.

## Acceptance falsifiers from pinned tests

These synthetic tests define concrete mismatches; no LAN or production capture is needed. The inspected upstream tests were not run.

- Packet fixtures must distinguish valid request/renewal DHCP from non-DHCP frames, non-request packets, invalid/missing hostname options, and malformed option data; the captured callback must produce the exact IP/hostname/MAC service info. ([packet/matcher cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L218-L273), [invalid packet/options cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L477-L631))
- Reject invalid, link-local, loopback, and unspecified IPs; accept router tracker events only when home/router/IP/MAC predicates hold; support hostname-less MAC matches and exact device-registration mapping. ([device tracker cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L836-L1143))
- Match hostname-only, MAC-only, combined and registered-device rules; produce one target-domain flow when multiple rules match; discover every registered target domain sharing a MAC. Verify exact source, discovery-key version/domain/key, and service-info payload. ([matcher/registry cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L218-L474))
- Suppress repeated observations and shorter hostnames, allow a longer hostname or changed IP, and force rediscovery on removal only for DHCP version-1 string keys found in the cache. ([dedup case](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L1187-L1256), [rediscovery cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L1341-L1475))
- Preserve adapter-index selection, stop unsubscribe/cancel behavior, service-info snapshot normalization, capture permission/filter failure logs, and WebSocket result → snapshot → incremental-add ordering and row formatting. Add an explicit WebSocket non-admin denial assertion if native authorization is implemented. ([adapter selection](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L171-L216), [setup/snapshot/failure](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_init.py#L632-L794), [WebSocket event](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/dhcp/test_websocket_api.py#L15-L75))

## Pinned DHCP engine provenance and runtime contract

### Published source provenance

Audited the exact pinned PyPI source distributions on 2026-11-07. For each archive, the downloaded bytes' SHA-256 equals the `digests.sha256` value in that version's PyPI JSON metadata. Source was read from archive members only: none of these packages was installed, imported, or executed.

- `aiodhcpwatcher==1.2.7`: [PyPI metadata](https://pypi.org/pypi/aiodhcpwatcher/1.2.7/json), [sdist](https://files.pythonhosted.org/packages/47/ae/bb51e9ff147dae5102d4b949a2bfd4617b27bed8604bf7a6a5885d08371c/aiodhcpwatcher-1.2.7.tar.gz); SHA-256 `a9e83daea34e576a72c4cf52ccac6ba98c1389cb08de1d5c086698f4fda8e6ca`; local archive `/tmp/dhcp-engine-source-audit-2026-11-07/aiodhcpwatcher-1.2.7.tar.gz`.
- `aiodiscover==3.3.2`: [PyPI metadata](https://pypi.org/pypi/aiodiscover/3.3.2/json), [sdist](https://files.pythonhosted.org/packages/39/2d/a4b0cf7878373b0d8a0d3882afd0c1ee7cd93db3378ca2c8ce5567eabb59/aiodiscover-3.3.2.tar.gz); SHA-256 `0919faf317035670f1458b1ff02302b253aa6c7ba33bf6638e61c23f759ed8de`; local archive `/tmp/dhcp-engine-source-audit-2026-11-07/aiodiscover-3.3.2.tar.gz`.
- `cached-ipaddress==1.1.2`: [PyPI metadata](https://pypi.org/pypi/cached-ipaddress/1.1.2/json), [sdist](https://files.pythonhosted.org/packages/09/5f/a5f4c350307c00fa23322acd24ad8f6a46929a7cfd321e2cc3870f689524/cached_ipaddress-1.1.2.tar.gz); SHA-256 `232f62f768a2531e3d64e661eb2e484b52954ceb8d2b04ad0d2317f52ce25108`; local archive `/tmp/dhcp-engine-source-audit-2026-11-07/cached_ipaddress-1.1.2.tar.gz`.

The inspected archive paths are `aiodhcpwatcher-1.2.7/src/aiodhcpwatcher/__init__.py`, `aiodiscover-3.3.2/aiodiscover/{discovery,network}.py`, and `cached_ipaddress-1.1.2/src/cached_ipaddress/ipaddress.py`. Source line references below refer to those exact archive members, not a floating repository branch. PyPI metadata declares Scapy as the packet watcher's runtime dependency; the scanner depends on aiodns, ifaddr, pyroute2, and cached-ipaddress.

### Packet capture and decoder contract

The capture filter is exactly `udp and (port 67 or 68)`, applied to Scapy's `ETH_P_ALL` listener. The watcher sets `conf.sniff_promisc = 0`. A captured packet is only reported if it has a Scapy `DHCP` layer and its parsed `message-type` option is the integer `3` (`DHCPREQUEST`). The filter alone therefore admits UDP on either source or destination port 67/68; it is not a DHCP-message decoder. The handler does not check BOOTP opcode or use BOOTP `ciaddr`, `yiaddr`, or `chaddr`.

For a reported request, the last parsed value for a repeated option wins until the `end` option. A truthy `requested_addr` is the reported IP; otherwise it uses the source address from the packet's `IP` layer (the renewal fallback), not a BOOTP address field. A DHCP layer without an IP layer can still be reported when it has `requested_addr`, but only if an Ethernet layer is also present. The MAC is the Ethernet source as returned by Scapy; the handler does not canonicalize it. Missing or non-byte hostname data becomes `""`; hostname bytes are decoded as IDNA, with UTF-8 replacement as the Unicode-error fallback. It emits only when IP and Ethernet source are both available. Exceptions while reading Scapy layers/options are not caught by this handler; only hostname Unicode errors have a fallback. The HA cache/matcher then applies the separate normalization contract above. (Archive member `src/aiodhcpwatcher/__init__.py:20-94`.)

`if_indexes` selects a deduplicated set of interfaces; a missing or empty list selects Scapy's default interface. Filter compilation is mandatory: a missing or broken filter compiler logs an error and disables capture instead of listening to all traffic. A handled socket-open failure logs and aborts setup; the root and non-root messages differ. Socket setup runs in an executor, then the asyncio loop registers each valid descriptor with `add_reader`, so the packet callback runs on that event loop. Invalid descriptors, permission errors, and unsupported reader operations are logged and their sockets closed. A receive `OSError` stops the readers and schedules a restart in 30 seconds; other fatal receive exceptions shut the watcher down. The unsubscribe callable sets shutdown, cancels restart timer/task, removes readers, and closes sockets. Known setup failures are logged and `async_start()` resolves with the unsubscribe callable rather than raising to the caller. (Archive member `src/aiodhcpwatcher/__init__.py:97-308`.)

For multiple selected interfaces, opening the first handled failing interface aborts setup. Any earlier opened sockets remain owned by the watcher until its unsubscribe callable runs. Cancellation while `async_start()` is awaiting executor-backed `_start()` has no local `finally` cleanup; if the caller cancels before receiving the unsubscribe callable, source does not prove that any sockets opened by the executor are closed. A native provider should preserve the BPF selection, decoded DHCPREQUEST gate, exact address/hostname/MAC fields, event-loop callback contract, and explicit unsubscribe boundary, and should test startup cancellation without assuming the package closes those sockets. Do not infer a broader packet parser from the HA fixture helper.

### Active scanner contract and limits

HA constructs `DiscoverHosts()` without `local_ip`; capture interface indices do not pin this separate scanner. The package chooses one IPv4 source by trying a UDP socket `connect()`/`getsockname()` against `10.255.255.255`, `224.0.0.251`, `8.8.8.8`, then `127.0.0.1`. It matches that address to `ifaddr` adapter data for the prefix; if no prefix matches, it assumes `/24`. An explicit valid `local_ip` would pin one interface subnet, but HA does not supply it. The scanner returns no results when `network.num_addresses > 2048`; otherwise it enumerates that one IPv4 network's usable hosts. It does not walk every HA-enabled adapter or subnet. (`discovery.py:146-180,262-309`; `network.py:31-44,113-144,261-308`.)

Discovery is PTR-led. `resolv.conf` nameservers are deduplicated and retained only if they are in `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, or `192.168.0.0/16`; public resolvers are excluded. If a default router is known, is not already listed, no configured server lies in the chosen subnet, and the router answers the neighbor lookup, the router is appended as a DNS server. The default resolver sets the DNS no-recursion flag. It queries reverse PTR for subnet host addresses in concurrent buckets of 64, with a two-second resolver timeout and a three-second wait bound per bucket. It accepts only a valid RFC 1035 LDH short label (1-63 characters, alphanumeric at both ends); only the first label is kept, and an `xn--` label is IDNA-decoded. If one nameserver yields at least one accepted hostname, it stops querying later nameservers. Failed nameservers are remembered only when some nameserver yielded a hostname, and the remembered failures clear when `resolv.conf` changes or the 24-hour cache interval expires. (`discovery.py:36-82,85-125,311-368`; `network.py:31-46,66-104`.)

For host results, neighbor lookup runs after PTR lookup and only for PTR-backed IPs; the router fallback separately looks up the router IP while choosing nameservers. Existing neighbor entries are reused. Missing entries trigger a nonblocking empty UDP datagram to port 80 per IP, a ten-second ARP-cache wait, then a second neighbor-table read; this is not a raw ARP packet encoder. On Linux with pyroute2, route and neighbor records come from `AsyncIPRoute`; macOS uses `route -n get -inet default` for the gateway and `arp -a -n` for neighbors. The fallback `arp` command has a ten-second timeout. Results require both an accepted hostname and a validated neighbor IP/MAC. Neighbor IPs that are loopback, link-local, multicast, or unspecified are dropped. MAC validation permits one- or two-digit octets and colon or hyphen separators; the implementation zero-pads by splitting on colons, so hyphen-delimited inputs retain hyphens. All-zero and broadcast MACs are dropped. (`network.py:155-202,205-231,310-384`; `discovery.py:262-309`.)

Scanner setup can propagate a missing `resolv.conf` error on non-Windows hosts or fail when no local IPv4 address can be selected. Route discovery and router lookup failures are suppressed where shown in source; missing/failed `arp` execution and its timeout yield no neighbor rows. Cancellation cancels pending PTR futures and resolver queries, closes the UDP cache-population socket in `finally`, and kills/reaps the fallback `arp` or macOS `route` subprocess on timeout/cancellation. `DiscoverHosts.close()` releases the DNS resolver and pyroute2 socket and makes the instance unusable. The pinned HA adapter cancels its current scan task on stop but does not call `DiscoverHosts.close()`; a native lifecycle should not claim that the HA adapter currently invokes this package cleanup method. (`discovery.py:85-125,194-246`; `network.py:164-202,310-384`; HA adapter `homeassistant/components/dhcp/__init__.py:296-348` at the pinned core commit.)

### Cached IP parser boundary

`cached_ip_addresses` is an LRU cache of at most 535 parsed objects. It tries stdlib `IPv4Address` first, then `IPv6Address`; invalid values return `None`. Its wrappers cache common properties such as string form, `compressed`, and address flags. It adds no DHCP-specific policy: HA still decides which parsed addresses to reject and which text representation to store. (`cached_ipaddress/ipaddress.py:10-169`.)

### Native falsifiers using synthetic inputs

These checks can run against in-memory frames, stubbed sockets/resolvers/route tables, and fake time; no packet capture, LAN probe, or live DNS is needed.

- **Packet decode:** accept a synthetic Ethernet/IP/UDP DHCPREQUEST with a requested address and byte hostname; accept a renewal with no requested-address option by using IP source; decode IDNA and exercise invalid IDNA UTF-8 replacement. Reject non-DHCP, discover/offer/ack types, absent IP plus absent requested address, missing Ethernet, and hostname-only packets. Include duplicate option keys, an `end` option, an option after `end`, and contrasting BOOTP `chaddr`/`ciaddr`/`yiaddr` to prove the selected fields. Separately fake capture admission for either UDP port direction and nonmatching ports/protocols. Add malformed layer/option cases and require the native decoder to reject them without terminating capture.
- **Capture lifecycle:** feed duplicate interface indexes and empty/None selections to a fake backend; assert filter compilation failure never falls back to unfiltered capture; inject open/reader/receive errors and assert log/result, restart timing, and close/remove-reader calls. Cancel while executor-backed startup is blocked or after it opens a fake socket; require native cleanup even though the pinned package source has no cancellation `finally` around that await.
- **Scanner subnet and PTR:** stub route-selected local IPv4 plus adapter prefixes and `resolv.conf`; assert one inferred subnet, `/24` fallback, explicit `local_ip` pinning, and the 2048-address threshold (`>2048` makes zero DNS/neighbor calls). Stub PTR answers for selected hosts, no-answer/timeout servers, invalid labels, multiple labels, and an `xn--` label; verify only a valid first label is used and later DNS servers are consulted only until one returns at least one accepted host. Prove public nameservers are ignored and the router fallback is added only under its stated predicates.
- **Scanner neighbors/cancellation:** seed a fake neighbor table and assert existing entries avoid probes; missing PTR-backed IPs receive only empty UDP-to-port-80 cache-population requests, then a mocked ten-second wait and reread. Assert rows require hostname plus neighbor; exercise invalid IP/MAC records, single-digit MAC octets, hyphen separators, all-zero/broadcast MACs, pyroute2 and `arp -a -n` paths, command failure/timeout, and cancellation cleanup for resolver futures, subprocesses, UDP sockets, and explicit `DiscoverHosts.close()`. Also test watcher stop: pinned HA cancels the scan task but does not call scanner `close()`.
- **Cached IP behavior:** table valid IPv4/IPv6 strings, byte/integer forms, invalid text, and canonical/compressed values; assert a repeated input hits the cached parser while HA filtering remains separately responsible for link-local, loopback, and unspecified rejection.

No package tests, capture operations, LAN probes, DNS queries, or provider traffic were run as part of this source audit.

## Current native implementation boundary

The current 35-crate workspace includes `hass-dhcp`, `hass-dhcp-wire` and
`hass-dhcp-discover`. The cache SDK retains the exact 386 generated matchers,
119 target domains and 43 registered-device rules. These are metadata, not
accepted device integration implementations. Source review corrected presence
semantics for `registered_devices: false`; the branch is selected by key
presence. Cache snapshots preserve original hostname casing and compressed IP
text; live discovery data preserves incoming IP text and lowercases hostname.
The typed key remains a version-1 string in provider domain `dhcp`.

The app reads existing and newly added router/home tracker states, rather than
every subsequent tracker update. A typed native connected-device publisher
forwards actual integration observations through the private dispatcher;
it does not create a tracker or publish/persist a public event. The app queries
all matching registered-device rows and replays removed accepted discovery
keys through the SDK. Startup admission shares the existing twenty-callback
queue. `dhcp/subscribe_discovery` registers before ACK and sends one cache
snapshot (including an empty `add` array), followed by add-only updates. It
shares the socket's limits and joined cancellation with other native streams.

Packet decoding is native Rust for Ethernet IPv4/IPv6 UDP 67/68 BOOTP requests.
The requested-address option is preferred; only an IPv4 header supplies the
fallback source address. Duplicate options use the last value before `end`.
Hostname decoding uses IDNA2003 Nameprep/round-trip validation, with the source's
whole-input UTF-8 replacement fallback. The capture backend uses
`pnet_datalink` 0.35.0 with promiscuous mode disabled and a bounded read timeout.
It receives broader frame types than the pinned kernel BPF filter, then
discards nonmatching traffic before observation delivery. No raw frames are
logged or persisted. Captures require explicit selected Network interfaces;
an empty selection stays idle. This default-interface projection is narrower
than the Python library's empty/default selector behavior. Other link-layer
formats and full native kernel filtering remain open.

Capture callbacks enter a bounded runtime queue with visible overflow; one
consumer performs cache/flow work. Providers start after the real started
event. Active scans run immediately and on anchored hourly deadlines, skipping
overlaps and missed ticks. The scanner bounds the subnet at 2,048 addresses,
uses local nonrecursive PTR queries, checks the router before PTR, and probes
neighbors only for named hosts. Failure caching is committed only after
another resolver succeeds, resets for resolver configuration changes, and
expires on the independent 24-hour timer. DNS name bounds include the root
terminator, and observations follow ascending host order. The system route and
neighbor provider currently supports Linux; macOS reports an explicit
unsupported-platform error while injected providers remain usable.

Shutdown stops/joins capture workers, drains/joins their consumer, cancels and
joins scanning, and only then drops the cache SDK. An in-flight SDK ingest may
finish after concurrent owner drop, so adapters own that ordering. Focused
proofs cover pure protocol fixtures, actual loopback DNS, fake capture frames
through the real decoder, actual Runtime/registry/flow behavior and loopback
WebSocket transport. No live capture, LAN scan, real resolver or device traffic
has been used for acceptance. Custom integration loading, platform coverage,
interface hot rebinding, frontend consumers and full pinned upstream fixtures
remain open. Exact gate/publication receipts are in [VALIDATION.md](VALIDATION.md)
and [PORTING.md](PORTING.md).
