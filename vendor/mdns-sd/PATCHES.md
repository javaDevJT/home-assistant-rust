# Vendored source and local patches

Upstream project: <https://github.com/keepsimple1/mdns-sd>

Release: `mdns-sd` 0.21.7.

Primary registry archive: <https://static.crates.io/crates/mdns-sd/mdns-sd-0.21.7.crate>

SHA-256: `e6743ed743ff2bada2b8651ef298d3f98addb0b74844f19cdd57137d8bd0219e`

The archive digest was checked against the `cksum` for version 0.21.7 in the
official crates.io sparse index at <https://index.crates.io/md/ns/mdns-sd>.
Upstream license files (`LICENSE-APACHE` and `LICENSE-MIT`) are retained.

## Local patch rationale

- Two upstream trailing-whitespace occurrences in `CHANGELOG.md` and a
  `src/dns_parser.rs` documentation comment are trimmed for repository checks.
- `Cargo.toml` adds an empty `[workspace]` table so this vendored crate stays
  outside the repository's root Cargo workspace until the root explicitly
  enrolls it. `Cargo.toml.orig` and the crate's `Cargo.lock` are unmodified.
- `src/lib.rs` and `src/service_daemon.rs` add
  `ServiceDaemon::new_with_interfaces`, which accepts a complete provided
  interface snapshot and UDP port before socket setup, and returns the daemon
  with its real `std::thread::JoinHandle`. `NetworkInterface` and its address
  types are re-exported so callers can build the snapshot without a separate
  `if-addrs` dependency. Explicit mode binds only the address families in that
  snapshot, joins only its interfaces, and reuses the snapshot for rescans and
  interface operations without enumerating the host. Existing constructors
  keep their original behavior. The upstream wildcard socket per selected
  family and packet-info receive filtering remain intact.
- `src/service_info.rs` and `src/service_daemon.rs` add raw TXT entries to the
  cache-derived `ResolvedService` payload. The new getter preserves original
  key bytes, raw values, wire order, case variants, duplicates, and valueless
  versus empty values; the existing `TxtProperties` view is unchanged. The
  resolver also exposes the scoped-address order already retained by the cache
  (new records are inserted first), while keeping the existing address set.
  The additive fields make external `ResolvedService` struct literals
  incompatible; this project's supported consumers obtain records through the
  daemon API. This local fork does not claim unmodified upstream source API
  compatibility for such literals.
- Focused tests cover explicit loopback/custom-port startup, owned shutdown and
  port release, snapshot-only rescans, raw TXT wire fidelity, resolved-event
  cache fidelity, and cached address order.
