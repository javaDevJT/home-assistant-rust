# hass-usb-backend

Native serial-port provider and USB hotplug watcher for the `hass-usb` SDK.

`enumerate_serial_ports()` reads physical serial ports exposed by Linux sysfs
and `/dev/serial/by-id`, or macOS IOKit serial services. It preserves the
source `device` alias and the resolved physical path separately, maps both USB
and non-USB serial ports, and computes the source-compatible description.
URL-backed serial transports are not enumerated by the operating system.

`NativeSerialPortWatcher` watches `/dev/bus/usb` on Linux and emits only a
changed notification. It recursively watches directories and newly created
directories. `stop_and_join()` and `Drop` cancel and join the owned worker.
On platforms without the pinned inotify behavior, startup returns
`WatcherStartError::InotifyNotAvailable`; an application may use its own
five-second polling fallback for that error. Other startup and worker errors
remain errors. The backend does not debounce or scan.

The crate uses the SDK's `SerialPortInfo`, provides `NativeSerialPortProvider`,
`NativePathResolver`, `resolve_serial_path`, `serial_path_udev_id`, and
`udev_serial_by_id_stem`. Synthetic sysfs trees and injected IOKit records are
available to tests; they do not inspect live serial inventory or open devices.

See [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for source and license
provenance.
