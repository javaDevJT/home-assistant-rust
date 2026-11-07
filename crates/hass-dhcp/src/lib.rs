//! Native Home Assistant DHCP discovery SDK.
//!
//! Providers hand normalized observations to a deterministic cache and
//! matcher. Packet capture, active scanning, and runtime event adapters remain
//! separate owners and are intentionally outside this crate.
#![forbid(unsafe_code)]
mod manager;
mod matcher;
pub use hass_config::{DiscoveryKey, DiscoveryKeyValue};
pub use manager::{
    DHCP_DOMAIN, DHCP_SOURCE, DhcpAddressData, DhcpDiscoveryAction, DhcpIntegration, DhcpManager,
    DhcpObservation, DhcpServiceInfo, DhcpUpdate, DhcpUpdateGuard, DiscoveryActionSink,
    IngestReport, RegisteredDeviceProvider, format_mac,
};
pub use matcher::{DhcpMatcher, DhcpMatcherIndex, GENERATED_SOURCE_COMMIT};
#[cfg(test)]
mod tests;
