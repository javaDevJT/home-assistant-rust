# USB native port contract

## Baseline and scope

This contract is based on Home Assistant commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The integration discovers USB devices exposed as serial ports. It does not enumerate arbitrary USB HID devices or other USB endpoints.

The integration's public models and helper functions are in `homeassistant/components/usb/{__init__.py,models.py,utils.py}`. The integration manifest declares `aiousbwatcher==1.1.2` and `serialx==1.15.1` (`manifest.json`).

## Public data contract

`SerialDevice` is an immutable, keyword-only record with `device` (path or URL), optional `resolved_device`, `serial_number`, `manufacturer`, `description`, `interface_description`, and `interface_num`. `USBDevice` extends it with required string `vid` and `pid`, plus optional integer `bcd_device`. Device records are structurally equal and hashable; the current change cache compares every field, including the device path and metadata.

`UsbServiceInfo`, passed to integration config flows, contains only `device`, `vid`, `pid`, `serial_number`, `manufacturer`, and `description`. It omits the resolved path, BCD revision, and interface metadata. `SerialPortConsumer` reports `kind` (`config_entry` or `app`), `title`, `active`, and optional `domain`, `config_entry_id`, and `slug`.

The existing integration unique-ID helper returns the exact string `"{vid}:{pid}_{serial_number}_{manufacturer}_{description}"`. It does not include the path, BCD revision, or interface fields, and does not normalize case or escape separators. Preserve this output for compatibility with current integration config flows. It can collide when identifying fields are absent or shared, so test that case explicitly before treating the helper output as a globally unique hardware identifier.

## Enumeration and port providers

The base scanner calls `serialx.list_serial_ports()` on an executor and converts each `SerialPortInfo` to `USBDevice` only when both VID and PID are available; otherwise it returns `SerialDevice`. VID/PID are four-digit uppercase hexadecimal strings. Serial number, manufacturer, description, BCD revision, interface description, interface number, device path, and resolved path are copied from the provider. USB config-flow discovery processes only `USBDevice`; the serial-port listing API returns both record types.

Integrations may register synchronous serial-port scanners. Their results are merged by exact `device` string after base enumeration: later registered scanners replace earlier/base entries at the same path, allowing a scanner to enrich metadata. A scanner exception is logged and isolated; later scanners and the base scan remain usable. `async_notify_serial_ports_changed` schedules a delayed rescan after a scanner's contributed ports change. Registration returns an unregister callback.

The hotplug source is `AIOUSBWatcher`. The code registers its callback, starts the watcher, and retains its cancel function for Home Assistant stop. Only `InotifyNotAvailableError` selects the five-second filesystem-polling fallback. Other watcher startup failures propagate from setup; they are not silently converted to polling. Polling is documented as a development fallback. Watcher changes coalesce through a five-second add/remove debounce. A successful Home Assistant started event triggers the initial full serial scan. Stop cancels the watcher or polling interval and shuts down the explicit-scan debouncer.

Manual scan requests use an immediate debouncer with a ten-second cooldown. `usb/scan` does not force a scan while the hotplug observer is active; when there is no active observer, it requests the debounced scan. A requested scan invokes registered scan-request callbacks before serial enumeration; the Home Assistant started initial scan calls the serial scan directly. The initial-scan flag is set after the first successful serial scan, including an empty scan. Pending initial-scan callbacks are then invoked in reverse registration order; callbacks registered after that point run immediately. Current initial-scan callback exceptions are not isolated, so a failing callback can prevent later callbacks from running. Port-event callbacks receive sets of added and removed `USBDevice` records only. A failing port-event callback is logged without preventing the others from running.

The state cache is a set of complete `USBDevice` values. Each scan computes set differences, updates the cache, sends port events, then aborts matching removed-device flows and starts flows for matching added devices. A metadata or path change therefore appears as one removal plus one addition. Non-USB serial ports never enter this discovery diff. On macOS, when a serial number has a `/dev/cu.SLAB_USBtoUART...` path, the corresponding `/dev/cu.usbserial-...` duplicate is discarded. The current stop handler shuts down the explicit-request debouncer; the delayed add/remove debouncer has no explicit shutdown there, so the native owner must guard pending work against shutdown and stale watcher callbacks.

`async_find_moved_serial_port` recognizes a missing configured `/dev/serial/by-id/<name>` path or a proxy URL with `port_udev_id`. It returns a replacement only when exactly one scanned device has that ID and `serialx.is_non_unique_serial` says its serial number is unique. `usb_device_from_path` resolves real paths and looks up only USB serial devices. Do not use transient `/dev/tty*` paths as a persisted discovery identity.

## Matcher data and behavior

`homeassistant/generated/usb.py` is generated from core integration manifests by `script/hassfest/usb.py`. Core domains are visited in sorted order; each manifest's USB entries retain their listed order; the `known_devices` field is stripped from each generated matcher. The generated `USB` list is the core baseline. `loader.async_get_usb()` copies it and appends USB matchers from custom components, again dropping `known_devices`. A native port must keep these sources and ordering semantics; do not hand-maintain a second matcher inventory.

A matcher has required `domain` and optional string fields `vid`, `pid`, `serial_number`, `manufacturer`, and `description` (`homeassistant/loader.py`, `USBMatcher`). All supplied fields must match. VID and PID compare as exact, case-sensitive strings. Since enumerated VID/PID are uppercase hexadecimal, generated values must retain that convention. The three text fields use `fnmatch` wildcard matching, and the implementation lowercases the observed value but does **not** lowercase the matcher pattern; generated text patterns are lowercase. Missing observed text never matches a supplied pattern.

The matcher pass keeps only matches with the greatest dictionary length (including the always-present `domain` field). Sorting is stable, and every tied most-specific matcher is returned; ties can intentionally start flows in multiple domains. Less-specific matches are discarded when a more-specific match exists. Preserve this rather than choosing the first matching entry or returning every match.

`SERIAL_PORT_SELECTOR_INTEGRATIONS` is a separate generated list. Hassfest identifies core config flows that call `SerialPortSelector`; it is not the USB hardware matcher list. The WebSocket query filters core domains that are overridden by custom components.

## Discovery flow identity and rediscovery

For each newly added matching `USBDevice`, the current code builds `UsbServiceInfo` and calls the shared discovery-flow helper with the matched integration domain and USB source (`config_entries.SOURCE_USB`). Removal rebuilds the same service info and aborts only in-progress flows whose typed init data is exactly equal and whose handler is one of that device's most-specific matching domains. Removing a device does not unload an already configured integration entry.

The pinned USB implementation does not pass a `DiscoveryKey`; consequently USB discovery does not persist a rediscovery key on the resulting config entry. The native port must preserve that absence. The shared native flow boundary needs a no-key admission variant rather than inventing persistence or reload behavior:

```text
DiscoveryHandle(
  domain = matcher.domain,
  source = "usb",
  key = None,
  context = {source: "usb"},
  data = service_info,
)
```

The existing unique-ID helper is a consumer API, not the scanner's persistent key. Preserve its source calculation and its possible collisions when serial/metadata are absent. Do not use it to collapse different path-specific device records. Config-entry reload/removal does not imply automatic keyed USB rediscovery in this source; add no such behavior. Explicit scans and hotplug changes use a fresh provider snapshot. The shared discovery manager still owns startup gating and the twenty-flow admission limit. Keep the watcher strongly owned, use weak callback handles and exact lifecycle guards, and join provider work on stop.

The shared flow helper suppresses duplicate in-progress flows by handler, context and initial data. `UsbServiceInfo.device` remains path-specific, so a moved port can produce different init data despite an unchanged consumer unique ID. Removal/addition and moved-path tests must cover that distinction. Config entries created by this source must not acquire a `discovery_keys["usb"]` value merely because the native helper supports keys for other protocols.

## Configuration, API, and consumer surfaces

The integration YAML schema is empty (`usb: {}`); it has no USB-specific options or service registration. Its manifest depends on `websocket_api` and is `local_push`.

All three registered WebSocket commands require admin access:

| Command | Request and result |
| --- | --- |
| `usb/scan` | Requires only `type`; asks for a scan subject to observer/debounce behavior and returns an empty successful result. |
| `usb/list_serial_ports` | Optional `include_usage` boolean defaults to `false`. Lists scanned USB and non-USB serial ports, each with device/resolved path, serial/manufacturer/description/interface metadata, matching integrations, and `present`. USB records add VID, PID, and BCD revision. With usage enabled, also returns consumers and in-progress flows. An `OSError` becomes `unknown_error` with the original message. |
| `usb/list_serial_integrations` | Requires only `type`; returns the generated, sorted core list of config flows using `SerialPortSelector`, excluding domains overridden by custom components. |

With `include_usage`, config-entry consumers are inferred from literal configured key paths in entry data/options (such as `device`, `port`, `serial_port`, `socket_path`, and `usb_path`) for integrations that depend on USB or are known serial integrations. Paths are resolved to correlate symlinks. App consumers are inferred recursively from `/dev/...` values in app options when Supervisor is available; static manifest device mappings are not evidence of use. Missing configured ports remain listed. `esphome://`, `rfc2217://`, `socket://`, and `tcp://` ports are considered present while claimed because no scanner can enumerate them; `esphome-hass://` ports are scanner-backed and may be absent. The Z-Wave ESPHome URL's `?key=` value is stripped before reporting so its PSK is not exposed. Preserve these semantics and the `SerialPortConsumer` result fields.

## Dependency boundary and source-audit follow-up

The baseline contract above comes from Home Assistant call sites and pinned tests. The pinned dependency source audit is recorded below; package code was inspected as archive text only and was not imported, executed, installed, or tested. A native equivalent still requires its own source and lifecycle review before selection:

| Pinned dependency | Home Assistant boundary |
| --- | --- |
| `serialx==1.15.1` | `list_serial_ports`, `SerialPortInfo` field meanings, path/resolved-path and symlink behavior, VID/PID extraction, `is_non_unique_serial`, and custom serial transport registration. |
| `aiousbwatcher==1.1.2` | watcher startup, inotify failure modes, callback threading/loop guarantees, filesystem coverage, and cancellation/join guarantees. |

These versions are exact manifest pins. No replacement Rust crate or provider is recommended by this contract; verify a candidate's source and lifecycle behavior separately before selection.

## Verified pinned dependency source audit
The repository's `docs/upstream-parity.json` USB record identifies `homeassistant/components/usb/manifest.json` at blob `631189599c0ba9f73aba65cdb24e9be6976303a8` and records both requirements below. The original Home Assistant manifest file is not present in this porting worktree; the parity snapshot is the local pin evidence. Each exact PyPI release JSON supplied its source archive URL and published SHA256. The downloaded archive's SHA256 was computed locally and matched that metadata.

| Pin | Primary PyPI release JSON | Source archive | Local audit archive | Verified SHA256 |
| --- | --- | --- | --- | --- |
| `serialx==1.15.1` | [release JSON](https://pypi.org/pypi/serialx/1.15.1/json) | [sdist](https://files.pythonhosted.org/packages/93/80/20ddfe87f1f62f604112def5bfc65673bd6013496201a6dfd8cc56f40366/serialx-1.15.1.tar.gz) | `/tmp/usb-engine-source-audit-2026-11-07/serialx-1.15.1.tar.gz` | `689dfbaf55f5f87ea0b6bcdcab135577ce1e70f98d490650b02a8b86eb0b0281` |
| `aiousbwatcher==1.1.2` | [release JSON](https://pypi.org/pypi/aiousbwatcher/1.1.2/json) | [sdist](https://files.pythonhosted.org/packages/3a/fc/b358b2ed4115d0c93f44e737191517bc0973f9d101ad20f03f5b6cf77e17/aiousbwatcher-1.1.2.tar.gz) | `/tmp/usb-engine-source-audit-2026-11-07/aiousbwatcher-1.1.2.tar.gz` | `908919fed3b5d6b5b0a26f7aea82a5f45c6a6205789dcb39348abc34a4f06ebe` |

The serialx archive members inspected were `serialx/common.py`, `serialx/platforms/serial_linux.py`, `serialx/platforms/serial_darwin.py`, `serialx/udev.py`, `serialx/serialx_rust.py`, `serialx/platforms/serial_esphome.py`, `serialx/platforms/serial_socket.py`, `serialx/platforms/serial_rfc2217/__init__.py`, `Cargo.toml`, `setup.py`, `pyproject.toml`, `src/lib.rs`, `src/darwin.rs`, and the source tests for listing, by-id stems, and Linux fixtures. The aiousbwatcher members inspected were `src/aiousbwatcher/impl.py`, `src/aiousbwatcher/__init__.py`, `pyproject.toml`, `README.md`, and `tests/test_impl.py`.

### `serialx==1.15.1`: enumeration and path contract
`SerialPortInfo` is a dataclass with string `device` and `resolved_device`; optional integer `vid`, `pid`, `bcd_device`, `interface_num`, and `port_number`; and optional string `serial_number`, `manufacturer`, `product`, and `interface_description`. Its deprecated `description` property returns `<product> - <interface_description>` when the interface description exists, using the literal `None` for a missing product; otherwise it returns `product`, then the basename of `resolved_device`. `is_non_unique_serial` treats `None`, whitespace-only values, and the package's known placeholder serial list as non-unique. Home Assistant formats the integer VID/PID as four-digit uppercase hex and copies serial, manufacturer, computed description, BCD/interface fields, and paths into its records; it does not expose `product` or `port_number`.

On Linux, `list_serial_ports()` dispatches to the physical `device://` backend. It walks `/sys/class/tty`, resolves each `/sys/class/tty/<tty>/device`, and recognizes `usb-serial` and CDC ACM (`usb`) subsystems. It reads VID, PID, BCD revision and interface number from sysfs; usb-serial entries also read `port_number`. Missing optional sysfs text becomes `None`; a disappearing required device is skipped. Native serial subsystems (`serial-base`, `platform`, `pnp`, `amba`) are included only when their `type` is not `PORT_UNKNOWN`, with USB metadata unset. A path under `/dev/serial/by-id` is preferred when its symlink resolves to the enumerated `/dev/<tty>` node; otherwise `device` is `/dev/<tty>`. `resolved_device` is the `/dev/<tty>` path. Directory enumeration errors are tolerated, while unknown subsystems are logged and skipped.

The package's `udev_serial_by_id_stem()` reproduces Linux by-id naming from `manufacturer` and `product` (falling back to VID/PID), requires an interface number, sanitizes descriptor strings, and appends a serial only when its characters satisfy the udev rule. It adds `-portN` only when `port_number` is present, matching the distinction between vendor usb-serial drivers and class drivers such as CDC ACM. This stable link is not guaranteed to exist; keep the configured by-id identity and resolved device path distinct. The Home Assistant moved-port helper separately accepts a by-id basename or a proxy URL's `port_udev_id`, and accepts a replacement only when exactly one enumerated candidate matches and its serial is not considered non-unique by `is_non_unique_serial`.

On macOS, `darwin_list_serial_ports()` maps the optional Rust extension's records to `SerialPortInfo` and sets `resolved_device == device`. The extension matches IOKit `IOSerialBSDClient` services, reads `IOCalloutDevice`, then searches the service's registry parents for USB descriptors (`idVendor`, `idProduct`, serial, manufacturer, product, BCD revision and interface properties). Non-USB serial services are retained with USB fields unset. This is point-in-time port enumeration, not an IOKit hotplug observer. The extension is optional at package build/import time; when it cannot load, serialx logs a warning and returns an empty list, so no POSIX fallback supplies the missing macOS enumeration.

The exact serialx source uses an optional PyO3 extension on macOS and Windows. Its manifest declares `pyo3 >=0.23,<0.30` with `extension-module` and `abi3-py310`; the macOS target uses `core-foundation 0.10`, `core-foundation-sys 0.8`, and `io-kit-sys 0.4`. The Windows target uses `serialport 4` with the `usbportinfo-interface` feature. The Linux enumerator in this pin is the Python sysfs implementation above; the package does not contain a Linux Rust USB enumerator. These versions describe the audited upstream build only and are not recommendations for a replacement crate.

Serial URL transports are a separate registration path from the default physical-device scan. `socket://`, `tcp://`, and `rfc2217://` register transport classes without enumeration callbacks. `esphome://` has an API-backed list operation only when explicitly selected; default `list_serial_ports()` dispatches through the host's `device://` handler. A URL in Home Assistant's serial records can therefore be supplied by an integration scanner or claimed-port logic and does not imply USB hardware metadata. Preserve the existing USB source's `DiscoveryKey` absence and do not add automatic keyed reload rediscovery.

### `aiousbwatcher==1.1.2`: watcher lifecycle and fallback contract
`AIOUSBWatcher` captures the running asyncio loop at construction and monitors `/dev/bus/usb`. Importing `asyncinotify` and its `_ffi` library is wrapped in a broad exception handler; any import-time exception is saved. `async_start()` raises `InotifyNotAvailableError` synchronously when that saved failure exists, rejects a second active start with `RuntimeError`, otherwise stores one asyncio task and returns a synchronous cancel callable immediately. There is no awaitable readiness barrier, and the class itself never emits an initial snapshot or initial callback. The watcher owns its callback set and returns an unregister closure; that closure removes the callback with `set.remove`, so repeated unregister is not idempotent. Home Assistant's successful `EVENT_HOMEASSISTANT_STARTED` path performs that initial serial scan separately.

The watcher recursively discovers directories using the loop's default executor, registers inotify watches for create, move, delete, delete-self, and ignored events, and adds watches beneath newly created directories. It invokes zero-argument callbacks synchronously on the watcher task's loop for any matching event; it does not pass device details. Each callback exception is caught and logged so other callbacks still run. Directory walking runs in an executor; callback work runs on the asyncio loop.

The returned stop callable cancels the task and clears the watcher's `_task` field. It does not await task completion or join it; the inotify context manager closes during coroutine cancellation/unwind. Errors from opening or consuming inotify occur inside the created task and are not converted into `InotifyNotAvailableError` by this class. The package has no separate Darwin observer. Home Assistant's documented five-second polling fallback is selected only for the synchronous `InotifyNotAvailableError`; other synchronous startup failures propagate, and task failures are not a polling signal. The watcher library itself does not debounce changes; Home Assistant applies the separate five-second add/remove debounce.

The package source tests replace its module `_PATH` with a temporary directory, then exercise callbacks, nested directory watches, callback exception isolation, duplicate-start rejection, unregister, and cancellation. This confirms the upstream fixture pattern without touching the real USB bus; these tests were read, not run.

### Safe native fixture ideas and audit limits
Use temporary synthetic filesystem roots for Linux `/sys/class/tty` and `/dev/serial/by-id` trees, with fake symlinks and descriptor files for USB-serial, CDC ACM, missing/partial descriptors, native serial ports, and disappearing entries. Keep a Darwin IOKit adapter behind an injectable record source so tests can supply synthetic `IOSerialBSDClient`/USB property rows without querying the host registry. Exercise watcher directory changes against a temporary root or synthetic event source, and use only an owned PTY pair to validate serial transport I/O; PTYs do not prove USB descriptors or hotplug behavior. Snapshot-provider tests can inject `SerialPortInfo` values and assert USB versus non-USB conversion without opening device paths.

This was a static audit of the two hash-verified sdists and Home Assistant's recorded manifest pin; no dependency was installed, imported, executed, or tested, and no USB/serial device or OS watcher was opened. The transitive `asyncinotify>=4.2.2` requirement is not separately pinned or audited here. PyPI metadata and the exact sdists were checked, but no published wheels or host-specific runtime behavior were inspected. No Rust replacement crate is selected or validated. The USB flow identity above remains source `usb`, `key=None`; config-entry reload/removal does not trigger keyed USB rediscovery in the audited Home Assistant source.

## Native implementation

The executable boundary uses `hass-usb` for matching, path-specific full-record
diffs, scanner replacement, source callbacks and debounce; `hass-usb-backend`
provides Linux sysfs/by-id and Darwin IOKit enumeration. Only USB records enter
discovery, while fresh port listings retain non-USB serial records. The 32
generated matchers, 19 serial-selector integration domains and 39 eligible
core consumer domains have separate meanings; none proves those integrations
are implemented. Serial uniqueness uses the pinned serialx placeholder set.

The app strongly owns the SDK, native watcher, shared blocking timer scheduler
and five-second fallback poll. Watcher startup never performs an initial scan;
the actual started boundary runs that scan. Only `InotifyNotAvailable` selects
fallback. Stop closes admission, cancels pending work and joins admitted
request callbacks, scans, diagnostics, native watcher and scheduler jobs.
Synchronous callbacks must terminate. Provider-only path lookup excludes
scanner contributions, while manual/delayed scans retain visible isolated
scanner and callback error reports. The non-strict path resolver follows
dangling by-id links and retains missing tails; Linux enumeration preserves
aliases during device-node removal races.

USB actions share the app's twenty-flow discovery admission queue and carry no
persisted discovery key. A private Rust init-data marker distinguishes native
USB observations from arbitrary JSON that names source `usb`. That marker
survives form steps, participates in duplicate matching and removal, and cannot
be supplied through JSON context. Removal aborts only typed, exact original
service data in the most-specific matching domains, including protected flows.
Ordinary SSDP/Zeroconf dismissal protection remains in effect.

The three admin WebSocket commands are request-response operations. Fresh
listing runs on the blocking pool and does not update discovery state. Usage
comes from actual config entries and typed active flows, preserving source
eligibility, active states, path order, aliases, missing ports and URL query
redaction. Neither static manifests nor scanned devices stand in for actual
configured consumers. `usb:` presence enables setup; unsupported parameters
are ignored with the source warning, and `--check` starts no providers.

Focused checks use injected records/watchers/schedulers, real temporary SQLite
stores and symlink trees, and owned loopback WebSockets. Linux-only watcher
fixtures use a temporary tree. No physical serial device, live USB tree,
IOKit inventory or Supervisor service is exercised. Windows enumeration,
serial-proxy transport registration, runtime custom-component loading,
Supervisor app consumers and full pinned upstream acceptance remain open.
The dependency audit above is source evidence; it does not claim those open
surfaces or live hardware verification.

## Acceptance falsifiers

Use the pinned tests as the initial behavioral oracle; this document review did not run Home Assistant, dependency code, or tests.

| Surface | Existing coverage | Falsify the port if… |
| --- | --- | --- |
| Startup, hotplug, fallback, debounce | `tests/components/usb/test_init.py`: `test_aiousbwatcher_discovery`, `test_notify_serial_ports_changed_with_aiousbwatcher`, `test_polling_discovery`, `test_aiousbwatcher_on_wsl_fallback_without_throwing_exception`, `test_discovered_by_aiousbwatcher_before_started` | An initial snapshot is missed; repeated changes create repeated flows; the five-second delay/fallback distinction is lost; or a stopped watcher can still dispatch. |
| Matcher rules | `test_most_targeted_matcher_wins`, description/manufacturer/serial-number positive and negative cases, `test_discovered_by_websocket_scan_match_vid_only`, `test_discovered_by_websocket_scan_match_vid_wrong_pid`, `test_discovered_by_websocket_scan_rejected_with_empty_serial_number_only` | Matching becomes case-insensitive for VID/PID, case-sensitive for text values, ignores a supplied field, accepts a missing required text field, chooses a less-specific matcher, or drops a tied most-specific domain. |
| Scanner merge and cache | `test_async_scan_serial_ports`, `test_async_scan_serial_ports_with_scanner`, `test_async_scan_serial_ports_scanner_raises`, `test_register_port_event_callback`, `test_register_port_event_callback_failure`, `test_cp2102n_ordering_on_macos` | A contributed path cannot replace base metadata, one scanner exception aborts other providers, non-USB serial ports enter discovery, callback errors abort the scan, or the macOS duplicate survives. |
| Removal, moved path, identity | `test_removal_aborts_discovery_flows`, `test_find_moved_serial_port`, `test_serial_path_udev_id` | A removal aborts a flow for another device/domain, a non-unique moved-path candidate is accepted, a device path alone becomes consumer identity, or a USB discovery key is invented. |
| WebSocket/admin/API | `test_list_serial_ports`, `test_ws_commands_require_admin`, `test_list_serial_ports_os_error`, `test_list_serial_integrations`, `test_list_serial_integrations_custom_override` | Any route loses admin gating, required fields/presence/matcher order change, OS errors become silent empty results, or a custom override remains in the core selector list. |
| Usage and privacy | `tests/components/usb/test_consumers.py`: config-entry consumers, app consumers, aliases, missing ports, active states, ignored entries, and `test_socket_path_psk_not_exposed` | Static app mappings are misreported as use, configured missing ports disappear, symlink aliases split consumers, or a URL secret is returned. |
| Generated data | `script/hassfest/usb.py`, `tests/script/hassfest/test_usb.py` | Core ordering, per-manifest matcher ordering, `known_devices` filtering, or selector detection diverges from hassfest output. |
