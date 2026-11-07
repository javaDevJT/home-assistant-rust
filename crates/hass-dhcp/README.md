# Native DHCP discovery SDK

`DhcpIntegration` strongly owns the observation cache, matcher index and update
listeners. `DhcpManager` and exact-ID `DhcpUpdateGuard` registrations are weak;
they cannot keep the owner or a registered-device provider alive. Providers,
packet capture, scanning, runtime events and the API are separate adapters.

The cache preserves first-seen MAC order and stores compressed IP text with
the original hostname. An ordinary observation is ignored when its compressed
IP matches and the old hostname starts with the incoming hostname. Force replay
bypasses that test. Discovery data instead retains the incoming IP text, a
lowercase hostname and canonical colonless MAC, serialized as `macaddress`.
Invalid, loopback, link-local and unspecified IPs are ignored. The MAC formatter
preserves the pinned source's permissive recognized-length behavior.

The bundled metadata contains exactly 386 matchers across 119 target domains,
including 43 registered-device rules. Presence of `registered_devices`, even
when false, selects that branch. Registered-device domains are intersected
with the injected provider's results; ordinary hostname/OUI buckets preserve
the source's ordering and case-sensitive patterns. This metadata does not
establish acceptance of those device integrations.

Accepted cache writes happen before update callbacks and flow admission.
Callbacks, registered-device lookups and sinks run outside the cache lock and
may reenter the SDK. A sink failure is returned after that cache write and
notification; it stops that ingest's remaining admissions. An already accepted
ingest may finish its cloned callbacks after concurrent owner drop. App owners
must stop and join their producers before dropping this SDK; new calls through
a weak manager fail after the owner closes.

Discovery actions keep the target domain separate from source `dhcp` and the
version-1 string key in provider domain `dhcp`. Removing an accepted key replays
the existing cache row through the same matcher path with force enabled. No
cache TTL, synthetic removal, public runtime event, service or HTTP route is
introduced.

Run the focused tests from the root workspace with
`cargo test -p hass-dhcp --locked --offline`. They use deterministic observations
and injected registry/sink callbacks without packet capture or network traffic.
The [source contract](../../docs/DHCP-PORT.md) records provider boundaries and
remaining upstream acceptance work.
