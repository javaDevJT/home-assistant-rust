# Hardware integration source contract

## Native work in progress

`hass-hardware` implements typed records, a guarded loaded-provider registry,
one private delayed metrics sampler, bounded subscriptions and joined
shutdown. Its 16 synthetic cases and strict Clippy pass. The app's API adapter
passes eight loopback cases, including non-admin access, shared sampling,
unsubscribe, overflow and authorization revalidation. No CPU, memory or
hardware inventory was read by these checks.

The selected provider boundary is direct Linux proc/Mac Mach metrics using
the exact psutil 7.2.2 formulas below. `hass-hardware-metrics` is enrolled with
15 fixture cases and strict Clippy passing. The actual compiled app's loaded
provider list and SIGTERM teardown pass without subscribing to host metrics.
Percentage rounding uses the exact binary input before decimal quantization:
CPU deltas 69/2000 produce 3.5%, then 4 after HA's integer rounding; memory
51/2000 produces 2.5%. A pure built-in Python oracle agrees with the native
helper for 17,007 percentage values, decimal ties and adjacent f64 values.
No HA or dependency package was imported or executed for that comparison.

The final 39-crate workspace passes 970 tests across 110 targets with zero
failures or ignored cases. Formatting, strict locked offline workspace
all-target Clippy, optimized build and no-side-effect CLI checks pass.
Physical collection and full pinned platform acceptance remain open. Source
`4d875e379b9a91d2adcbcec5b2e3279e55dbe48a` is published and passed
[exact native Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42385397159);
this is partial coverage.


Source audited at Home Assistant Core `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` with `git show`; no HA Python or installed dependency was imported or executed. This records source behavior, not parity acceptance.

## Integration and WebSocket contract

- `hardware` has an empty configuration schema and no config flow. Setup creates `HardwareData`, lazily discovers loaded `hardware` platforms, constructs one `PsutilWrapper` in the executor, registers two WebSocket commands, and succeeds (`homeassistant/components/hardware/__init__.py:27-53`).
- `hardware/info` returns `{ "hardware": [...] }`. It asks the lazy platform registry for all providers belonging to loaded integrations; a provider with synchronous `async_info(hass)` contributes its `HardwareInfo` records serialized with dataclass field names. A provider's `HomeAssistantError` is suppressed, so its records disappear and the WebSocket request still succeeds (`homeassistant/components/hardware/websocket_api.py:25-43`).
- The only integration-specific schema field is required `type` (the standard base WS message schema is added by the decorator); `hardware/info` is wrapped with `async_response`, while system-status subscribe is a callback handler (`homeassistant/components/hardware/websocket_api.py:25-33,46-54`; decorator schema at `homeassistant/components/websocket_api/decorators.py:134-165`). Neither handler adds `require_admin`; that restriction is explicit when used (`homeassistant/components/websocket_api/decorators.py:54-69`), so this source has no hardware-specific admin gate. WebSocket authentication remains the connection layer's responsibility.
- Platform lookup is lazy and cached per loaded integration; integrations that are not loaded are not queried, missing/import-failed platforms resolve to `None`, and a platform processing exception is logged and cached as absent (`homeassistant/helpers/integration_platform.py:272-380`). The helper tests cover not-loaded, missing, multi-provider and cached lookup (`tests/helpers/test_integration_platform.py:320-388`), import/processing failure (`:391-435`), and concurrent callers sharing one import (`:438-472`).
- Public records have these fields: `BoardInfo(hassio_board_id, manufacturer, model, revision)`, `USBInfo(vid, pid, serial_number, manufacturer, description)`, and `HardwareInfo(name, board, config_entries, dongle, url)`. Nullable fields are part of the serialized shape and should remain present as `null` (`homeassistant/components/hardware/models.py:30-60`; baseline response in `tests/components/hardware/test_websocket_api.py:17-30`).

## Provider and data-source contracts

All current board providers call `hassio.get_os_info`; they do not discover board identity from local sysfs themselves. That helper returns Supervisor's cached `DATA_OS_INFO.to_dict()` and raises `HassioNotReadyError` when no data is available (`homeassistant/components/hassio/coordinator.py:1074-1083`; error is a `HomeAssistantError` at `homeassistant/components/hassio/exceptions.py:6-7`). Provider manifests order after `hassio` and depend on `hardware` (for example `homeassistant/components/homeassistant_green/manifest.json:1-10` and `homeassistant/components/hardkernel/manifest.json:1-10`).

| Provider | Acceptance rule and output source |
| --- | --- |
| Hardkernel | Supervisor `board` must start with `odroid`; board ID/model are that value, manufacturer is `hardkernel`, and the display name uses a known-name map or an explicit unknown-model string (`homeassistant/components/hardkernel/hardware.py:10-47`). |
| Raspberry Pi | `board` must start with `rpi`; ID is the Supervisor value, model/name use maps (unknown model/name remain descriptive), manufacturer is `raspberry_pi` (`homeassistant/components/raspberry_pi/hardware.py:10-62`). |
| Home Assistant Green / Yellow | `board` must equal `green` / `yellow`; fixed manufacturer, model, name, documentation URL, and Supervisor board ID are returned (`homeassistant/components/homeassistant_green/hardware.py:10-43`; `homeassistant/components/homeassistant_yellow/hardware.py:10-43`). |
| SkyConnect | Each matching-version config entry supplies its stored VID/PID/serial/manufacturer/product; the variant name is derived from the entry. It calls `async_entries(DOMAIN)` without filters, so ConfigEntries defaults include ignored and disabled entries; the expected-version check is the only explicit row filter. No USB scan is performed here (`homeassistant/components/homeassistant_sky_connect/hardware.py:11-38`; defaults `homeassistant/config_entries.py:2329-2349`). |
| Connect ZBT-2 | Non-ignored, enabled, expected-version config entries supply stored VID/PID/serial/manufacturer/product. The version guard excludes unmigrated entries. No USB scan is performed here (`homeassistant/components/homeassistant_connect_zbt2/hardware.py:13-42`). |

Board records attach config-entry IDs for their provider domain (`homeassistant/components/hardkernel/hardware.py:30-42`, `homeassistant/components/raspberry_pi/hardware.py:45-57`, Green/Yellow `homeassistant/components/homeassistant_green/hardware.py:26-39` and `homeassistant/components/homeassistant_yellow/hardware.py:26-39`); these calls also use the default include-ignored/include-disabled behavior. USB records carry the owning config-entry ID. Thus USB discovery data is not itself a Hardware page row: rows describe configured integration devices, and an entry can outlive a current physical enumeration. Keep those sources distinct when joining the existing USB runtime to this API. Tests demonstrate two SkyConnect entry rows plus exclusion of a failed entry (`tests/components/homeassistant_sky_connect/test_hardware.py:49-130`) and that a ZBT-2 ignored entry with empty data does not break or add a row (`tests/components/homeassistant_connect_zbt2/test_hardware.py:71-131`).

## Supervisor absent versus test doubles

The board-provider happy-path tests set up a mocked `hassio` integration and patch `get_os_info` to return a board ID before config-entry setup and again for the WebSocket read. For example, Hardkernel does this at `tests/components/hardkernel/test_hardware.py:15-44`; Green, Yellow, and Raspberry Pi use the same pattern in `tests/components/homeassistant_green/test_hardware.py:15-47`, `tests/components/homeassistant_yellow/test_hardware.py:16-48`, and `tests/components/raspberry_pi/test_hardware.py:15-47`. These fixtures prove serialization and provider selection for supplied board data; they do not prove native board detection or a real Supervisor connection.

The provider tests separately patch `get_os_info` to raise `HassioNotReadyError`, then assert `hardware/info` succeeds with an empty list: Hardkernel `tests/components/hardkernel/test_hardware.py:103-136`, Green `tests/components/homeassistant_green/test_hardware.py:103-136`, Yellow `tests/components/homeassistant_yellow/test_hardware.py:105-139`, and Raspberry Pi `tests/components/raspberry_pi/test_hardware.py:105-139`. The base integration test also proves the valid no-provider result is `{"hardware": []}` (`tests/components/hardware/test_websocket_api.py:17-30`). In a native port, absence of a Supervisor source must stay distinct from a fabricated test board or guessed Supervisor app metadata. Source behavior for not-ready data is an omitted provider contribution, not a hardware error response.

## System-status subscription and dependency

The manifest pins `psutil-home-assistant==0.0.1` (`homeassistant/components/hardware/manifest.json:1-10`). This integration's use is narrow: instantiate `ha_psutil.PsutilWrapper` in an executor on setup (`homeassistant/components/hardware/__init__.py:40-48`), then every five seconds while at least one subscriber exists call `psutil.cpu_percent(interval=None)` and `psutil.virtual_memory()` (`homeassistant/components/hardware/websocket_api.py:59-84`). Emit `cpu_percent` rounded to an integer, `memory_used_percent` from psutil, `memory_used_mb = round((total - available) / 1024**2, 1)`, `memory_free_mb = round(available / 1024**2, 1)`, and a UTC ISO timestamp (`homeassistant/components/hardware/websocket_api.py:65-79`). The source comments that these reads touch `/proc` but do not block on Linux. First subscriber starts one shared timer; removing the last subscription cancels it (`homeassistant/components/hardware/websocket_api.py:81-97`). The published dependency implementation and Core's transitive `psutil` pin are audited below.

Existing tests require an initial subscribe acknowledgement, the exact event field names/rounding, and no further reads after unsubscribe (`tests/components/hardware/test_websocket_api.py:36-103`). The single-subscriber test does not cover shared-timer behavior for multiple subscribers; the source contract is explicit at `homeassistant/components/hardware/websocket_api.py:81-95`, so include a multi-subscriber check. A native implementation can own this command/event lifecycle and metric calculation without Supervisor, but needs an explicit OS telemetry provider rather than assuming Linux `/proc` on every target.

## Focused native acceptance boundary

After the current network, USB, SSDP, and Zeroconf runtimes (`crates/hass-api/src/network.rs`, `crates/hass-api/src/usb.rs`, `crates/hass-api/src/ssdp.rs`, `crates/hass-api/src/zeroconf.rs`; app USB owner in `crates/hass/src/usb.rs` and `crates/hass/src/usb_usage.rs`), the self-contained next boundary is the two `hardware/*` WebSocket commands, typed record serialization, lazy/config-entry-backed hardware provider registry, and subscriber-scoped system-status sampler. A source search in those crates found no `hardware/info`, `subscribe_system_status`, or hardware status provider yet. The API's empty-list/no-provider behavior and locally sampled CPU/memory status are feasible without Supervisor. The USB port may supply current enumerated devices, but it cannot by itself recreate HA's `config_entries` ownership IDs or the source's persisted dongle metadata.

Board parity is a separate native seam: pinned HA receives board identity from Supervisor `DATA_OS_INFO`, whereas this Rust service has no Supervisor bridge in this audited contract. Add an explicit host-board provider only for a named, available native source and keep unknown/unavailable distinct; do not claim the mocked provider tests validate it. Acceptance should cover (1) no provider -> empty success (`tests/components/hardware/test_websocket_api.py:17-30`), (2) multiple provider records and nullable fields serialize exactly (provider examples: `tests/components/homeassistant_sky_connect/test_hardware.py:49-130`, base fields in `tests/components/hardware/test_websocket_api.py:17-30`), (3) missing/unloaded provider excluded (`tests/helpers/test_integration_platform.py:320-388`), (4) HomeAssistantError/not-ready provider omitted while other providers still contribute (`homeassistant/components/hardware/websocket_api.py:37-43`), (5) USB rows sourced from configured entries and filtered by provider rules (`tests/components/homeassistant_connect_zbt2/test_hardware.py:71-131`; SkyConnect `tests/components/homeassistant_sky_connect/test_hardware.py:49-130`), (6) 5-second status event shape and UTC timestamp (`tests/components/hardware/test_websocket_api.py:36-89`), and (7) one timer for many subscribers, cancelled when the last unsubscribes (`homeassistant/components/hardware/websocket_api.py:81-95`; single-subscriber unsubscribe test at `tests/components/hardware/test_websocket_api.py:91-103`). Existing HA source tests are references for the covered cases; no tests were run as part of this audit.

## Published dependency engine audit

Fetched the official release JSON and source archives; hashes below match each registry-published SHA-256. Archive contents were read as text only. No dependency was imported or executed, and no CPU, memory, device, or OS-hardware readings were taken.

| Artifact | Provenance and verified digest |
| --- | --- |
| [`psutil-home-assistant` 0.0.1 PyPI JSON](https://pypi.org/pypi/psutil-home-assistant/0.0.1/json) / [sdist](https://files.pythonhosted.org/packages/01/4f/32a51f53d645044740d0513a6a029d782b35bdc51a55ea171ce85034f5b7/psutil-home-assistant-0.0.1.tar.gz) | Published SHA-256 `ebe4f3a98d76d93a3140da2823e9ef59ca50a59761fdc453b30b4407c4c1bdb8`, downloaded SHA-256 matched. Apache-2.0; author Erik Montnemery; project `home-assistant-libs/psutil-home-assistant`. |
| [`psutil` 7.2.2 PyPI JSON](https://pypi.org/pypi/psutil/7.2.2/json) / [sdist](https://files.pythonhosted.org/packages/aa/c6/d1ddf4abb55e93cebc4f2ed8b5d6dbad109ecb8d63748dd2b20ab5e57ebe/psutil-7.2.2.tar.gz) | Core pins `psutil==7.2.2` at `requirements_all.txt:2020`; the wrapper's own `requirements.txt` only says `psutil`. Published SHA-256 `0746f5f8d406af344fd547f1c8daa5f5c33dbc293bb8d6a16d80b4bb88f59372`, downloaded SHA-256 matched. BSD-3-Clause; upstream `giampaolo/psutil`; sdist contains Python collectors and platform C extensions. |
| [`sysinfo` 0.39.6 crates.io release](https://crates.io/api/v1/crates/sysinfo/0.39.6) / [crate source](https://static.crates.io/crates/sysinfo/sysinfo-0.39.6.crate) | Published/downloaded SHA-256 `d2071df9448915b71c4fe6d25deaf1c22f12bd234f01540b77312bb8e41361e6` matched. MIT; edition 2024, MSRV 1.95. It is a candidate only; no dependency was added or selected here. |

The wrapper is Python glue, not a metrics engine or Rust library. Its 16-line `PsutilWrapper` finds the installed `psutil` module spec, creates a fresh module object, executes it, then exposes that object as `self.psutil` (`psutil-home-assistant-0.0.1/psutil_home_assistant/__init__.py:1-16`). This gives its top-level module state its own sample dictionaries, the reason named by its README (`psutil-home-assistant-0.0.1/README.md:3-5`). Packaging metadata identifies the Apache license, author, project URL, and unversioned `psutil` requirement (`psutil-home-assistant-0.0.1/setup.py:9-22`, `psutil-home-assistant-0.0.1/requirements.txt:1`). The actual native collectors are upstream psutil 7.2.2: Python state/aggregation plus platform sources such as Linux `/proc` parsing and macOS C/Mach calls.

### CPU sample, baseline, rounding, and errors

- Upstream psutil keys `cpu_percent(interval=None)` state by current thread ID (`psutil-7.2.2/psutil/__init__.py:1823-1827,1841-1849`). Import initializes `_last_cpu_times` on the importing thread, with import-time collection errors caught and replaced by an empty map (`:1717-1729`). The wrapper is constructed in HA's executor, while its callback is a `@callback` timer handler (`homeassistant/components/hardware/__init__.py:45`, `homeassistant/components/hardware/websocket_api.py:59-66`). **Source-based inference:** the first event-loop sample misses the executor thread's baseline, takes two near-consecutive CPU time reads, and emits the documented meaningless near-zero first value (usually 0) rather than a startup-to-first-event average. HA does not prime/discard that sample. The focused HA test mocks `cpu_percent`; it does not test this real baseline (`tests/components/hardware/test_websocket_api.py:63-89`).
- `cpu_percent` clamps each negative time delta to zero, subtracts idle and Linux iowait from total to form busy time, divides busy delta by total delta, and rounds to one decimal (`psutil-7.2.2/psutil/__init__.py:1732-1785,1829-1849`). A zero total delta returns `0.0` (`:1834-1839`). HA then applies Python `round()` to an integer before emitting (`homeassistant/components/hardware/websocket_api.py:65-67`). A native port needs to preserve this two-stage rounding and Python's ties-to-even integer behavior; Rust's ordinary `round()` is not that rule.
- Linux `cpu_times()` reads aggregate `/proc/stat`; macOS delegates to `cext.cpu_times()` (`psutil-7.2.2/psutil/_pslinux.py:501-513`; `_psosx.py:125-128`). `PsutilWrapper` catches no `exec_module` error. After setup, HA's periodic metric callback has no try/except around either metrics call. Apart from psutil's explicit zero-delta case and guarded import-time priming, native read/API failures propagate; the hardware integration does not convert them to zero or null.

### Linux and macOS memory semantics

`virtual_memory()` returns byte counts, with `available` as the memory that can be given to processes without swapping and `percent = round((total - available) / total * 100, 1)` (`psutil-7.2.2/psutil/__init__.py:1995-2051`). Hardware uses `available` for both reported “free” MB and its `total - available` “used” MB; it does not use the psutil `free` or `used` fields.

| OS | psutil 7.2.2 source behavior |
| --- | --- |
| Linux | Reads `/proc/meminfo` into bytes; `MemAvailable` is primary. If it is missing **or zero**, fallback estimates it with file-LRU/slab data and zone low-watermarks, or `MemFree + Cached` when those inputs are missing (`psutil/_pslinux.py:306-430,243-303`). Negative available clamps to zero; available above total is replaced with MemFree. Missing optional counters become zero with a warning. Required reads/fields have no surrounding fallback catch. |
| macOS | The C extension reads total via `sysctl(HW_MEMSIZE)` and page counts via `host_statistics64` (`psutil/arch/osx/mem.c:22-81`). Python maps `available = inactive + free`, `used = active + wired`, then subtracts speculative pages only from the separate `free` field; percent uses `total - available` (`psutil/_psosx.py:91-105`). Hardware's used-MB therefore remains `total - (inactive + free)`, not psutil's own `used`. |

The wrapper does not define a graceful metrics-error contract: its loader asserts the module spec/loader and directly calls `exec_module` (`psutil-home-assistant-0.0.1/psutil_home_assistant/__init__.py:11-16`); hardware setup awaits construction without a catch, and status sampling has no catch. Linux psutil's missing/unreadable `/proc/meminfo` or missing required keys can fail the call; on macOS, failed sysctl/Mach calls return a Python extension error rather than a synthesized available value. Do not report unavailable metrics as zero unless the native contract deliberately changes this behavior.

### Native Rust candidate: verified `sysinfo` source

`sysinfo` 0.39.6 exposes `System::new()`, `System::new_with_specifics`, `refresh_cpu_usage`, `global_cpu_usage() -> f32`, `refresh_memory_specifics`, and byte-valued `total_memory`, `available_memory`, `free_memory`, and `used_memory` (`sysinfo-0.39.6/src/common/system.rs:38-93,156-195,501-503`; platform getters in `sysinfo-0.39.6/src/unix/linux/system.rs:320-333` and `sysinfo-0.39.6/src/unix/apple/system.rs:342-356`). Keep one `System` instance and refresh only CPU usage and RAM; `new()` loads nothing, while `new_all()` also refreshes processes and unrelated data. Linux and Apple set `MINIMUM_CPU_UPDATE_INTERVAL` to 200 ms (`sysinfo-0.39.6/src/unix/linux/system.rs:114`; `sysinfo-0.39.6/src/unix/apple/system.rs:73-74`), so a 5 s cadence is sufficient.

It is not a drop-in psutil memory/error backend:

- Linux uses `MemAvailable` when present, but only falls back when that field is missing—not when its value is zero. Its fallback is `MemFree + Buffers + Cached + SReclaimable - Shmem`, with no zone-watermark correction and no psutil-style negative/>total clamp (`sysinfo-0.39.6/src/unix/linux/system.rs:232-267`). Its `read_table` silently ignores read/parse failures (`:606-623`); fields can remain default or stale and refresh APIs return no `Result`.
- macOS sets `available_memory = active + inactive + free` and `free_memory = free - speculative` from `host_statistics64` page counts (`sysinfo-0.39.6/src/unix/apple/system.rs:155-211`). The comment cites an XNU formula including speculative pages, but the assignment omits them (`:192-199`). This differs from psutil's `inactive + free` by the active count. `sysinfo.used_memory()` is also its own `internal - purgeable + wired + compressor` formula, not HA's `total - available` (`:200-204`). If using this crate, calculate the HA field from its normalized `total - available` only after resolving the macOS semantic difference.
- CPU state is retained in `System`; `global_cpu_usage()` only returns the last refresh value. Its docs warn the first refresh is inaccurate and requires two samples with time between them (`sysinfo-0.39.6/src/common/system.rs:172-195,501-503`). Linux first reads `/proc/stat` relative to zeroed CPU counters (`sysinfo-0.39.6/src/unix/linux/cpu.rs:45-91,309-358`); macOS first calls `host_processor_info` and computes per-core values from that initial snapshot (`sysinfo-0.39.6/src/unix/apple/cpu.rs:32-64,261-288,318-366`). Thus a first refresh from an empty `System` represents since-boot counters, unlike HA's near-zero first event-loop sample inferred above. macOS global is the arithmetic average of per-core percentages (`sysinfo-0.39.6/src/unix/apple/cpu.rs:290-315`); Linux uses aggregate counters. `sysinfo` returns unrounded `f32`, so its consumer must implement HA rounding explicitly.
- These public refresh APIs return no error result. Linux CPU open errors log and leave the prior sample; Linux memory file-read errors are swallowed; macOS memory host-stat failures retain stored values. This stale/default-data behavior differs from psutil's propagated failures (`sysinfo-0.39.6/src/unix/linux/cpu.rs:58-67`; `sysinfo-0.39.6/src/unix/linux/system.rs:606-623`; `sysinfo-0.39.6/src/unix/apple/system.rs:183-208`).

### Focused native-provider options and acceptance

Candidate approaches for root to choose:

- A persistent `sysinfo::System` instance offers compact cross-platform CPU/RAM sampling, but needs a normalization/error adapter and explicit Linux/macOS tests because its available-memory behavior and refresh failures differ from the pinned source.
- A `MetricsProvider` trait backed by Linux `/proc/stat` + `/proc/meminfo` and macOS Mach/sysctl calls can preserve the audited psutil formulas and surface unavailable/error states, at the cost of two platform collectors.
- A hybrid can use sysinfo for CPU while a narrow native memory adapter enforces psutil-compatible `available` semantics.

No option is selected by this audit.

Use injected counters/files/Mach-result fixtures, never host readings, to test: module/thread baseline versus later 5 s CPU delta; guest/iowait and regressing CPU fields; psutil one-decimal then ties-to-even integer rounding; Linux `MemAvailable` present, missing, zero, fallback-underflow, and above-total cases plus the fallback inputs; macOS active/inactive/free/speculative page mapping; initialization/sample errors versus stale/zero; exact WebSocket keys, UTC timestamp, and subscriber timer lifecycle. Current HA tests cover the mocked event shape and unsubscribe only; they do not cover the dependency's actual CPU baseline or memory platform algorithms.
