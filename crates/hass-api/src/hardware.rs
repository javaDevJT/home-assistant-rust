//! WebSocket adapter helpers for the hardware integration.
//!
//! The manager owns provider failure handling and the shared status sampler.
//! This module projects those typed values into the existing WebSocket result
//! and event envelopes without adding a second sampler or an unbounded queue.

use hass_core::Result;
use hass_hardware::{HardwareManager, SystemStatusSubscription};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Return the currently registered hardware records in the public response
/// shape. `HardwareInfo` and its nested records serialize every optional field
/// as an explicit `null`, matching the pinned dataclass response.
pub(crate) async fn info(manager: &HardwareManager) -> Result<Value> {
    Ok(json!({ "hardware": manager.hardware_info().await? }))
}

/// One WebSocket subscription to the shared hardware status sampler.
///
/// The subscription owns its SDK receiver; dropping this adapter drops that
/// receiver and unregisters only this subscriber. All subscribers still share
/// the one sampler owned by `HardwareIntegration`.
pub(crate) struct HardwareStatusStream {
    id: u64,
    subscription: SystemStatusSubscription,
}

impl HardwareStatusStream {
    /// Subscribe to the manager's shared sampler for this WebSocket command ID.
    pub(crate) fn subscribe(manager: &HardwareManager, id: u64) -> Result<Self> {
        Ok(Self {
            id,
            subscription: manager.subscribe_system_status()?,
        })
    }

    /// Read one queued status event without waiting, preserving Tokio's
    /// bounded-broadcast lag count for the WebSocket owner to report.
    pub(crate) fn try_recv(
        &mut self,
    ) -> std::result::Result<Value, broadcast::error::TryRecvError> {
        let status = self.subscription.receiver().try_recv()?;
        Ok(json!({
            "id": self.id,
            "type": "event",
            "event": status,
        }))
    }
}
