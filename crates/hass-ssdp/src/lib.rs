//! Native SSDP scanning, UPnP advertisement, and private discovery callbacks.
//!
//! Strong integrations, scanners, and servers own their work; weak managers
//! expose cache lookup and callback registration without retaining the host.

#![forbid(unsafe_code)]

mod description;
mod headers;
mod manager;
mod matcher;
mod network;
mod protocol;
mod server;
mod service_info;
mod tracker;

pub use description::{Description, DescriptionCache, parse_description_xml};
pub use headers::{Headers, InternalMetadata};
pub use manager::{
    DiscoveryAction, DiscoveryFuture, DiscoverySink, SsdpAsyncCallback, SsdpCallback,
    SsdpCallbackGuard, SsdpIntegration, SsdpManager, owned_discovery_task,
};
pub use matcher::{
    IntegrationMatchers, IntegrationMatchersConfig, MatchDict, headers_match,
    pinned_integration_matchers,
};
pub use network::{SsdpInterface, SsdpScanner, SsdpScannerOptions, enabled_sources};
pub use protocol::{
    DecodeError, MessageKind, SsdpMessage, build_search_packet, decode_ssdp_packet,
    is_valid_ssdp_packet, udn_from_usn,
};
pub use server::{DeviceIcon, UpnpDevice, UpnpServer, UpnpServerOptions};
pub use service_info::{SsdpChange, SsdpServiceInfo};
pub use tracker::{DeviceSnapshot, SsdpSource, Tracker, TrackerEvent};

/// Multicast SSDP port used by the standard search target.
pub const SSDP_PORT: u16 = 1900;

/// Standard IPv4 SSDP multicast target.
pub const SSDP_TARGET_V4: &str = "239.255.255.250";

/// Standard IPv6 link-local SSDP multicast target.
pub const SSDP_TARGET_V6: &str = "FF02::C";

/// Default UPnP search cache duration.
pub const DEFAULT_MAX_AGE_SECONDS: i64 = 900;
