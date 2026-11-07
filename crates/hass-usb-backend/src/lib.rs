//! Native serial-port discovery and hotplug support for `hass-usb`.
//!
//! Linux enumeration follows `/sys/class/tty` and `/dev/serial/by-id`; macOS
//! enumeration uses IOKit `IOSerialBSDClient` records. URL-backed ports are
//! intentionally not OS-enumerated. The watcher only reports that the USB
//! device tree changed; the SDK owns debouncing and scans.

#[cfg(target_os = "macos")]
mod darwin;
mod errors;
#[cfg(any(target_os = "linux", all(test, unix)))]
mod linux;
mod paths;
mod udev;
mod watcher_events;
#[cfg(target_os = "linux")]
mod watcher_linux;
#[cfg(not(target_os = "linux"))]
mod watcher_unavailable;

use hass_usb::{SerialPortInfo, SerialPortProvider, UsbError, UsbHotplugWatcher};
use std::sync::Arc;

pub use errors::{UsbBackendError, WatcherError, WatcherStartError};
pub use hass_usb::serial_path_udev_id;
pub use paths::{NativePathResolver, resolve_serial_path};
pub use udev::{UdevPortIdentity, udev_serial_by_id_stem};
pub use watcher_events::{SOURCE_WATCH_EVENT_MASK, watcher_event_requires_notification};
pub type UsbPortChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;
#[cfg(not(target_os = "linux"))]
pub use watcher_unavailable::NativeSerialPortWatcher;

#[cfg(target_os = "linux")]
pub use watcher_linux::{NativeSerialPortWatcher, USB_DEVICE_TREE};

/// Enumerate physical serial ports using the native platform source.
///
/// Linux and macOS are supported. Other platforms return an explicit error
/// rather than reporting a successful empty inventory. URL-backed ports are
/// left to the serial-port provider or scanner that owns the URL scheme.
pub fn enumerate_serial_ports() -> Result<Vec<SerialPortInfo>, UsbBackendError> {
    #[cfg(target_os = "linux")]
    {
        linux::enumerate_linux(std::path::Path::new("/sys"), std::path::Path::new("/dev"))
    }
    #[cfg(target_os = "macos")]
    {
        darwin::enumerate_iokit()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(UsbBackendError::UnsupportedPlatform(
            "native serial enumeration is implemented for Linux and macOS",
        ))
    }
}

/// Enumerate a Linux sysfs/device tree supplied by the caller.
///
/// This seam permits deterministic fixture-based validation and can also be
/// used by containers that mount sysfs and devfs at nonstandard paths.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub fn enumerate_serial_ports_at(
    sys_root: &std::path::Path,
    dev_root: &std::path::Path,
) -> Result<Vec<SerialPortInfo>, UsbBackendError> {
    linux::enumerate_linux(sys_root, dev_root)
}

/// Default SDK adapter for the native platform enumerator.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSerialPortProvider;

impl SerialPortProvider for NativeSerialPortProvider {
    fn list_serial_ports(&self) -> Result<Vec<SerialPortInfo>, UsbError> {
        enumerate_serial_ports().map_err(|error| UsbError::new(error.to_string()))
    }
}

impl UsbHotplugWatcher for NativeSerialPortWatcher {
    fn start(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), hass_usb::WatcherStartError> {
        self.start_watching(callback).map_err(|error| match error {
            WatcherStartError::InotifyNotAvailable => {
                hass_usb::WatcherStartError::InotifyNotAvailable("inotify unavailable".to_owned())
            }
            other => hass_usb::WatcherStartError::Other(UsbError::new(other.to_string())),
        })
    }

    fn stop_and_join(&self) -> Result<(), UsbError> {
        NativeSerialPortWatcher::stop_and_join(self)
            .map_err(|error| UsbError::new(error.to_string()))
    }
}

/// Map an IOKit serial record to the provider-facing source fields.
///
/// This pure mapper is also the injection boundary for Darwin tests; it never
/// queries the host registry itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DarwinSerialPortRecord {
    pub device: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub bcd_device: Option<u16>,
    pub interface_description: Option<String>,
    pub interface_num: Option<u8>,
}

/// Convert injected IOKit records while preserving their order and fields.
pub fn map_darwin_serial_records(
    records: impl IntoIterator<Item = DarwinSerialPortRecord>,
) -> Vec<SerialPortInfo> {
    records
        .into_iter()
        .map(|record| {
            let description = source_description(
                record.product.as_deref(),
                record.interface_description.as_deref(),
                &record.device,
            );
            SerialPortInfo {
                device: record.device.clone(),
                resolved_device: Some(record.device),
                vid: record.vid,
                pid: record.pid,
                serial_number: record.serial_number,
                manufacturer: record.manufacturer,
                description: Some(description),
                bcd_device: record.bcd_device.map(i64::from),
                interface_description: record.interface_description,
                interface_num: record.interface_num.map(i64::from),
            }
        })
        .collect()
}

pub(crate) fn source_description(
    product: Option<&str>,
    interface_description: Option<&str>,
    resolved_device: &str,
) -> String {
    if let Some(interface_description) = interface_description {
        return format!("{} - {interface_description}", product.unwrap_or("None"));
    }
    if let Some(product) = product {
        return product.to_owned();
    }
    std::path::Path::new(resolved_device)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests;
