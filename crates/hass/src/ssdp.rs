//! Strong app ownership of the discovery scanner, flow bridge and UPnP server.

use crate::discovery::DiscoveryDispatcher;
use anyhow::{Context, Result};
use hass_config::{
    ConfigEntries, DiscoveryKeyRemoval, DiscoveryKeyRemovalObserver,
    DiscoveryKeyRemovalObserverGuard, DiscoveryKeyValue,
};
use hass_core::Runtime;
use hass_ssdp::{
    SsdpIntegration, SsdpInterface, SsdpManager, SsdpScanner, SsdpScannerOptions, UpnpDevice,
    UpnpServer, UpnpServerOptions, pinned_integration_matchers, select_sources,
};
use std::{net::SocketAddr, sync::Arc};

pub const CONFIGURATION_WARNING: &str = "The ssdp integration does not support configuration parameters; remove the parameters from configuration.";
const INSTANCE_KEY: &str = "ssdp.instance_id";

struct RemovedKeys(SsdpManager);
impl DiscoveryKeyRemovalObserver for RemovedKeys {
    fn discovery_keys_removed(&self, event: &DiscoveryKeyRemoval) -> hass_core::Result<()> {
        for key in &event.keys {
            if key.domain == "ssdp"
                && key.version == 1
                && let DiscoveryKeyValue::String(udn) = &key.key
            {
                self.0.rediscover_cached_udn(udn)?;
            }
        }
        Ok(())
    }
}

pub struct SsdpHost {
    observer: Option<DiscoveryKeyRemovalObserverGuard>,
    scanner: Option<SsdpScanner>,
    servers: Vec<UpnpServer>,
    dispatcher: Option<DiscoveryDispatcher>,
    integration: Option<SsdpIntegration>,
    entries: ConfigEntries,
    sources: Vec<SsdpInterface>,
    instance_id: String,
    started: bool,
}

impl SsdpHost {
    pub async fn start(
        runtime: Runtime,
        entries: ConfigEntries,
        network: hass_network::NetworkManager,
    ) -> Result<Self> {
        let sources = selected_sources(&network)?;
        Self::start_with_sources(runtime, entries, sources, SsdpScannerOptions::default()).await
    }

    async fn start_with_sources(
        runtime: Runtime,
        entries: ConfigEntries,
        sources: Vec<SsdpInterface>,
        options: SsdpScannerOptions,
    ) -> Result<Self> {
        let instance_id = tokio::task::spawn_blocking(move || instance_id(&runtime)).await??;
        let dispatcher = DiscoveryDispatcher::new(entries.clone());
        let integration =
            SsdpIntegration::new(pinned_integration_matchers(), Some(dispatcher.sink()))?;
        let manager = integration.manager();
        let observer =
            entries.observe_discovery_key_removals(Arc::new(RemovedKeys(manager.clone())))?;
        let scanner = SsdpScanner::start(manager, sources.clone(), options).await?;
        Ok(Self {
            observer: Some(observer),
            scanner: Some(scanner),
            servers: Vec::new(),
            dispatcher: Some(dispatcher),
            integration: Some(integration),
            entries,
            sources,
            instance_id,
            started: false,
        })
    }

    pub fn manager(&self) -> SsdpManager {
        self.integration
            .as_ref()
            .expect("active host owns integration")
            .manager()
    }

    /// Run only after main emitted the real homeassistant_started event.
    pub async fn mark_started(&mut self, bound_address: SocketAddr) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.mark_started().await;
        }
        let core_config = self.entries.core_config();
        let presentation = core_config.internal_url.or_else(|| {
            let ip = if bound_address.ip().is_unspecified() {
                self.sources.first().map(|source| source.address)
            } else {
                Some(bound_address.ip())
            }?;
            Some(format!(
                "http://{}/",
                SocketAddr::new(ip, bound_address.port())
            ))
        });
        let Some(presentation_url) = presentation else {
            tracing::warn!("No internal URL available for the SSDP advertisement server");
            return Ok(());
        };
        let udn = device_udn(&self.instance_id)?;
        let device = UpnpDevice {
            udn,
            friendly_name: format!("{} (Home Assistant)", core_config.location_name),
            model_name: "Home Assistant Rust".into(),
            model_number: hass_core::VERSION.into(),
            serial_number: self.instance_id.clone(),
            presentation_url,
            // Native frontend icon assets are not served yet. Do not advertise
            // nonexistent images; this remains an explicit compatibility gap.
            icons: Vec::new(),
        };
        let boot_id = u32::try_from(chrono::Utc::now().timestamp()).unwrap_or(u32::MAX);
        for source in &self.sources {
            let source_address = source.socket_addr(0)?;
            let target = match source_address {
                SocketAddr::V4(_) => "239.255.255.250:1900".parse().unwrap(),
                SocketAddr::V6(address) => SocketAddr::V6(std::net::SocketAddrV6::new(
                    "ff02::c".parse().unwrap(),
                    1900,
                    0,
                    address.scope_id(),
                )),
            };
            match UpnpServer::start(
                device.clone(),
                source_address,
                target,
                UpnpServerOptions {
                    boot_id,
                    ..Default::default()
                },
            )
            .await
            {
                Ok(server) => self.servers.push(server),
                Err(error) => {
                    tracing::debug!(%error, source = %source.address, "SSDP advertisement server failed to start")
                }
            }
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.observer.take();
        if let Some(scanner) = self.scanner.take() {
            scanner.shutdown().await;
        }
        let mut failure = None;
        for server in self.servers.drain(..) {
            if let Err(error) = server.shutdown().await {
                failure = Some(error);
            }
        }
        if let Some(integration) = self.integration.take() {
            integration.shutdown().await;
        }
        if let Some(dispatcher) = self.dispatcher.take() {
            dispatcher.shutdown().await;
        }
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(())
    }
}

fn selected_sources(
    network: &hass_network::NetworkManager,
) -> hass_core::Result<Vec<SsdpInterface>> {
    Ok(select_sources(
        network
            .enabled_source_ips()?
            .into_iter()
            .map(|address| match address {
                hass_network::ScopedIp::V4(address) => SsdpInterface {
                    address: address.into(),
                    index: None,
                },
                hass_network::ScopedIp::V6 { address, scope_id } => SsdpInterface {
                    address: address.into(),
                    index: Some(scope_id),
                },
            }),
    ))
}

fn instance_id(runtime: &Runtime) -> Result<String> {
    if let Some(value) = runtime.get_data(INSTANCE_KEY)? {
        let value = value
            .as_str()
            .context("invalid stored SSDP instance identity")?;
        device_udn(value)?;
        return Ok(value.into());
    }
    let value = uuid::Uuid::new_v4().simple().to_string();
    runtime.set_data(INSTANCE_KEY, &serde_json::json!(value))?;
    Ok(value)
}

fn device_udn(instance_id: &str) -> Result<String> {
    anyhow::ensure!(
        instance_id.len() == 32 && instance_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SSDP instance identity"
    );
    Ok(format!("uuid:{}", uuid::Uuid::parse_str(instance_id)?.hyphenated()).to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;
    use std::net::IpAddr;

    #[test]
    fn stable_identity_round_trips_and_malformed_storage_is_preserved() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let original = instance_id(&runtime).unwrap();
        assert_eq!(original.len(), 32);
        assert_eq!(instance_id(&runtime).unwrap(), original);
        assert_eq!(
            device_udn("12345678123456781234567812345678").unwrap(),
            "UUID:12345678-1234-5678-1234-567812345678"
        );
        assert_eq!(
            device_udn("abcdef1234567890abcdef1234567890").unwrap(),
            "UUID:ABCDEF12-3456-7890-ABCD-EF1234567890"
        );
        runtime
            .set_data(INSTANCE_KEY, &serde_json::json!("broken"))
            .unwrap();
        assert!(instance_id(&runtime).is_err());
        assert_eq!(
            runtime.get_data(INSTANCE_KEY).unwrap(),
            Some(serde_json::json!("broken"))
        );
    }

    #[test]
    fn ssdp_only_uses_selected_network_addresses_then_applies_its_own_filter() {
        struct Adapters;
        impl hass_network::AdapterEnumerator for Adapters {
            fn enumerate(&self) -> hass_core::Result<Vec<hass_network::AdapterAddresses>> {
                Ok([
                    ("first", "192.0.2.10"),
                    ("second", "198.51.100.20"),
                    ("loopback", "127.0.0.1"),
                    ("global", "8.8.8.8"),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (name, address))| hass_network::AdapterAddresses {
                    name: name.into(),
                    index: Some(index as u32 + 1),
                    ipv4: vec![hass_network::Ipv4AddressRecord {
                        address: address.parse().unwrap(),
                        network_prefix: 24,
                    }],
                    ipv6: Vec::new(),
                })
                .collect())
            }
        }
        struct Route;
        impl hass_network::RouteSourceResolver for Route {
            fn source_ip(&self, _: std::net::Ipv4Addr) -> Option<std::net::Ipv4Addr> {
                Some("192.0.2.10".parse().unwrap())
            }
        }
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let owner =
            hass_network::NetworkIntegration::start(runtime, Arc::new(Adapters), Arc::new(Route))
                .unwrap();
        let manager = owner.manager();
        assert_eq!(
            selected_sources(&manager).unwrap()[0].address,
            "192.0.2.10".parse::<IpAddr>().unwrap()
        );
        manager.configure(vec!["second".into()]).unwrap();
        assert_eq!(
            selected_sources(&manager).unwrap(),
            vec![SsdpInterface {
                address: "198.51.100.20".parse().unwrap(),
                index: None
            }]
        );
        manager
            .configure(vec!["loopback".into(), "global".into()])
            .unwrap();
        assert_eq!(manager.enabled_source_ips().unwrap().len(), 2);
        assert!(selected_sources(&manager).unwrap().is_empty());
        owner.shutdown();
        assert!(selected_sources(&manager).is_err());
    }

    #[tokio::test]
    async fn app_owner_without_enabled_interfaces_stops_and_releases_weak_manager() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let mut host = SsdpHost::start_with_sources(
            runtime,
            entries,
            Vec::new(),
            SsdpScannerOptions::default(),
        )
        .await
        .unwrap();
        let manager = host.manager();
        host.mark_started(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            8123,
        ))
        .await
        .unwrap();
        assert!(manager.get_by_udn("uuid:missing").await.unwrap().is_empty());
        host.shutdown().await.unwrap();
        assert!(manager.get_by_udn("uuid:missing").await.is_err());
    }

    #[tokio::test]
    async fn sqlite_identity_survives_a_new_host_without_retaining_the_old_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.sqlite");
        let mut original = None;
        for _ in 0..2 {
            let runtime =
                Runtime::new(Arc::new(hass_storage::SqliteStore::open(&path).unwrap())).unwrap();
            let entries = ConfigEntries::new(runtime.clone()).unwrap();
            let host = SsdpHost::start_with_sources(
                runtime.clone(),
                entries,
                Vec::new(),
                SsdpScannerOptions::default(),
            )
            .await
            .unwrap();
            let identity = instance_id(&runtime).unwrap();
            if let Some(original) = &original {
                assert_eq!(&identity, original);
            } else {
                original = Some(identity);
            }
            let manager = host.manager();
            host.shutdown().await.unwrap();
            assert!(manager.get_by_udn("uuid:missing").await.is_err());
            drop(runtime);
        }
    }
}
