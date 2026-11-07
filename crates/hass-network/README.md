# Native Network

`NetworkIntegration` owns adapter selection and the persisted `core.network`
preferences. `NetworkManager` is a weak read/mutation handle. The pinned source
and wire contract are in [NETWORK-PORT.md](../../docs/NETWORK-PORT.md).

Start with a Runtime, an `AdapterEnumerator` and a `RouteSourceResolver`.
Production uses `SystemAdapterEnumerator` and `SystemRouteSourceResolver`.
Tests inject both providers; enumeration and route callbacks run outside SDK
locks. The route resolver connects a UDP socket and reads its local address,
without sending a datagram or resolving a hostname.

Manager APIs expose adapter snapshots, configured names, configuration updates,
reload, all enabled source IPs and one selected IPv4 source. Selection compares
exact adapter names. Any surviving configured name selects only those matches;
empty or entirely unknown selections use the source auto/default rules. Saved
names retain unknown values and duplicates. Generic enabled-source output keeps
address order, duplicates and IPv6 zones; each discovery protocol applies its
own filter afterward.

`configure` writes the normal version-one Store envelope before publishing the
new snapshot. A failed write preserves the previous state. Reload enumerates
outside locks, then applies the latest committed names. Owner shutdown shares
commit serialization and deactivates the handle before subsequent mutations.
There is no invented Runtime event, service, periodic interface poll or
configuration-entry flow.

The app strongly owns Network, exposes the three source WebSocket commands and
passes selected addresses to SSDP. Preference changes update the Network
snapshot; discovery sockets consume selection at their startup. The HTTP
proxy/local-origin `hass-api::NetworkPolicy` serves a separate purpose.

Fourteen SDK cases pass, including MemoryStore/SQLite restart, atomic failure,
reentry and shutdown. Four loopback API cases cover roles, fields, selection,
URLs and weak teardown. A compiled-process case exercises the actual OS provider
and SQLite preference/URL restart over loopback; it sends no discovery traffic.

Platform limits remain explicit: `if-addrs` exposes OS names rather than every
platform's friendly-name variant, supplies no IPv6 flowinfo field, and requires
link-local zones to be inferred from the interface index. Its flowinfo projection
uses zero. Literal IPv4 route targets are supported; hostname resolution,
Supervisor-triggered reload/debounce, frontend settings and exhaustive pinned
acceptance remain open.
