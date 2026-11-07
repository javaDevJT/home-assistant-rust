# hass-hardware

A native Rust SDK for typed hardware records, explicitly loaded hardware
providers, and injected system-status metrics.

The crate does not scan USB devices, read host metrics, or infer board identity
from Supervisor. Callers register providers only while their owning integration
is loaded and supply a metrics provider for the host data source they control.

```rust,no_run
use std::sync::Arc;

use hass_hardware::{
    HardwareInfo, HardwareIntegration, MetricsProvider, RawSystemMetrics,
};

async fn example() -> hass_hardware::hass_core::Result<()> {
    let metrics: Arc<dyn MetricsProvider> = Arc::new(|| {
        Ok(RawSystemMetrics {
            cpu_percent: 12.4,
            total_bytes: 8 * 1024 * 1024,
            available_bytes: 3 * 1024 * 1024,
            memory_used_percent: 62.5,
        })
    });
    let mut integration = HardwareIntegration::new(metrics)?;
    let manager = integration.manager();
    let provider = manager.register_provider(
        "example",
        Arc::new(|| Ok(Vec::<HardwareInfo>::new())),
    )?;

    assert!(manager.hardware_info().await?.is_empty());
    drop(provider);
    integration.shutdown().await?;
    Ok(())
}
```

Status subscriptions share one sampler. The first event arrives after the
configured interval (five seconds by default); dropping the last subscription
cancels the next sample. The broadcast queue is bounded, and `recv()` exposes
`tokio::sync::broadcast::error::RecvError::Lagged` to consumers.

The next tick is scheduled before each read, preserving the source interval
when a read takes time. A generation change discards results admitted for an
earlier subscriber set. Shutdown closes both wrapped and raw receivers and
joins admitted metrics and hardware-info callbacks, including canceled info
callers. Synchronous providers must finish before that barrier can complete.
Zero-total-memory samples and provider failures are omitted with diagnostics.
Timestamps retain Python datetime's microsecond ISO formatting.
