# Home Assistant Network integration port contract

Status: native SDK, application and API verified locally; full pinned acceptance remains open.

## Scope and source pin

Behavior below is pinned to Home Assistant Core `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Its Network integration is `homeassistant/components/network/`; it is distinct from this worktree's `crates/hass-api/src/network.rs`, which resolves HTTP client origin and trusted-proxy policy. The latter is not an adapter manager and must not be reused as one.

The pinned integration declares `ifaddr==0.2.0` in its manifest. The native shared provider uses `if-addrs = 0.15`; the app passes its selected snapshot to SSDP before protocol-specific filtering. A standalone SSDP enumeration helper remains available to SDK callers.

## Adapter snapshot and selection

The public adapter record (`components/network/models.py`) is:

| Field | Meaning |
|---|---|
| `name` | `ifaddr.Adapter.nice_name`; this exact string is the configured-selection identity |
| `index` | `ifaddr.Adapter.index`, possibly null |
| `enabled` | Whether the selection algorithm currently includes this adapter |
| `auto` | Whether fallback auto-selection includes this adapter |
| `default` | Whether an address on it matches the source selected for the mDNS route probe |
| `ipv4` | Ordered records `{address, network_prefix}` |
| `ipv6` | Ordered records `{address, flowinfo, scope_id, network_prefix}` |

Enumeration preserves provider order and retains loopback and link-local adapters in the snapshot. Mapping sets `enabled=false`; it does not discard adapters based on protocol-specific eligibility. The adapter index is metadata; selection is by `name`, not index.

`util.async_load_adapters()` probes the local source address for `224.0.0.251`, then marks `default` on the adapter containing that address. The default adapter is `auto` only when the matched address is not multicast, loopback, or link-local. If no adapter is both `default` and `auto`, every adapter with at least one address passing that same predicate is marked `auto`. “External” here is this exact predicate; private IPv4 is eligible.

`enable_adapters()` first clears every `enabled` flag, then enables adapters whose names exactly match `configured_adapters`. If at least one name matches, only those matches are enabled; unknown names are ignored. If none match (including an empty list), auto-detection is applied. Thus an empty or all-unknown saved selection can enable multiple adapters; an old mixed list enables its surviving known names without falling back to auto.

The default configured list is empty. Configuration accepts only `configured_adapters`, a list of strings. There is no separate YAML/config-flow adapter-selection path in this integration.

## Persistence and reload

The integration stores `dict[str, list[str]]` through Home Assistant `Store` with key `core.network`, version `1`, and atomic writes. The stored data is `{ "configured_adapters": ["name", ...] }` inside the normal Store envelope `{ "version": 1, "key": "core.network", "data": ... }`. Loading replaces the in-memory data when a stored value exists. Reconfiguration updates the configured names, reapplies selection, and saves that data. The pinned integration has no explicit migration branch or older-version conversion; do not invent one as source behavior.

`async_reload_adapters()` re-enumerates and reapplies the stored name list (or auto fallback). The Network integration itself has no periodic interface polling or adapter-change event. In the pinned tree, `hassio` listens for a Supervisor update event whose key is `network` and schedules this reload through a 10-second `Debouncer` (`immediate=True`). The reload method itself does not publish a changed-adapter event. The Network test fixture replaces `ifaddr.get_adapters()` with synthetic adapters; route-dependent tests patch socket results or force failure, and the reload test replaces the adapter fixture. They do not require live interface enumeration.

## API contract

The integration registers three WebSocket commands in `websocket.py`. `id` is the usual command correlation field.

| Command | Authorization decorator | Request | Success result |
|---|---|---|---|
| `network` | `require_admin` | `{id, type}` | `{adapters: Adapter[], configured_adapters: string[]}` |
| `network/configure` | `require_admin` | `{id, type, config?: {configured_adapters?: string[]}}`; `config` defaults to `{}`, and its omitted `configured_adapters` defaults to `[]` | `{configured_adapters: string[]}` after reconfiguration and storage save |
| `network/url` | no `require_admin` decorator in this component | `{id, type}` | `{internal: string|null, external: string|null, cloud: string|null}` |

These are ordinary correlated WebSocket results, not pushed events. The component defines no Network service or HTTP route. `network/url` reports configured Home Assistant URLs; it is not an adapter query.

## Source-IP semantics

`async_get_enabled_source_ips()` returns every IPv4 and IPv6 address on enabled adapters, in adapter/address order. It does not deduplicate or apply loopback/global/protocol filtering. IPv6 values are constructed with the configured zone, `IPv6Address(f"{address}%{scope_id}")`; `flowinfo` is not part of this helper's returned address. Disabled adapters contribute no addresses.

`async_get_source_ip(hass, target_ip)` is a different, IPv4-only selector. The lower-level utility opens a nonblocking `AF_INET` UDP socket, connects it to `(target_ip, 1)`, reads `getsockname()[0]`, closes the socket, and returns `None` if that probe raises. With no target, the caller tries `8.8.8.8`, then `224.0.0.251`, then `127.0.0.1`, taking the first non-null route result. With a target, it probes only that target.

If enabled IPv4 addresses exist, the route-selected IPv4 is returned only if it is among them; otherwise the first enabled IPv4 is returned. IPv6 does not participate in this single-source helper. If there is no enabled IPv4, Home Assistant warns and returns the route-selected address even if it is outside the selected-adapter set; it raises `HomeAssistantError` only when no route source can be determined. This fallback matters for empty adapters and all-IPv6 configurations.

## SSDP and Zeroconf boundaries

Pinned Home Assistant SSDP first consumes the Network integration's enabled addresses, then keeps non-loopback, non-global sources; IPv6 is retained only when it has a truthy scope id. Pinned Zeroconf consumes the same selected adapter snapshot but applies its own protocol rules: it selects `IPVersion.All` when functional dual-stack support and an enabled IPv6 adapter are present, switches to V6-only when there is no enabled IPv4 and dual stack is unavailable, and otherwise uses V4-only. With no enabled IPv6 and only the default adapter enabled it passes `InterfaceChoice.Default`; otherwise it passes explicit enabled addresses filtered for loopback, global IPv6, and the chosen IP version.

The native app now consumes the shared `hass-network` selected snapshot. `SsdpHost::start()` maps enabled addresses to `SsdpInterface` and calls `select_sources`, which applies SSDP's own filter and deduplicates without OS enumeration. Its filter is non-global unicast IPv4 or link-local IPv6 with a nonzero interface index. The standalone `enabled_sources()` helper remains available to independent SDK callers. Network preferences select adapters; this consumer does not enable an unselected one. An injected app test verifies exact selection plus the separate loopback/global filter.

Implemented native boundary:

- A separate `hass-network` owner handles enumeration, selection, persistence and snapshots. SDK/API tests inject both providers; the actual-process acceptance case separately checks the OS provider and persistence over loopback.
- The application holds the strong integration; consumers hold weak managers. Explicit configure/reload methods serialize commits and respect owner shutdown.
- SSDP consumes selected addresses before its protocol filter. Future Zeroconf will use the same selection with its own IP-version/default-interface rules. The generic enabled-IP helper preserves source data.
- The HTTP `NetworkPolicy` remains separate from adapter enumeration and preferences.

The adapter implements the source WebSocket commands without adding an HTTP settings route. No independent persistent SSDP selection exists: its injected scanner options are a transport/test seam. Network is the authoritative generic selection. Discovery sockets consume the selected snapshot at startup; no live rebinding behavior is invented.

## Native falsifiers and evidence

Focused fixture checks for the future crate should falsify these common mistakes:

1. Selection is by exact adapter name, not interface index; empty, unknown-only, and partially surviving name lists follow the fallback rules above.
2. The generic enabled-source helper preserves ordered IPv4/IPv6 addresses and IPv6 scope IDs, and does not silently apply SSDP filtering or deduplication.
3. A route-selected address outside enabled IPv4 falls back to the first configured IPv4; no enabled IPv4 uses the route fallback, while failure to determine any route source is an error.
4. A reload with changed indices re-enumerates and reapplies saved names. An enumeration error is surfaced rather than converted to an empty successful snapshot; the pinned utility does not catch `ifaddr.get_adapters()` errors.
5. Storage round-trips the configured-name list with version/key/data identity; schema errors, admin-only preference commands, and the non-admin-decorated URL command preserve their distinct wire behavior.
6. SSDP and Zeroconf consume the same selected snapshot but keep their separate filters and IPv6 scope/IP-version behavior.

Pinned evidence: `components/network/{manifest.json,const.py,models.py,util.py,network.py,__init__.py,websocket.py}`; `tests/components/network/{conftest.py,test_init.py}`; `components/ssdp/common.py`; `components/zeroconf/__init__.py`; and the Supervisor reload hook in `components/hassio/__init__.py`. The local in-progress integration points are `crates/hass-ssdp/src/network.rs` and `crates/hass/src/ssdp.rs`.

## Implemented native SDK contract

The initial native crate lives at `crates/hass-network/` as an isolated Cargo workspace so it can be checked without changing the root workspace while other crates are publishing. `NetworkIntegration::start(runtime, enumerator, resolver)` loads preferences and takes the first provider snapshot. The owner exposes a weak `NetworkManager`; dropping or shutting down the owner deactivates existing handles. Configure/reload commits share a lifecycle lock, and a reload checks that the owner remains active after its provider callbacks return.

`AdapterEnumerator` supplies grouped `AdapterAddresses` with exact names, optional indices, and ordered address records. `SystemAdapterEnumerator` uses `if-addrs 0.15`: its names are operating-system interface names rather than `ifaddr.Adapter.nice_name`, and an interface index may be unavailable on a platform. The crate receives IPv4/IPv6 prefixes from the provider; `if-addrs` does not provide per-address IPv6 flowinfo or scope IDs, so the production adapter records flowinfo as zero and uses the interface index for link-local IPv6 scope (zero when the index is absent), and zero scope for other IPv6 addresses. Unit tests inject both providers and do not call OS enumeration or route probes.

`RouteSourceResolver` returns an optional IPv4 route source for a numeric target. The system resolver binds and UDP-connects to the address, then reads the local socket address; it sends no datagram and performs no DNS lookup. Initial adapter-default detection probes `224.0.0.251`. `NetworkManager::source_ip(None)` separately probes `8.8.8.8`, `224.0.0.251`, then `127.0.0.1`; an explicit target is probed once. With enabled IPv4 addresses, a failed or nonmatching route probe returns the first enabled IPv4 address. With no enabled IPv4 address, the route result is returned and an error is returned only when all relevant probes fail.

Preferences use the normal Store envelope at metadata key `core.network`: `{ "version": 1, "key": "core.network", "data": { "configured_adapters": [...] } }`. The native crate rejects malformed or unsupported envelopes and does not invent older-format migrations. Configure persists the full name list, including duplicates and unknown names, before publishing the new selected snapshot; a persistence failure leaves the old in-memory snapshot and stored value in place. Reload enumerates outside manager locks and applies the latest committed configured names. Empty or unknown-only lists use auto-selection; a partially matching list enables only its surviving names.

The generic `enabled_source_ips()` returns per-adapter IPv4 records followed by IPv6 records, preserving provider order, duplicates, and every scope ID including zero. It drops flowinfo and applies no protocol filter. The crate emits no runtime events and owns no WebSocket command or service registration; API and protocol consumers will use its snapshot later. The `hass-api` HTTP `NetworkPolicy` remains unrelated.

The earlier sentence posing precedence between an independent SSDP preference and Network adapter selection describes a hypothetical that does not exist as a user setting. There is no SSDP setting to override or intersect the generic selection: Network's selected snapshot is the shared input, and SSDP/Zeroconf retain only their own protocol filters afterward.

Focused validation in the isolated crate passed 14 provider-injected tests, including name fallback/reload, address order and scope handling, source-route fallback, Store envelope round-trip/rejection and restart through MemoryStore and SQLite, write-failure atomicity, callback re-entry, and owner teardown. Strict Clippy also passed with `-D warnings`.
