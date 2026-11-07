//! Application ownership for the shared DNS-SD browser and local advertisement.

use crate::discovery::{DiscoveryHandle, DiscoveryRemoval};
use crate::zeroconf_repairs::ZeroconfRepairsHost;
use anyhow::Result;
use hass_config::{
    ConfigEntries, DiscoveryKey, DiscoveryKeyRemoval, DiscoveryKeyRemovalObserver,
    DiscoveryKeyRemovalObserverGuard, DiscoveryKeyValue,
};
use hass_core::{Error, Runtime};
use hass_zeroconf::{
    DiscoveryAction, DiscoveryActionSink, DiscoverySource, HOME_ASSISTANT_SERVICE_TYPE,
    ListenerGuard, OrdinaryFlowDecision, ZeroconfIntegration, ZeroconfManager, ZeroconfServiceInfo,
    backend::MdnsWireOwner, classify_homekit,
};
use mdns_sd::{NetworkInterface, ServiceInfo};
use serde_json::{Map, json};
use std::{net::IpAddr, net::SocketAddr, sync::Arc};

pub const CONFIGURATION_WARNING: &str = "The zeroconf default_interface and ipv6 configuration parameters are deprecated and ignored; remove them from configuration.";

struct RemovedKeys(ZeroconfManager);

impl DiscoveryKeyRemovalObserver for RemovedKeys {
    fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> hass_core::Result<()> {
        for key in &event.keys {
            if key.domain == "zeroconf"
                && key.version == 1
                && let DiscoveryKeyValue::Tuple(parts) = &key.key
                && let [service_type, name] = parts.as_slice()
            {
                self.0.rediscover(service_type, name)?;
            }
        }
        Ok(())
    }
}

pub struct ZeroconfHost {
    wire: MdnsWireOwner,
    integration: ZeroconfIntegration,
    key_observer: Option<DiscoveryKeyRemovalObserverGuard>,
    homekit_listener: Option<ListenerGuard>,
    repairs: Option<ZeroconfRepairsHost>,
    advertisement: Option<ServiceInfo>,
    registered_name: Option<String>,
}

impl ZeroconfHost {
    pub async fn start(
        runtime: Runtime,
        entries: ConfigEntries,
        network: hass_network::NetworkManager,
        repairs: hass_repairs::RepairsManager,
        admission: DiscoveryHandle,
        bound_address: SocketAddr,
    ) -> Result<Self> {
        let interfaces = crate::zeroconf_network::selected_interfaces(&network)?;
        let addresses = crate::zeroconf_network::announce_addresses(&network)?;
        Self::start_with_transport(
            runtime,
            entries,
            repairs,
            admission,
            bound_address,
            interfaces,
            addresses,
            mdns_sd::MDNS_PORT,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_transport(
        runtime: Runtime,
        entries: ConfigEntries,
        repairs: hass_repairs::RepairsManager,
        admission: DiscoveryHandle,
        bound_address: SocketAddr,
        interfaces: Vec<NetworkInterface>,
        addresses: Vec<IpAddr>,
        port: u16,
    ) -> Result<Self> {
        let identity_runtime = runtime.clone();
        let instance_id =
            tokio::task::spawn_blocking(move || crate::instance_id::get(&identity_runtime))
                .await??;
        let advertisement = if interfaces.is_empty() || addresses.is_empty() {
            tracing::warn!("No selected addresses are available for the Zeroconf advertisement");
            None
        } else {
            Some(local_service_info(
                &entries.core_config(),
                &instance_id,
                &addresses,
                bound_address,
            )?)
        };
        let wire = MdnsWireOwner::start(interfaces, port)?;
        let integration =
            ZeroconfIntegration::new(wire.backend(), Some(discovery_sink(admission.clone())))?;
        let manager = integration.manager();
        let key_observer =
            entries.observe_discovery_key_removals(Arc::new(RemovedKeys(manager.clone())))?;
        let homekit_listener = manager.register_service_update_listener(move |raw| {
            let Some(info) = ZeroconfServiceInfo::from_raw(raw.clone()) else {
                return Ok(());
            };
            if let Some(flow) = classify_homekit(&info).homekit_flow {
                admit(
                    &admission,
                    flow.target_domain,
                    DiscoverySource::HomeKit,
                    discovery_key(&info),
                    &info,
                    None,
                )?;
            }
            Ok(())
        })?;
        let repairs = ZeroconfRepairsHost::start(runtime, repairs, manager.clone())?;
        if advertisement.is_some() {
            repairs.set_local_identity(instance_id, addresses);
        }
        let browser_manager = manager.clone();
        let browse_types = manager.browse_types()?;
        let wire = tokio::task::spawn_blocking(move || -> hass_core::Result<_> {
            wire.start_browsing(browse_types, move |service_type, name, state| {
                browser_manager.service_update(service_type, name, state)
            })?;
            Ok(wire)
        })
        .await??;
        Ok(Self {
            wire,
            integration,
            key_observer: Some(key_observer),
            homekit_listener: Some(homekit_listener),
            repairs: Some(repairs),
            advertisement,
            registered_name: None,
        })
    }

    pub fn manager(&self) -> ZeroconfManager {
        self.integration.manager()
    }

    /// Called after the real homeassistant_started event and HTTP bind.
    pub async fn mark_started(&mut self) -> Result<()> {
        if let Some(advertisement) = self.advertisement.take() {
            self.registered_name = Some(self.wire.register_service(advertisement).await?);
        }
        Ok(())
    }

    pub async fn stop_discovery(&mut self) -> Result<()> {
        self.key_observer.take();
        self.homekit_listener.take();
        if let Some(repairs) = self.repairs.take() {
            repairs.shutdown();
        }
        let browse_result = self.wire.stop_browsing().await;
        self.integration.shutdown().await;
        browse_result?;
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        // Keep the shared daemon available through the other integrations'
        // shutdown, then send our goodbye and join the actual OS thread.
        let browse_result = self.stop_discovery().await;
        let unregister_result = if let Some(name) = self.registered_name.take() {
            self.wire.unregister_service(&name).await
        } else {
            Ok(())
        };
        let shutdown_result = self.wire.shutdown().await;
        browse_result?;
        unregister_result?;
        shutdown_result?;
        Ok(())
    }
}

fn discovery_sink(admission: DiscoveryHandle) -> DiscoveryActionSink {
    Arc::new(move |action| match action {
        DiscoveryAction::Create {
            domain,
            source,
            discovery_key,
            info,
            alternative_domain,
        } => {
            let alternative_domain = if source == DiscoverySource::Zeroconf {
                match classify_homekit(&info).ordinary_flow {
                    OrdinaryFlowDecision::Continue {
                        alternative_domain, ..
                    } => alternative_domain,
                    OrdinaryFlowDecision::Stop => return Ok(()),
                    OrdinaryFlowDecision::Abort(error) => {
                        return Err(Error::Service(error.to_string()));
                    }
                }
            } else {
                alternative_domain
            };
            admit(
                &admission,
                domain,
                source,
                discovery_key,
                &info,
                alternative_domain,
            )
        }
        DiscoveryAction::Dismiss { name, .. } => {
            admission.dismiss(DiscoveryRemoval::ZeroconfName(name))
        }
    })
}

fn admit(
    admission: &DiscoveryHandle,
    domain: String,
    source: DiscoverySource,
    key: DiscoveryKey,
    info: &ZeroconfServiceInfo,
    alternative_domain: Option<String>,
) -> hass_core::Result<()> {
    let data = serde_json::to_value(info)
        .map_err(|error| Error::Service(error.to_string()))?
        .as_object()
        .cloned()
        .ok_or_else(|| Error::Service("invalid Zeroconf discovery data".into()))?;
    let mut context = Map::new();
    if let Some(domain) = alternative_domain {
        context.insert("alternative_domain".into(), json!(domain));
    }
    admission.create(domain, source.as_str().into(), key, context, data)
}

fn discovery_key(info: &ZeroconfServiceInfo) -> DiscoveryKey {
    DiscoveryKey {
        domain: "zeroconf".into(),
        key: DiscoveryKeyValue::Tuple(vec![info.service_type.clone(), info.name.clone()]),
        version: 1,
    }
}

fn local_service_info(
    config: &hass_config::CoreConfig,
    instance_id: &str,
    addresses: &[IpAddr],
    bound: SocketAddr,
) -> Result<ServiceInfo> {
    let location = valid_location_name(&config.location_name);
    let external = config.external_url.clone().unwrap_or_default();
    let internal = config.internal_url.clone().unwrap_or_else(|| {
        let address = if bound.ip().is_unspecified() {
            addresses.first().copied().unwrap_or(bound.ip())
        } else {
            bound.ip()
        };
        format!("http://{}", SocketAddr::new(address, bound.port()))
    });
    let base = if external.is_empty() {
        &internal
    } else {
        &external
    };
    let properties = [
        ("location_name", location.as_str()),
        ("uuid", instance_id),
        ("version", hass_core::VERSION),
        ("external_url", external.as_str()),
        ("internal_url", internal.as_str()),
        ("base_url", base.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), bounded_property(value).to_owned()))
    .collect::<std::collections::HashMap<_, _>>();
    Ok(ServiceInfo::new(
        HOME_ASSISTANT_SERVICE_TYPE,
        &location,
        &format!("{instance_id}.local."),
        addresses,
        bound.port(),
        properties,
    )?)
}

fn valid_location_name(location: &str) -> String {
    let location = if location.is_empty() {
        "Home"
    } else {
        location
    };
    let mut name = location.replace('.', " ");
    let mut end = name.len().min(63);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

fn bounded_property(value: &str) -> &str {
    if value.len() > 230 { "" } else { value }
}

#[cfg(test)]
#[path = "zeroconf_tests.rs"]
mod tests;
