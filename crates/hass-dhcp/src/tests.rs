use crate::{
    DHCP_DOMAIN, DHCP_SOURCE, DhcpAddressData, DhcpDiscoveryAction, DhcpIntegration, DhcpMatcher,
    DhcpMatcherIndex, DhcpObservation, DhcpServiceInfo, DhcpUpdate, DiscoveryActionSink,
    DiscoveryKey, DiscoveryKeyValue, RegisteredDeviceProvider, format_mac,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn matcher(
    domain: &str,
    hostname: Option<&str>,
    macaddress: Option<&str>,
    registered_devices: bool,
) -> DhcpMatcher {
    DhcpMatcher {
        domain: domain.to_owned(),
        hostname: hostname.map(str::to_owned),
        macaddress: macaddress.map(str::to_owned),
        registered_devices: registered_devices.then_some(true),
    }
}
fn capture_sink(actions: Arc<Mutex<Vec<DhcpDiscoveryAction>>>) -> DiscoveryActionSink {
    Arc::new(move |action| {
        actions.lock().unwrap().push(action);
        Ok(())
    })
}
#[derive(Default)]
struct TestRegistry {
    domains: Vec<String>,
    queried_macs: Arc<Mutex<Vec<String>>>,
}
impl RegisteredDeviceProvider for TestRegistry {
    fn domains_for_network_mac(&self, formatted_mac: &str) -> Vec<String> {
        self.queried_macs
            .lock()
            .unwrap()
            .push(formatted_mac.to_owned());
        self.domains.clone()
    }
}
fn discovery_key(domain: &str, version: u32, key: DiscoveryKeyValue) -> DiscoveryKey {
    DiscoveryKey {
        domain: domain.to_owned(),
        version,
        key,
    }
}
#[test]
fn cache_preserves_mac_order_source_dedup_force_and_snapshot_forms() {
    let integration = DhcpIntegration::with_matchers(DhcpMatcherIndex::new(Vec::new()), None, None);
    let manager = integration.manager();
    let updates = Arc::new(Mutex::new(Vec::<DhcpUpdate>::new()));
    let updates_for_listener = Arc::clone(&updates);
    let _guard = manager
        .register_update_listener(move |update| {
            updates_for_listener.lock().unwrap().push(update);
        })
        .unwrap();
    let first = |ip: &str, hostname: &str| DhcpObservation {
        ip: ip.to_owned(),
        hostname: hostname.to_owned(),
        mac: "AA:BB:CC:DD:EE:FF".to_owned(),
    };
    assert!(
        manager
            .ingest(first("198.51.100.1", "Node-X"), false)
            .unwrap()
            .accepted
    );
    assert!(
        manager
            .ingest(
                DhcpObservation {
                    ip: "198.51.100.2".into(),
                    hostname: "other".into(),
                    mac: "11-22-33-44-55-66".into(),
                },
                false,
            )
            .unwrap()
            .accepted
    );
    // Same compressed address and an incoming hostname already prefixed by
    // the cached hostname is a duplicate; the longer name is accepted.
    assert!(
        !manager
            .ingest(first("198.51.100.1", "Node"), false)
            .unwrap()
            .accepted
    );
    assert!(
        manager
            .ingest(first("198.51.100.1", "Node-Extra"), false)
            .unwrap()
            .accepted
    );
    assert!(
        !manager
            .ingest(first("198.51.100.1", "Node"), false)
            .unwrap()
            .accepted
    );
    // A changed IP is accepted even with a shorter hostname; force bypasses
    // the duplicate guard for config-entry removal replay.
    assert!(
        manager
            .ingest(first("198.51.100.3", "Node"), false)
            .unwrap()
            .accepted
    );
    assert!(
        manager
            .ingest(first("198.51.100.3", "Node"), true)
            .unwrap()
            .accepted
    );
    let raw = manager.address_data_snapshot().unwrap();
    assert_eq!(
        raw,
        vec![
            (
                "aabbccddeeff".into(),
                DhcpAddressData {
                    ip: "198.51.100.3".into(),
                    hostname: "Node".into(),
                },
            ),
            (
                "112233445566".into(),
                DhcpAddressData {
                    ip: "198.51.100.2".into(),
                    hostname: "other".into(),
                },
            ),
        ]
    );
    assert_eq!(
        manager.service_info_snapshot().unwrap(),
        vec![
            DhcpServiceInfo {
                ip: "198.51.100.3".into(),
                hostname: "node".into(),
                mac: "aabbccddeeff".into(),
            },
            DhcpServiceInfo {
                ip: "198.51.100.2".into(),
                hostname: "other".into(),
                mac: "112233445566".into(),
            },
        ]
    );
    assert_eq!(updates.lock().unwrap().len(), 5);
}
#[test]
fn source_ip_rejections_and_ipv6_compression_are_exact() {
    let integration = DhcpIntegration::with_matchers(DhcpMatcherIndex::new(Vec::new()), None, None);
    let manager = integration.manager();
    for ip in [
        "invalid",
        "169.254.0.1",
        "127.0.0.1",
        "0.0.0.0",
        "fe80::1",
        "::1",
        "::",
    ] {
        let result = manager
            .ingest(
                DhcpObservation {
                    ip: ip.into(),
                    hostname: "ignored".into(),
                    mac: "aa:bb:cc:dd:ee:ff".into(),
                },
                false,
            )
            .unwrap();
        assert!(!result.accepted, "unexpectedly accepted {ip}");
    }
    assert!(manager.address_data_snapshot().unwrap().is_empty());
    let raw_ip = "2001:0DB8:0000:0000:0000:0000:0000:0001";
    let report = manager
        .ingest(
            DhcpObservation {
                ip: raw_ip.into(),
                hostname: "Router".into(),
                mac: "aa:bb:cc:dd:ee:ff".into(),
            },
            false,
        )
        .unwrap();
    assert_eq!(report.service_info.unwrap().ip, raw_ip);
    assert_eq!(
        manager.address_data_snapshot().unwrap()[0].1.ip,
        "2001:db8::1"
    );
}
#[test]
fn matcher_registry_and_discovery_action_keep_source_payload_and_order() {
    let queried_macs = Arc::new(Mutex::new(Vec::new()));
    let registry: Arc<dyn RegisteredDeviceProvider> = Arc::new(TestRegistry {
        domains: vec![
            "registered-b".into(),
            "unsupported".into(),
            "registered-a".into(),
            "registered-a".into(),
        ],
        queried_macs: Arc::clone(&queried_macs),
    });
    let actions = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_sink = Arc::clone(&events);
    let actions_for_sink = Arc::clone(&actions);
    let sink: DiscoveryActionSink = Arc::new(move |action| {
        events_for_sink.lock().unwrap().push("flow".to_owned());
        actions_for_sink.lock().unwrap().push(action);
        Ok(())
    });
    let matchers = DhcpMatcherIndex::new(vec![
        matcher("registered-a", None, None, true),
        matcher("registered-b", None, None, true),
        matcher("hostname", Some("connect*"), None, false),
        matcher("hostname", Some("con[!x]*"), None, false),
        matcher("mac", None, Some("B8B7F1*"), false),
        matcher("combined", Some("connect-[0-9]"), Some("B8B7F1*"), false),
    ]);
    let integration =
        DhcpIntegration::with_matchers(matchers, Some(Arc::downgrade(&registry)), Some(sink));
    let manager = integration.manager();
    let events_for_listener = Arc::clone(&events);
    let _guard = manager
        .register_update_listener(move |_| {
            events_for_listener
                .lock()
                .unwrap()
                .push("update".to_owned());
        })
        .unwrap();
    let report = manager
        .ingest(
            DhcpObservation {
                ip: "192.168.210.56".into(),
                hostname: "Connect-7".into(),
                mac: "B8:B7:F1:6D:B5:33".into(),
            },
            false,
        )
        .unwrap();
    assert!(report.accepted);
    assert_eq!(
        report.matched_domains,
        vec![
            "registered-a",
            "registered-b",
            "hostname",
            "mac",
            "combined",
        ]
    );
    assert_eq!(*queried_macs.lock().unwrap(), vec!["b8:b7:f1:6d:b5:33"]);
    assert_eq!(events.lock().unwrap()[0], "update");
    assert_eq!(actions.lock().unwrap().len(), 5);
    let action = actions.lock().unwrap()[0].clone();
    assert_eq!(action.domain, "registered-a");
    assert_eq!(action.source, DHCP_SOURCE);
    assert!(action.context.is_empty());
    assert_eq!(action.discovery_key.domain, DHCP_DOMAIN);
    assert_eq!(action.discovery_key.version, 1);
    assert_eq!(
        action.discovery_key.key,
        DiscoveryKeyValue::String("b8b7f16db533".into())
    );
    assert_eq!(
        action.data,
        serde_json::json!({
            "ip": "192.168.210.56",
            "hostname": "connect-7",
            "macaddress": "b8b7f16db533"
        })
        .as_object()
        .unwrap()
        .clone()
    );
}
#[test]
fn rediscovery_only_replays_cached_dhcp_version_one_string_keys() {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let matchers = DhcpMatcherIndex::new(vec![matcher(
        "device",
        Some("node*"),
        Some("AABBCC*"),
        false,
    )]);
    let integration =
        DhcpIntegration::with_matchers(matchers, None, Some(capture_sink(Arc::clone(&actions))));
    let manager = integration.manager();
    manager
        .ingest(
            DhcpObservation {
                ip: "192.168.1.10".into(),
                hostname: "Node".into(),
                mac: "AA:BB:CC:DD:EE:FF".into(),
            },
            false,
        )
        .unwrap();
    let keys = vec![
        discovery_key("other", 1, DiscoveryKeyValue::String("aabbccddeeff".into())),
        discovery_key(
            DHCP_DOMAIN,
            2,
            DiscoveryKeyValue::String("aabbccddeeff".into()),
        ),
        discovery_key(
            DHCP_DOMAIN,
            1,
            DiscoveryKeyValue::Tuple(vec!["aabbccddeeff".into()]),
        ),
        discovery_key(DHCP_DOMAIN, 1, DiscoveryKeyValue::String("missing".into())),
        discovery_key(
            DHCP_DOMAIN,
            1,
            DiscoveryKeyValue::String("aabbccddeeff".into()),
        ),
    ];
    let reports = manager.rediscover(&keys).unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].accepted);
    assert_eq!(actions.lock().unwrap().len(), 2);
    assert_eq!(manager.address_data_snapshot().unwrap().len(), 1);
}
#[test]
fn update_guard_removes_only_its_listener_and_weak_manager_does_not_own_sdk() {
    let integration = DhcpIntegration::with_matchers(DhcpMatcherIndex::new(Vec::new()), None, None);
    let manager = integration.manager();
    let first_count = Arc::new(AtomicUsize::new(0));
    let second_count = Arc::new(AtomicUsize::new(0));
    let first_seen = Arc::clone(&first_count);
    let first_guard = manager
        .register_update_listener(move |_| {
            first_seen.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    let second_seen = Arc::clone(&second_count);
    let second_guard = manager
        .register_update_listener(move |_| {
            second_seen.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    let observation = |ip: &str| DhcpObservation {
        ip: ip.to_owned(),
        hostname: "host".into(),
        mac: "00:11:22:33:44:55".into(),
    };
    manager.ingest(observation("192.0.2.1"), false).unwrap();
    drop(first_guard);
    manager.ingest(observation("192.0.2.2"), false).unwrap();
    assert_eq!(first_count.load(Ordering::Relaxed), 1);
    assert_eq!(second_count.load(Ordering::Relaxed), 2);
    drop(second_guard);
    drop(integration);
    assert!(manager.service_info_snapshot().is_err());
    assert!(manager.ingest(observation("192.0.2.3"), true).is_err());
}
#[test]
fn source_mac_formatter_canonicalizes_only_documented_shapes() {
    assert_eq!(
        vec![
            format_mac("AA:BB:CC:DD:EE:FF"),
            format_mac("AA-BB-CC-DD-EE-FF"),
            format_mac("aabb.ccdd.eeff"),
            format_mac("AABBCCDDEEFF"),
            format_mac("zz:zz:zz:zz:zz:zz"),
            format_mac("AA:BB"),
        ],
        vec![
            "aa:bb:cc:dd:ee:ff".to_owned(),
            "aa:bb:cc:dd:ee:ff".to_owned(),
            "aa:bb:cc:dd:ee:ff".to_owned(),
            "aa:bb:cc:dd:ee:ff".to_owned(),
            "zz:zz:zz:zz:zz:zz".to_owned(),
            "AA:BB".to_owned(),
        ]
    );

    let integration = DhcpIntegration::with_matchers(DhcpMatcherIndex::new(Vec::new()), None, None);
    let manager = integration.manager();
    manager
        .ingest(
            DhcpObservation {
                ip: "192.0.2.10".into(),
                hostname: String::new(),
                mac: "AA:BB".into(),
            },
            false,
        )
        .unwrap();
    assert_eq!(manager.address_data_snapshot().unwrap()[0].0, "AABB");
}
