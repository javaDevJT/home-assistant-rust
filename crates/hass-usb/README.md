# hass-usb

Native USB serial discovery SDK primitives for the Home Assistant Rust host.

The contract is pinned to Home Assistant `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and documented in the worktree's `docs/USB-PORT.md`. Core matcher rows are generated from that commit's `homeassistant/generated/usb.py`; the list order and the separate serial-port-selector list are retained in `src/generated.rs`.

## Scope

This crate handles serial-port records, ordered USB matching, merged scans, complete-value snapshot diffs, discovery action payloads, source callback lifecycle, and cancellable debounce ownership. It does not enumerate USB devices or start a platform watcher. The embedding app supplies `SerialPortProvider`, `TimerScheduler`, and (when used) `UsbHotplugWatcher` implementations. No USB HID or other non-serial USB endpoints are represented.

`SerialPortInfo.device` is the exact path-or-URL string used for scanner replacement and snapshot identity. `resolved_device` is copied metadata. Provider records become `USBDevice` only when both numeric VID and PID exist; otherwise they remain `SerialDevice`. The discovery snapshot contains USB records only, and equality includes every field, so metadata or path changes produce removal plus addition. Consumer unique-ID output is preserved by `usb_unique_id_from_service_info`, including collisions; it is not used to merge scan records.

## Public API

```rust,ignore
let owner = UsbIntegration::new(
    provider,
    scheduler,
    UsbOptions::with_custom_matchers(&custom_matchers),
    serial_number_policy,
);
let usb = owner.manager(); // weak handle; owner retains the state and timers

let scanner_guard = usb.register_serial_port_scanner(|| {
    Ok(vec![PortDevice::Usb(contributed_device)])
})?;
let scan = usb.scan_serial_ports()?; // USB and non-USB serial ports
let report = usb.scan_now()?;         // started/polling scan, no request callbacks
let _ = usb.request_scan()?;          // observer-aware 10 second debouncer
let _ = usb.notify_serial_ports_changed()?; // 5 second change debouncer
```

`UsbIntegration` strongly owns the callback state, debouncers, and successfully started watcher. `UsbManager`, callback guards, scanner guards, watcher callbacks, and timer callbacks use weak references. Dropping a guard removes only its registration ID. Stopping the owner cancels pending timers, stops and joins the watcher, and waits for active request callbacks, provider/scanner work and diagnostic callbacks before returning; a watcher join error is returned after cleanup. Concurrent stop calls wait for the same completed barrier. The app also joins its scheduler and polling owner. Synchronous callbacks must finish, and must not call stop or start a nested scan while holding their scan gate.

The immediate manual-scan debouncer runs the first request now, then keeps one trailing request per 10 second cooldown. It is skipped while the observer is active. The non-immediate add/remove debouncer runs after 5 seconds and queues one trailing run if a new request arrives during cooldown. Both use the injected scheduler and can be deterministically advanced in tests.

Create actions carry domain, `source = "usb"`, matching source context, and exact `UsbServiceInfo`; the type has no discovery-key field. Dismiss actions carry the same typed service data and only the ordered, most-specific matching domains. The app adapter owns flow admission/abort policy.

Matcher behavior follows Home Assistant: VID/PID are exact case-sensitive strings; observed serial/manufacturer/description values are lowercased before Python-style `fnmatch` matching, while patterns are not changed. All supplied fields must match, and only all matches at greatest specificity (including `domain`) are returned in input order. Custom entries append after the generated core list.

`config_entry_uses_serial_port` exposes the pinned core eligibility set derived from manifest `usb` dependencies/after-dependencies plus the four source exceptions (`alarmdecoder`, `bryant_evolution`, `elkm1`, and `mysensors`). `integration_uses_serial_port` applies the same metadata rule to custom integrations. These helpers identify domains that may use serial ports; they do not infer actual configured use.

`SerialNumberPolicy` and `PathResolver` preserve the source moved-port and realpath helper seams without embedding serialx or filesystem assumptions. The provider returns hard enumeration errors as `UsbError`; registered scanner failures are isolated, recorded, and do not prevent later scanners from contributing.

`usb_device_from_path` performs a fresh physical-provider lookup, excluding registered scanner contributions as the pinned utility does. A fresh `scan_serial_ports` list includes scanners but does not change discovery state or invoke scan-request callbacks. Direct scan callers receive isolated failure reports; manual and delayed scans also forward these diagnostics through the scan-error observer before the debouncer discards its report. Shutdown-induced errors do not dispatch callbacks after admission closes.

## Synthetic checks

Run from the repository root:

```sh
cargo test --manifest-path crates/hass-usb/Cargo.toml
```

Tests use synthetic records, an injected fake scheduler, fake watchers, and fake providers only. They do not enumerate, open, or probe real serial devices.
