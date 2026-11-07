use super::*;
use crate::discovery::DiscoveryDispatcher;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    DiscoveryKey, DiscoveryKeyValue, NewConfigEntry, SetupOutcome,
};
use hass_core::{Context, MemoryStore};
use hass_registry::{DeviceRegistration, Registries};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

fn host_dependencies() -> (Runtime, ConfigEntries, Registries, DiscoveryDispatcher) {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime");
    let entries = ConfigEntries::new(runtime.clone()).expect("create config entries");
    let registries = Registries::new(runtime.clone()).expect("create registries");
    let dispatcher = DiscoveryDispatcher::new(entries.clone());
    (runtime, entries, registries, dispatcher)
}

fn tracker_attributes(
    source_type: &str,
    ip: &str,
    host_name: &str,
    mac: &str,
) -> Map<String, Value> {
    json!({
        "source_type": source_type,
        "ip": ip,
        "host_name": host_name,
        "mac": mac,
    })
    .as_object()
    .expect("tracker attributes are an object")
    .clone()
}

async fn wait_for_cache_len(manager: &hass_dhcp::DhcpManager, len: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if manager
                .address_data_snapshot()
                .expect("active DHCP cache")
                .len()
                >= len
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("DHCP observer should process the state event");
}

async fn wait_for_flow_count(driver: &CreateEntryProbe, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if driver.requests().len() >= count {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discovery queue should call the registered driver");
}

async fn wait_for_keyed_entry(entries: &ConfigEntries, key: &DiscoveryKey) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !entries.entries_for_discovery_key(key).is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discovery flow should persist its typed key on the entry");
}

struct CreateEntryProbe {
    domain: &'static str,
    requests: Mutex<Vec<ConfigFlowRequest>>,
}

impl CreateEntryProbe {
    fn new(domain: &'static str) -> Arc<Self> {
        Arc::new(Self {
            domain,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ConfigFlowRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ConfigEntryDriver for CreateEntryProbe {
    fn domain(&self) -> &str {
        self.domain
    }

    fn setup(&self, _: &Runtime, _: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _: &Runtime, _: &ConfigEntry) -> hass_core::Result<()> {
        Ok(())
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        Ok(ConfigFlowResult::create_entry(
            "DHCP fixture",
            request.user_input.clone().unwrap_or_default(),
            Some("dhcp-fixture-device".into()),
        ))
    }
}

#[tokio::test]
async fn tracker_observation_uses_initial_home_router_states_and_only_new_matching_states() {
    let (runtime, entries, registries, dispatcher) = host_dependencies();
    runtime
        .set_state(
            "device_tracker.existing_router",
            "home",
            tracker_attributes("router", "192.0.2.10", "Existing-Node", "AA:BB:CC:DD:EE:01"),
            Context::default(),
            false,
        )
        .unwrap();
    runtime
        .set_state(
            "device_tracker.not_home",
            "not_home",
            tracker_attributes("router", "192.0.2.11", "Away-Node", "AA:BB:CC:DD:EE:02"),
            Context::default(),
            false,
        )
        .unwrap();
    runtime
        .set_state(
            "device_tracker.non_router",
            "home",
            tracker_attributes("gps", "192.0.2.12", "GPS-Node", "AA:BB:CC:DD:EE:03"),
            Context::default(),
            false,
        )
        .unwrap();

    let host = DhcpHost::start(runtime.clone(), entries, registries, dispatcher.handle())
        .expect("start DHCP observation host");
    let manager = host.manager();
    assert_eq!(
        manager.address_data_snapshot().unwrap(),
        vec![(
            "aabbccddee01".into(),
            hass_dhcp::DhcpAddressData {
                ip: "192.0.2.10".into(),
                hostname: "Existing-Node".into(),
            },
        )]
    );

    runtime
        .set_state(
            "device_tracker.existing_router",
            "home",
            tracker_attributes("router", "192.0.2.99", "Changed-Node", "AA:BB:CC:DD:EE:09"),
            Context::default(),
            true,
        )
        .unwrap();
    runtime
        .set_state(
            "device_tracker.added_non_router",
            "home",
            tracker_attributes("gps", "192.0.2.13", "Another-GPS", "AA:BB:CC:DD:EE:04"),
            Context::default(),
            false,
        )
        .unwrap();
    runtime
        .set_state(
            "device_tracker.added_not_home",
            "not_home",
            tracker_attributes("router", "192.0.2.14", "Another-Away", "AA:BB:CC:DD:EE:05"),
            Context::default(),
            false,
        )
        .unwrap();
    // This matching addition is an event-order barrier for the ignored update
    // and the two non-matching additions above it on the same runtime bus.
    runtime
        .set_state(
            "device_tracker.added_router",
            "home",
            tracker_attributes("router", "2001:0db8::3", "Added-Node", "AA:BB:CC:DD:EE:06"),
            Context::default(),
            false,
        )
        .unwrap();
    wait_for_cache_len(&manager, 2).await;

    assert_eq!(
        manager.address_data_snapshot().unwrap(),
        vec![
            (
                "aabbccddee01".into(),
                hass_dhcp::DhcpAddressData {
                    ip: "192.0.2.10".into(),
                    hostname: "Existing-Node".into(),
                },
            ),
            (
                "aabbccddee06".into(),
                hass_dhcp::DhcpAddressData {
                    ip: "2001:db8::3".into(),
                    hostname: "Added-Node".into(),
                },
            ),
        ]
    );

    host.shutdown().await.expect("join DHCP observation jobs");
    assert!(manager.address_data_snapshot().is_err());
    dispatcher.shutdown().await;
}

#[tokio::test]
async fn connected_device_publisher_uses_private_signal_and_ignores_missing_identity_fields() {
    let (runtime, entries, registries, dispatcher) = host_dependencies();
    let host = DhcpHost::start(runtime.clone(), entries, registries, dispatcher.handle())
        .expect("start DHCP observation host");
    let manager = host.manager();
    let mut public_events = runtime.subscribe_with_states().1;
    let mut private_signals = runtime.subscribe_signals();

    for (device, missing_field) in [
        (
            hass_device_tracker::ConnectedDevice {
                ip: None,
                host_name: Some("missing-ip".into()),
                mac: Some("AA:BB:CC:DD:EE:10".into()),
            },
            "ip",
        ),
        (
            hass_device_tracker::ConnectedDevice {
                ip: Some("198.51.100.11".into()),
                host_name: Some("missing-mac".into()),
                mac: None,
            },
            "mac",
        ),
        (
            hass_device_tracker::ConnectedDevice {
                ip: Some("198.51.100.12".into()),
                host_name: None,
                mac: Some("AA:BB:CC:DD:EE:12".into()),
            },
            "host_name",
        ),
    ] {
        hass_device_tracker::dispatch_connected_device(&runtime, &device, Context::default())
            .expect("publish private connected-device signal");
        let event = private_signals
            .recv()
            .await
            .expect("receive private connected-device signal");
        assert_eq!(
            event.event_type,
            hass_device_tracker::CONNECTED_DEVICE_REGISTERED
        );
        if missing_field == "ip" || missing_field == "mac" {
            assert!(event.data[missing_field].is_null());
        }
    }

    wait_for_cache_len(&manager, 1).await;
    assert_eq!(
        manager.address_data_snapshot().unwrap(),
        vec![(
            "aabbccddee12".into(),
            hass_dhcp::DhcpAddressData {
                ip: "198.51.100.12".into(),
                hostname: String::new(),
            },
        )]
    );
    assert!(matches!(
        public_events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    host.shutdown().await.expect("join DHCP observation jobs");
    dispatcher.shutdown().await;
}

#[tokio::test]
async fn registered_device_matching_collects_domains_from_each_native_registry_row() {
    let (runtime, entries, registries, dispatcher) = host_dependencies();
    let esphome = entries
        .create_entry(NewConfigEntry::new(
            "esphome",
            "ESPHome fixture",
            Map::new(),
        ))
        .unwrap();
    let roomba = entries
        .create_entry(NewConfigEntry::new("roomba", "Roomba fixture", Map::new()))
        .unwrap();
    let unsupported = entries
        .create_entry(NewConfigEntry::new(
            "custom_fixture",
            "Unsupported fixture",
            Map::new(),
        ))
        .unwrap();
    let same_mac = BTreeSet::from([("mac".into(), "02:00:00:00:00:10".into())]);
    for entry in [&esphome, &roomba, &unsupported] {
        registries
            .get_or_create_device(DeviceRegistration {
                config_entry_id: Some(entry.entry_id.clone()),
                connections: same_mac.clone(),
                ..Default::default()
            })
            .unwrap();
    }
    assert_eq!(registries.devices().len(), 3);

    let host = DhcpHost::start(runtime, entries, registries, dispatcher.handle())
        .expect("start DHCP observation host");
    let report = host
        .manager()
        .ingest(
            hass_dhcp::DhcpObservation {
                ip: "198.51.100.10".into(),
                hostname: String::new(),
                mac: "02:00:00:00:00:10".into(),
            },
            false,
        )
        .unwrap();

    assert!(report.accepted);
    assert_eq!(report.matched_domains, ["esphome", "roomba"]);
    host.shutdown().await.expect("join DHCP observation jobs");
    dispatcher.shutdown().await;
}

#[tokio::test]
async fn queued_roomba_flow_persists_typed_key_and_replays_it_after_entry_removal() {
    let (runtime, entries, registries, dispatcher) = host_dependencies();
    let driver = CreateEntryProbe::new("roomba");
    entries.register_driver(driver.clone()).unwrap();
    let mut host = DhcpHost::start(
        runtime.clone(),
        entries.clone(),
        registries,
        dispatcher.handle(),
    )
    .expect("start DHCP observation host");
    host.observe_removed_keys()
        .expect("observe keyed config-entry removals");
    let manager = host.manager();
    let report = manager
        .ingest(
            hass_dhcp::DhcpObservation {
                ip: "192.0.2.50".into(),
                hostname: "irobot-test".into(),
                mac: "50:14:79:00:00:01".into(),
            },
            false,
        )
        .unwrap();
    assert_eq!(report.matched_domains, ["roomba"]);
    assert!(driver.requests().is_empty(), "candidate waits for startup");

    dispatcher.mark_started().await;
    let key = DiscoveryKey {
        domain: hass_dhcp::DHCP_DOMAIN.into(),
        key: DiscoveryKeyValue::String("501479000001".into()),
        version: 1,
    };
    let entries_for_key = entries.entries_for_discovery_key(&key);
    assert_eq!(entries_for_key.len(), 1);
    let first_entry = &entries_for_key[0];
    assert_eq!(first_entry.domain, "roomba");
    assert_eq!(
        first_entry.discovery_keys["dhcp"].as_slice(),
        std::slice::from_ref(&key)
    );
    let persisted = serde_json::to_value(first_entry).unwrap();
    assert_eq!(
        persisted["discovery_keys"]["dhcp"][0],
        json!({
            "domain": "dhcp",
            "key": "501479000001",
            "version": 1,
        })
    );

    // Read the persisted config-entry document, proving the typed key crossed
    // the storage boundary without constructing a second manager for this
    // Runtime.
    let persisted_document = runtime
        .get_data("hass.config_entries.v1")
        .unwrap()
        .expect("config-entry document is persisted");
    let persisted_entry: ConfigEntry = serde_json::from_value(
        persisted_document["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["entry_id"] == first_entry.entry_id)
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_eq!(
        persisted_entry.discovery_keys["dhcp"].as_slice(),
        std::slice::from_ref(&key)
    );

    entries.remove_entry(&first_entry.entry_id).unwrap();
    dispatcher.mark_started().await;
    wait_for_flow_count(&driver, 2).await;
    wait_for_keyed_entry(&entries, &key).await;
    assert_eq!(
        driver
            .requests()
            .iter()
            .map(|request| request.source.as_str())
            .collect::<Vec<_>>(),
        ["dhcp", "dhcp"]
    );
    assert_eq!(
        entries.entries_for_discovery_key(&key).len(),
        1,
        "removal replay should create one replacement flow entry"
    );
    assert_eq!(manager.address_data_snapshot().unwrap().len(), 1);

    host.shutdown().await.expect("join DHCP observation jobs");
    dispatcher.shutdown().await;
}
