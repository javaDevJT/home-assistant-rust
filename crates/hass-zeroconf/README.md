# `hass-zeroconf`

`hass-zeroconf` owns Home Assistant's shared Zeroconf discovery policy. A
runtime-provided [`ZeroconfBackend`] supplies complete cache hits, PTR-backed
instance names, and owned asynchronous service resolution. The `backend`
module supplies the native DNS-SD wire adapter using the locally patched
`mdns-sd` crate. It receives a complete selected-interface snapshot and never
enumerates host interfaces itself.

## Ownership and callbacks

`ZeroconfIntegration` is the strong runtime owner. It captures a Tokio
`Handle`, owns cache-driven lookup tasks, and aborts then joins them during
`shutdown`. Dropping the owner synchronously aborts outstanding jobs as a
fallback. `ZeroconfManager` and `ListenerGuard` hold weak references, so an API
handle or subscription cannot keep the integration alive. Dropping a listener
guard removes exactly its own registration.

The native browser calls `service_update(type, name, state)` for Added,
Updated, and Removed states. Added/Updated callbacks use complete cache records
synchronously; a miss transfers an owned backend lookup into the manager's
ten-second cancellation scope. Removed callbacks admit a name-only Dismiss
action and notify name-only removal listeners. The manager does not delete
cache data or publish on a public runtime event bus.

Raw update listeners run before address validation and generated matcher
dispatch. The runtime registers HomeKit classification, alternative-domain
filtering, duplicate Home Assistant UUID checks, and raw WebSocket observation
at this hook. Ordinary generated matcher actions keep their source set to
`zeroconf`; HomeKit flows and alternative-domain decisions belong to that
runtime hook.

## Registry and cache access

`MatcherIndex::builtin()` loads the generated Home Assistant matcher registry.
`ZeroconfManager::service_types()` returns only generated types;
`browse_types()` adds `_hap._tcp.local.`, `_hap._udp.local.`, and
`_home-assistant._tcp.local.` for the browser. `snapshot_names()` and
`cached_snapshot()` walk generated types only, matching the WebSocket initial
cache scan. An API can register update/removal listeners and send its
subscription acknowledgement before walking `snapshot_names`; it can use
`get_cached` for complete entries and `resolve` for incomplete PTR entries.

`RawZeroconfServiceInfo` aliases the frozen `RawResolvedServiceInfo` model.
`ZeroconfServiceInfo::from_raw` performs Home Assistant's usable-address
selection. Raw listeners still receive records with no usable address, which
allows APIs and runtime-specific policies to observe the original TXT and
address fields. `ZeroconfProperties::from_txt_entries` decodes raw TXT data
without imposing the address filter, for raw WebSocket serialization.

Ordinary matcher actions retain the provider key domain `zeroconf`, tuple
identity `(service_type, instance_name)`, and version `1`; the target
integration domain stays a separate action field.

## Wire ownership and checks

`MdnsWireOwner` owns the actual daemon thread, browser receivers, resolver
mirror, and advertisement monitor. `stop_browsing` cancels and joins browser
jobs. `shutdown` waits for the daemon's shutdown acknowledgement and joins its
OS thread. Its weak backend and manager borrowers cannot close or retain it.
An empty provided selection starts an idle daemon without UDP sockets.
Production uses port 5353; fixtures use a custom port and explicit loopback
interfaces. Dropping an owner requests cleanup through a fallback reaper;
explicit async shutdown is the path that reports errors and completes the join.

Pure policy tests use an in-memory backend. Wire tests send actual DNS-SD
records over loopback, checking TXT bytes, cache order, rename, goodbye,
cancellation and socket/thread release. No fixture joins a production
interface or uses the system Bonjour port. Run package checks from the root
workspace with `cargo test -p hass-zeroconf --locked`.
