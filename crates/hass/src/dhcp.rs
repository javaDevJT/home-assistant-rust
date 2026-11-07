//! Strong app ownership of DHCP observations and their discovery-flow bridge.

use crate::discovery::DiscoveryHandle;
use anyhow::Result;
use hass_config::{
    ConfigEntries, DiscoveryKeyRemoval, DiscoveryKeyRemovalObserver,
    DiscoveryKeyRemovalObserverGuard,
};
use hass_core::{Error, Runtime, State};
use hass_dhcp::{
    DhcpIntegration, DhcpManager, DhcpObservation, DiscoveryActionSink, RegisteredDeviceProvider,
    format_mac,
};
use hass_registry::Registries;
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::{sync::watch, task::JoinHandle};

struct RegisteredDevices {
    registries: Registries,
    entries: ConfigEntries,
}

impl RegisteredDeviceProvider for RegisteredDevices {
    fn domains_for_network_mac(&self, formatted_mac: &str) -> Vec<String> {
        self.registries
            .devices()
            .into_iter()
            .filter(|device| {
                device
                    .connections
                    .iter()
                    .any(|(kind, value)| kind == "mac" && format_mac(value) == formatted_mac)
            })
            .filter_map(|device| device.config_entry_id)
            .filter_map(|entry_id| self.entries.get_entry(&entry_id))
            .map(|entry| entry.domain)
            .collect()
    }
}

struct RemovedKeys(DhcpManager);

impl DiscoveryKeyRemovalObserver for RemovedKeys {
    fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> hass_core::Result<()> {
        self.0.rediscover(&event.keys)?;
        Ok(())
    }
}

/// Cache/matcher, tracker-state and connected-device observation owner.
/// Packet capture and active scan providers borrow its weak manager.
pub struct DhcpHost {
    integration: Option<DhcpIntegration>,
    _registered_devices: Arc<dyn RegisteredDeviceProvider>,
    entries: ConfigEntries,
    observer: Option<DiscoveryKeyRemovalObserverGuard>,
    stop: watch::Sender<bool>,
    jobs: Vec<JoinHandle<()>>,
}

impl DhcpHost {
    pub fn start(
        runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        admission: DiscoveryHandle,
    ) -> Result<Self> {
        let registered_devices: Arc<dyn RegisteredDeviceProvider> = Arc::new(RegisteredDevices {
            registries,
            entries: entries.clone(),
        });
        let sink: DiscoveryActionSink = Arc::new(move |action| {
            admission.create(
                action.domain,
                action.source,
                action.discovery_key,
                action.context,
                action.data,
            )
        });
        let integration =
            DhcpIntegration::new(Some(Arc::downgrade(&registered_devices)), Some(sink));
        let manager = integration.manager();
        let (initial_states, mut events) = runtime.subscribe_with_states();
        let mut signals = runtime.subscribe_signals();
        let (stop, mut state_stop) = watch::channel(false);
        let mut registration_stop = stop.subscribe();
        for state in initial_states {
            process_tracker_state(&manager, &state);
        }
        let state_manager = manager.clone();
        let state_job = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = state_stop.changed() => break,
                    event = events.recv() => match event {
                        Ok(event) => {
                            // HA subscribes to added tracker states, rather
                            // than every update of an existing tracker.
                            if event.event_type != "state_changed"
                                || !event.data.get("old_state").is_some_and(Value::is_null)
                            {
                                continue;
                            }
                            if let Some(value) = event.data.get("new_state")
                                && !value.is_null()
                            {
                                match serde_json::from_value::<State>(value.clone()) {
                                    Ok(state) => process_tracker_state(&state_manager, &state),
                                    Err(error) => tracing::error!(%error, "Invalid tracker state in DHCP observer"),
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::warn!(missed, "DHCP tracker observer lagged; loading current states");
                            let (states, receiver) = runtime.subscribe_with_states();
                            events = receiver;
                            for state in states {
                                process_tracker_state(&state_manager, &state);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        let registration_job = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = registration_stop.changed() => break,
                    event = signals.recv() => match event {
                        Ok(event) if event.event_type == hass_device_tracker::CONNECTED_DEVICE_REGISTERED => {
                            match registered_observation(&event.data) {
                                Ok(Some(observation)) => ingest(&manager, observation),
                                Ok(None) => {},
                                Err(error) => tracing::error!(%error, "Invalid connected-device observation for DHCP"),
                            }
                        }
                        Ok(_) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::warn!(missed, "DHCP connected-device observer missed private signals");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        Ok(Self {
            integration: Some(integration),
            _registered_devices: registered_devices,
            entries,
            observer: None,
            stop,
            jobs: vec![state_job, registration_job],
        })
    }

    pub fn manager(&self) -> DhcpManager {
        self.integration
            .as_ref()
            .expect("active DHCP owner")
            .manager()
    }

    /// Install only after started providers have finished their setup attempt,
    /// matching the source's config-entry removal subscription order.
    pub fn observe_removed_keys(&mut self) -> Result<()> {
        if self.observer.is_none() {
            self.observer = Some(
                self.entries
                    .observe_discovery_key_removals(Arc::new(RemovedKeys(self.manager())))?,
            );
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.observer.take();
        self.stop.send_replace(true);
        let mut failure = None;
        for job in self.jobs.drain(..) {
            if let Err(error) = job.await {
                failure.get_or_insert(error);
            }
        }
        self.integration.take();
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(())
    }
}

impl Drop for DhcpHost {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        for job in &self.jobs {
            job.abort();
        }
    }
}

fn ingest(manager: &DhcpManager, observation: DhcpObservation) {
    if let Err(error) = manager.ingest(observation, false) {
        tracing::error!(%error, "DHCP observation failed");
    }
}

fn process_tracker_state(manager: &DhcpManager, state: &State) {
    if !state.entity_id.starts_with("device_tracker.")
        || state.state != "home"
        || state.attributes.get("source_type").and_then(Value::as_str) != Some("router")
    {
        return;
    }
    match tracker_observation(&state.attributes) {
        Ok(Some(observation)) => ingest(manager, observation),
        Ok(None) => {}
        Err(error) => tracing::error!(%error, "Invalid router tracker observation for DHCP"),
    }
}

fn optional_string<'a>(
    data: &'a Map<String, Value>,
    key: &str,
) -> hass_core::Result<Option<&'a str>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::InvalidInput(format!("{key} must be a string"))),
    }
}

fn tracker_observation(data: &Map<String, Value>) -> hass_core::Result<Option<DhcpObservation>> {
    let (Some(ip), Some(mac)) = (optional_string(data, "ip")?, optional_string(data, "mac")?)
    else {
        return Ok(None);
    };
    let hostname = match data.get("host_name") {
        None => "",
        Some(Value::String(value)) => value,
        Some(_) => return Err(Error::InvalidInput("host_name must be a string".into())),
    };
    Ok(Some(DhcpObservation {
        ip: ip.into(),
        hostname: hostname.into(),
        mac: mac.into(),
    }))
}

fn registered_observation(data: &Map<String, Value>) -> hass_core::Result<Option<DhcpObservation>> {
    let (Some(ip), Some(mac)) = (optional_string(data, "ip")?, optional_string(data, "mac")?)
    else {
        return Ok(None);
    };
    Ok(Some(DhcpObservation {
        ip: ip.into(),
        hostname: optional_string(data, "host_name")?
            .unwrap_or_default()
            .into(),
        mac: mac.into(),
    }))
}

#[cfg(test)]
#[path = "dhcp_tests.rs"]
mod tests;
