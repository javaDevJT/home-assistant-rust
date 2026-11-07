//! Native Home Assistant USB serial discovery primitives.
//!
//! This crate implements Home Assistant's USB serial discovery contract without
//! owning a platform enumerator or hotplug watcher. The embedding application
//! supplies those through [`SerialPortProvider`] and [`TimerScheduler`].

#![forbid(unsafe_code)]

mod debounce;
mod manager;
mod matcher;
mod models;
mod paths;

pub mod generated;

pub use generated::{
    CORE_USB_MATCHER_SPECS, NON_USB_SERIAL_DOMAINS, SERIAL_CONFIG_ENTRY_DOMAINS,
    SERIAL_PORT_SELECTOR_INTEGRATIONS, config_entry_uses_serial_port, integration_uses_serial_port,
};

pub use debounce::{ScheduledTask, TimerScheduler};
pub use manager::{
    ADD_REMOVE_SCAN_COOLDOWN, DebounceRequest, DiscoveryAction, DiscoveryContext, DiscoveryCreate,
    DiscoveryDismiss, HostPlatform, MANUAL_SCAN_COOLDOWN, PortEventCallback, ScanErrorCallback,
    ScanReport, ScanRequestCallback, ScannerFailure, SerialNumberPolicy, SerialPortProvider,
    SerialPortScan, SerialPortScanner, UsbCallbackGuard, UsbDiscoveryCallback, UsbError,
    UsbHotplugWatcher, UsbIntegration, UsbManager, UsbOptions, UsbScannerGuard, WatcherStartError,
};
pub use matcher::{
    USBMatcher, USBMatcherSpec, matching_usb_matchers, merge_usb_matchers,
    usb_device_matches_matcher,
};
pub use models::{
    PortDevice, SerialDevice, SerialPortConsumer, SerialPortConsumerKind, SerialPortInfo,
    USBDevice, UsbServiceInfo, has_unique_serial_number_with, usb_device_from_port,
    usb_serial_device_from_port, usb_service_info_from_device, usb_unique_id_from_service_info,
};
pub use paths::{PathResolver, find_moved_serial_port, serial_path_udev_id, usb_device_from_path};

/// Shared discovery source spelling. Create requests deliberately have no key.
pub const USB_SOURCE: &str = "usb";

#[cfg(test)]
mod tests;
