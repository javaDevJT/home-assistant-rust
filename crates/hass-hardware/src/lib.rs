//! Native hardware SDK with typed Home Assistant hardware records, explicit
//! loaded-provider ownership, and an injected system-metrics sampler.
//!
//! This crate does not scan USB devices, read host metrics, or infer Supervisor
//! board information. Callers register only providers that are actually
//! loaded, and inject any host metrics source they are authorized to use.

mod integration;
mod models;
mod provider;

pub use hass_core;
pub use integration::{
    HardwareConfig, HardwareIntegration, HardwareManager, ProviderGuard, SystemStatusSubscription,
};
pub use models::{BoardInfo, HardwareInfo, RawSystemMetrics, SystemStatus, USBInfo};
pub use provider::{HardwareProvider, MetricsProvider};

#[cfg(test)]
mod tests;
