//! Provider interfaces used by the hardware registry.

use crate::{HardwareInfo, RawSystemMetrics};

/// Synchronous hardware information callback.
///
/// The manager invokes callbacks on Tokio's blocking pool. A callback should
/// return only rows backed by its loaded integration's configuration or other
/// explicit source; the manager does not invent board or USB records.
pub trait HardwareProvider: Send + Sync + 'static {
    /// Return this provider's hardware rows.
    fn hardware_info(&self) -> hass_core::Result<Vec<HardwareInfo>>;
}

impl<F> HardwareProvider for F
where
    F: Fn() -> hass_core::Result<Vec<HardwareInfo>> + Send + Sync + 'static,
{
    fn hardware_info(&self) -> hass_core::Result<Vec<HardwareInfo>> {
        self()
    }
}

/// Injected source of operating-system CPU and memory metrics.
pub trait MetricsProvider: Send + Sync + 'static {
    /// Read a complete sample. Errors are logged and the sample is omitted.
    fn read(&self) -> hass_core::Result<RawSystemMetrics>;
}

impl<F> MetricsProvider for F
where
    F: Fn() -> hass_core::Result<RawSystemMetrics> + Send + Sync + 'static,
{
    fn read(&self) -> hass_core::Result<RawSystemMetrics> {
        self()
    }
}
