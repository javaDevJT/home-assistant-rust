//! Native core health fields with actual runtime sources.

use std::{collections::BTreeMap, sync::Arc};

use hass_core::Runtime;
use hass_system_health::{
    SystemHealthCallback, SystemHealthData, SystemHealthIntegration, SystemHealthValue,
};
use serde_json::json;

pub const CONFIGURATION_WARNING: &str = "The system_health integration does not support configuration parameters; remove the parameters from configuration.";

pub fn start(runtime: Runtime) -> hass_core::Result<SystemHealthIntegration> {
    let owner = SystemHealthIntegration::new();
    let callback: SystemHealthCallback = Arc::new(move || {
        let time_zone = runtime.time_zone();
        Box::pin(async move {
            Ok(BTreeMap::from([
                (
                    "version".into(),
                    SystemHealthValue::Immediate(SystemHealthData::Json(json!(format!(
                        "core-{}",
                        hass_core::VERSION
                    )))),
                ),
                (
                    "timezone".into(),
                    SystemHealthValue::Immediate(SystemHealthData::Json(json!(time_zone))),
                ),
            ]))
        })
    });
    owner.register_info("homeassistant", callback, None)?;
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;

    #[tokio::test]
    async fn core_provider_reports_native_version_and_current_runtime_timezone() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime.set_time_zone("America/Detroit").unwrap();
        let owner = start(runtime.clone()).unwrap();
        let first = owner.manager().get_info().await.unwrap();
        assert_eq!(
            first["homeassistant"],
            BTreeMap::from([
                ("version".into(), json!("core-0.1.0-rust")),
                ("timezone".into(), json!("America/Detroit")),
            ])
        );
        runtime.set_time_zone("Europe/London").unwrap();
        let second = owner.manager().get_info().await.unwrap();
        assert_eq!(second["homeassistant"]["timezone"], "Europe/London");
    }
}
