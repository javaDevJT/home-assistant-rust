use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use hass_core::{
    Error, Event, EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, Result,
    Runtime, Store,
};
use hass_network::{
    AdapterAddresses, AdapterEnumerator, Ipv4AddressRecord, Ipv6AddressRecord, NetworkAdapter,
    NetworkIntegration, NetworkManager, RouteSourceResolver, STORAGE_KEY, STORAGE_VERSION,
    ScopedIp,
};
use hass_storage::SqliteStore;
use serde_json::{Value, json};

const MDNS_TARGET: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PUBLIC_TARGET: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const LOOPBACK_TARGET: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[derive(Default)]
struct FakeEnumerator {
    adapters: Mutex<Vec<AdapterAddresses>>,
    fail: AtomicBool,
}

impl FakeEnumerator {
    fn new(adapters: Vec<AdapterAddresses>) -> Self {
        Self {
            adapters: Mutex::new(adapters),
            fail: AtomicBool::new(false),
        }
    }

    fn replace(&self, adapters: Vec<AdapterAddresses>) {
        *self.adapters.lock().unwrap() = adapters;
    }
}

impl AdapterEnumerator for FakeEnumerator {
    fn enumerate(&self) -> Result<Vec<AdapterAddresses>> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Service("injected enumeration failure".into()));
        }
        Ok(self.adapters.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct FakeResolver {
    results: Mutex<HashMap<Ipv4Addr, Option<Ipv4Addr>>>,
    calls: Mutex<Vec<Ipv4Addr>>,
}

impl FakeResolver {
    fn set(&self, target: Ipv4Addr, source: Option<Ipv4Addr>) {
        self.results.lock().unwrap().insert(target, source);
    }

    fn calls(&self) -> Vec<Ipv4Addr> {
        self.calls.lock().unwrap().clone()
    }

    fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

impl RouteSourceResolver for FakeResolver {
    fn source_ip(&self, target: Ipv4Addr) -> Option<Ipv4Addr> {
        self.calls.lock().unwrap().push(target);
        self.results.lock().unwrap().get(&target).copied().flatten()
    }
}

fn ipv4(value: &str, network_prefix: u8) -> Ipv4AddressRecord {
    Ipv4AddressRecord {
        address: value.parse().unwrap(),
        network_prefix,
    }
}

fn ipv6(value: &str, flowinfo: u32, scope_id: u32, network_prefix: u8) -> Ipv6AddressRecord {
    Ipv6AddressRecord {
        address: value.parse().unwrap(),
        flowinfo,
        scope_id,
        network_prefix,
    }
}

fn adapter(
    name: &str,
    index: Option<u32>,
    ipv4: Vec<Ipv4AddressRecord>,
    ipv6: Vec<Ipv6AddressRecord>,
) -> AdapterAddresses {
    AdapterAddresses {
        name: name.into(),
        index,
        ipv4,
        ipv6,
    }
}

fn create_integration(
    runtime: Runtime,
    enumerator: Arc<dyn AdapterEnumerator>,
    resolver: Arc<dyn RouteSourceResolver>,
) -> NetworkIntegration {
    NetworkIntegration::start(runtime, enumerator, resolver).unwrap()
}

fn create_runtime() -> (Runtime, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    (runtime, store)
}

fn adapter_named<'a>(adapters: &'a [NetworkAdapter], name: &str) -> &'a NetworkAdapter {
    adapters
        .iter()
        .find(|adapter| adapter.name == name)
        .unwrap()
}

#[test]
fn default_adapter_and_auto_fallback_follow_pinned_selection_rules() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("lo", Some(1), vec![ipv4("127.0.0.1", 8)], vec![]),
        adapter("wan", Some(2), vec![ipv4("192.0.2.8", 24)], vec![]),
        adapter("backup", Some(3), vec![ipv4("198.51.100.9", 24)], vec![]),
        adapter("link", Some(4), vec![], vec![ipv6("fe80::1", 0, 4, 64)]),
    ]));
    let resolver = Arc::new(FakeResolver::default());
    resolver.set(MDNS_TARGET, Some("192.0.2.8".parse().unwrap()));
    let owner = create_integration(runtime, enumerator, resolver);
    let manager = owner.manager();

    let adapters = manager.adapters().unwrap();
    assert!(adapter_named(&adapters, "wan").default);
    assert!(adapter_named(&adapters, "wan").auto);
    assert!(adapter_named(&adapters, "wan").enabled);
    assert!(!adapter_named(&adapters, "backup").enabled);
    assert!(!adapter_named(&adapters, "link").auto);
    assert!(!adapter_named(&adapters, "lo").auto);

    let configured = vec!["backup".into(), "missing".into(), "backup".into()];
    assert_eq!(manager.configure(configured.clone()).unwrap(), configured);
    let selected = manager.adapters().unwrap();
    assert!(!adapter_named(&selected, "wan").enabled);
    assert!(adapter_named(&selected, "backup").enabled);
    assert_eq!(manager.configured_adapters().unwrap(), configured);

    manager.configure(vec!["missing".into()]).unwrap();
    let auto_selected = manager.adapters().unwrap();
    assert!(adapter_named(&auto_selected, "wan").enabled);
    assert!(!adapter_named(&auto_selected, "backup").enabled);
}

#[test]
fn route_failure_auto_selects_every_external_adapter() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("lo", Some(1), vec![ipv4("127.0.0.1", 8)], vec![]),
        adapter("lan", Some(2), vec![ipv4("192.0.2.8", 24)], vec![]),
        adapter("wan6", Some(3), vec![], vec![ipv6("2001:db8::2", 0, 0, 64)]),
        adapter("link", Some(4), vec![], vec![ipv6("fe80::1", 0, 4, 64)]),
    ]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime, enumerator, resolver);
    let adapters = owner.manager().adapters().unwrap();

    assert!(!adapter_named(&adapters, "lo").enabled);
    assert!(adapter_named(&adapters, "lan").auto);
    assert!(adapter_named(&adapters, "lan").enabled);
    assert!(adapter_named(&adapters, "wan6").auto);
    assert!(adapter_named(&adapters, "wan6").enabled);
    assert!(!adapter_named(&adapters, "link").auto);
}

#[test]
fn reload_reorders_renames_and_applies_only_surviving_configured_names() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("en0", Some(1), vec![ipv4("192.0.2.1", 24)], vec![]),
        adapter("en1", Some(2), vec![ipv4("198.51.100.1", 24)], vec![]),
    ]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime, enumerator.clone(), resolver);
    let manager = owner.manager();
    let configured = vec!["en0".into(), "en1".into(), "missing".into(), "en0".into()];
    manager.configure(configured.clone()).unwrap();

    enumerator.replace(vec![
        adapter(
            "renamed-en1",
            Some(22),
            vec![ipv4("198.51.100.2", 25)],
            vec![],
        ),
        adapter("en0", Some(11), vec![ipv4("192.0.2.2", 25)], vec![]),
    ]);
    manager.reload_adapters().unwrap();

    let adapters = manager.adapters().unwrap();
    assert_eq!(
        adapters
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["renamed-en1", "en0"]
    );
    assert!(!adapter_named(&adapters, "renamed-en1").enabled);
    assert!(adapter_named(&adapters, "en0").enabled);
    assert_eq!(adapter_named(&adapters, "en0").index, Some(11));
    assert_eq!(manager.configured_adapters().unwrap(), configured);

    enumerator.fail.store(true, Ordering::SeqCst);
    let before = manager.adapters().unwrap();
    assert!(manager.reload_adapters().is_err());
    assert_eq!(manager.adapters().unwrap(), before);
}

#[test]
fn enabled_sources_preserve_order_duplicates_scope_zero_and_drop_flowinfo() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![adapter(
        "en0",
        Some(2),
        vec![ipv4("192.0.2.1", 24), ipv4("192.0.2.1", 24)],
        vec![ipv6("fe80::1", 77, 0, 64), ipv6("fe80::2", 98, 5, 64)],
    )]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime, enumerator, resolver);
    let sources = owner.manager().enabled_source_ips().unwrap();

    assert_eq!(
        sources,
        vec![
            ScopedIp::V4("192.0.2.1".parse().unwrap()),
            ScopedIp::V4("192.0.2.1".parse().unwrap()),
            ScopedIp::V6 {
                address: "fe80::1".parse().unwrap(),
                scope_id: 0,
            },
            ScopedIp::V6 {
                address: "fe80::2".parse().unwrap(),
                scope_id: 5,
            },
        ]
    );
    assert_eq!(sources[2].to_string(), "fe80::1%0");
    assert_eq!(
        serde_json::to_value(sources).unwrap(),
        json!(["192.0.2.1", "192.0.2.1", "fe80::1%0", "fe80::2%5"])
    );
}

#[test]
fn source_ip_uses_ordered_route_fallback_then_first_enabled_ipv4() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("lan", Some(2), vec![ipv4("192.0.2.10", 24)], vec![]),
        adapter(
            "v6-only",
            Some(3),
            vec![],
            vec![ipv6("2001:db8::1", 0, 0, 64)],
        ),
    ]));
    let resolver = Arc::new(FakeResolver::default());
    resolver.set(PUBLIC_TARGET, None);
    resolver.set(MDNS_TARGET, Some("203.0.113.99".parse().unwrap()));
    resolver.set(LOOPBACK_TARGET, Some("203.0.113.100".parse().unwrap()));
    let owner = create_integration(runtime, enumerator, resolver.clone());
    let manager = owner.manager();

    resolver.clear_calls();
    assert_eq!(
        manager.source_ip(None).unwrap(),
        "192.0.2.10".parse::<Ipv4Addr>().unwrap()
    );
    assert_eq!(resolver.calls(), vec![PUBLIC_TARGET, MDNS_TARGET]);

    resolver.set(MDNS_TARGET, None);
    resolver.set(LOOPBACK_TARGET, None);
    resolver.clear_calls();
    assert_eq!(
        manager.source_ip(None).unwrap(),
        "192.0.2.10".parse::<Ipv4Addr>().unwrap()
    );
    assert_eq!(
        resolver.calls(),
        vec![PUBLIC_TARGET, MDNS_TARGET, LOOPBACK_TARGET]
    );

    let explicit_target = "198.51.100.44".parse().unwrap();
    resolver.set(explicit_target, Some("192.0.2.10".parse().unwrap()));
    resolver.clear_calls();
    assert_eq!(
        manager.source_ip(Some(explicit_target)).unwrap(),
        "192.0.2.10".parse::<Ipv4Addr>().unwrap()
    );
    assert_eq!(resolver.calls(), vec![explicit_target]);

    manager.configure(vec!["v6-only".into()]).unwrap();
    let outside = "198.51.100.91".parse().unwrap();
    resolver.set(explicit_target, Some(outside));
    assert_eq!(manager.source_ip(Some(explicit_target)).unwrap(), outside);
    resolver.set(explicit_target, None);
    assert!(manager.source_ip(Some(explicit_target)).is_err());
}

#[test]
fn store_envelope_round_trips_with_real_memory_store_and_rejects_unwrapped_legacy_data() {
    let (runtime, store) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![adapter(
        "eth0",
        Some(1),
        vec![ipv4("192.0.2.1", 24)],
        vec![],
    )]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime.clone(), enumerator.clone(), resolver.clone());
    let manager = owner.manager();
    let configured = vec!["missing".into(), "eth0".into(), "eth0".into()];
    manager.configure(configured.clone()).unwrap();
    assert_eq!(
        runtime.get_data(STORAGE_KEY).unwrap(),
        Some(json!({
            "version": STORAGE_VERSION,
            "key": STORAGE_KEY,
            "data": {"configured_adapters": configured}
        }))
    );

    drop(owner);
    drop(runtime);
    let restored_runtime = Runtime::new(store.clone()).unwrap();
    let restored_owner = create_integration(restored_runtime, enumerator, resolver);
    assert_eq!(
        restored_owner.manager().configured_adapters().unwrap(),
        configured
    );

    let (legacy_runtime, _) = create_runtime();
    legacy_runtime
        .set_data(STORAGE_KEY, &json!({"configured_adapters": ["eth0"]}))
        .unwrap();
    let result = NetworkIntegration::start(
        legacy_runtime,
        Arc::new(FakeEnumerator::default()),
        Arc::new(FakeResolver::default()),
    );
    assert!(matches!(result, Err(Error::Storage(_))));
}

#[test]
fn store_envelope_survives_sqlite_runtime_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("network.sqlite");
    let store = Arc::new(SqliteStore::open(&database_path).unwrap());
    let runtime = Runtime::new(store.clone()).unwrap();
    let enumerator = Arc::new(FakeEnumerator::new(vec![adapter(
        "eth0",
        Some(1),
        vec![ipv4("192.0.2.1", 24)],
        vec![],
    )]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime.clone(), enumerator.clone(), resolver.clone());
    let expected = vec!["eth0".into(), "stale-name".into()];
    owner.manager().configure(expected.clone()).unwrap();

    drop(owner);
    drop(runtime);
    drop(store);

    let restored_store = Arc::new(SqliteStore::open(&database_path).unwrap());
    let restored_runtime = Runtime::new(restored_store).unwrap();
    let restored_owner = create_integration(restored_runtime, enumerator, resolver);
    assert_eq!(
        restored_owner.manager().configured_adapters().unwrap(),
        expected
    );
}

struct FailingMetadataStore {
    memory: MemoryStore,
    fail: AtomicBool,
}

impl Store for FailingMetadataStore {
    fn load_states(&self) -> Result<Vec<hass_core::State>> {
        self.memory.load_states()
    }

    fn put_state(&self, state: &hass_core::State, event: Option<&Event>) -> Result<()> {
        self.memory.put_state(state, event)
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
        self.memory.remove_state(entity_id, event)
    }

    fn append_event(&self, event: &Event) -> Result<()> {
        self.memory.append_event(event)
    }

    fn get_data(&self, key: &str) -> Result<Option<Value>> {
        self.memory.get_data(key)
    }

    fn set_data(&self, key: &str, value: &Value) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Storage("injected metadata write failure".into()));
        }
        self.memory.set_data(key, value)
    }

    fn delete_data(&self, key: &str) -> Result<bool> {
        self.memory.delete_data(key)
    }

    fn history(
        &self,
        entity_id: &str,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<hass_core::State>> {
        self.memory.history(entity_id, start, end)
    }

    fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
        self.memory.history_query(query)
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
        self.memory.event_history_query(query)
    }

    fn purge_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> Result<PurgeResult> {
        self.memory.purge_before(cutoff)
    }

    fn repack(&self) -> Result<()> {
        self.memory.repack()
    }
}

#[test]
fn failed_persistence_keeps_the_previous_configured_and_adapter_snapshot() {
    let store = Arc::new(FailingMetadataStore {
        memory: MemoryStore::default(),
        fail: AtomicBool::new(false),
    });
    let runtime = Runtime::new(store.clone()).unwrap();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("eth0", Some(1), vec![ipv4("192.0.2.1", 24)], vec![]),
        adapter("eth1", Some(2), vec![ipv4("198.51.100.1", 24)], vec![]),
    ]));
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime.clone(), enumerator, resolver);
    let manager = owner.manager();

    manager.configure(vec!["eth1".into()]).unwrap();
    let before_adapters = manager.adapters().unwrap();
    let before_names = manager.configured_adapters().unwrap();
    let before_store = runtime.get_data(STORAGE_KEY).unwrap();
    store.fail.store(true, Ordering::SeqCst);

    assert!(manager.configure(vec!["eth0".into()]).is_err());
    assert_eq!(manager.adapters().unwrap(), before_adapters);
    assert_eq!(manager.configured_adapters().unwrap(), before_names);
    assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), before_store);
}

struct ReentrantEnumerator {
    adapters: Vec<AdapterAddresses>,
    manager: Mutex<Option<NetworkManager>>,
    reenter: AtomicBool,
}

impl AdapterEnumerator for ReentrantEnumerator {
    fn enumerate(&self) -> Result<Vec<AdapterAddresses>> {
        if self.reenter.swap(false, Ordering::SeqCst) {
            self.manager
                .lock()
                .unwrap()
                .as_ref()
                .expect("manager installed")
                .configure(vec!["configured-during-enumeration".into()])?;
        }
        Ok(self.adapters.clone())
    }
}

#[test]
fn enumeration_callback_runs_without_operation_lock_and_reload_uses_latest_names() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(ReentrantEnumerator {
        adapters: vec![
            adapter("en0", Some(1), vec![ipv4("192.0.2.1", 24)], vec![]),
            adapter(
                "configured-during-enumeration",
                Some(2),
                vec![ipv4("198.51.100.1", 24)],
                vec![],
            ),
        ],
        manager: Mutex::new(None),
        reenter: AtomicBool::new(false),
    });
    let resolver = Arc::new(FakeResolver::default());
    let owner = create_integration(runtime, enumerator.clone(), resolver);
    let manager = owner.manager();
    *enumerator.manager.lock().unwrap() = Some(manager.clone());
    enumerator.reenter.store(true, Ordering::SeqCst);

    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || sender.send(manager.reload_adapters()).unwrap());
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("enumerator re-entry must not block on the operation lock")
        .unwrap();

    let manager = owner.manager();
    assert_eq!(
        manager.configured_adapters().unwrap(),
        vec!["configured-during-enumeration"]
    );
    let adapters = manager.adapters().unwrap();
    assert!(!adapter_named(&adapters, "en0").enabled);
    assert!(adapter_named(&adapters, "configured-during-enumeration").enabled);
}

struct ReentrantResolver {
    manager: Mutex<Option<NetworkManager>>,
    reenter: AtomicBool,
}

impl RouteSourceResolver for ReentrantResolver {
    fn source_ip(&self, target: Ipv4Addr) -> Option<Ipv4Addr> {
        if target == MDNS_TARGET && self.reenter.swap(false, Ordering::SeqCst) {
            self.manager
                .lock()
                .unwrap()
                .as_ref()
                .expect("manager installed")
                .configure(vec!["configured-during-route-probe".into()])
                .expect("reentrant configuration should succeed");
        }
        None
    }
}

#[test]
fn route_callback_runs_without_operation_lock_and_reload_uses_latest_names() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(FakeEnumerator::new(vec![
        adapter("en0", Some(1), vec![ipv4("192.0.2.1", 24)], vec![]),
        adapter(
            "configured-during-route-probe",
            Some(2),
            vec![ipv4("198.51.100.1", 24)],
            vec![],
        ),
    ]));
    let resolver = Arc::new(ReentrantResolver {
        manager: Mutex::new(None),
        reenter: AtomicBool::new(false),
    });
    let owner = create_integration(runtime, enumerator, resolver.clone());
    let manager = owner.manager();
    *resolver.manager.lock().unwrap() = Some(manager.clone());
    resolver.reenter.store(true, Ordering::SeqCst);

    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || sender.send(manager.reload_adapters()).unwrap());
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("route callback re-entry must not block on the operation lock")
        .unwrap();

    let manager = owner.manager();
    assert_eq!(
        manager.configured_adapters().unwrap(),
        vec!["configured-during-route-probe"]
    );
    let adapters = manager.adapters().unwrap();
    assert!(!adapter_named(&adapters, "en0").enabled);
    assert!(adapter_named(&adapters, "configured-during-route-probe").enabled);
}

struct BlockingEnumerator {
    adapter: AdapterAddresses,
    gate: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
}

impl AdapterEnumerator for BlockingEnumerator {
    fn enumerate(&self) -> Result<Vec<AdapterAddresses>> {
        if let Some((entered, release)) = self.gate.lock().unwrap().take() {
            entered.wait();
            release.wait();
        }
        Ok(vec![self.adapter.clone()])
    }
}

#[test]
fn owner_shutdown_deactivates_a_reload_that_is_still_in_provider_work() {
    let (runtime, _) = create_runtime();
    let enumerator = Arc::new(BlockingEnumerator {
        adapter: adapter("eth0", Some(1), vec![ipv4("192.0.2.1", 24)], vec![]),
        gate: Mutex::new(None),
    });
    let owner = create_integration(
        runtime,
        enumerator.clone(),
        Arc::new(FakeResolver::default()),
    );
    let manager = owner.manager();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *enumerator.gate.lock().unwrap() = Some((entered.clone(), release.clone()));

    let (reload_sender, reload_receiver) = mpsc::sync_channel(1);
    let reload_manager = manager.clone();
    std::thread::spawn(move || {
        reload_sender
            .send(reload_manager.reload_adapters())
            .unwrap()
    });
    entered.wait();

    let (shutdown_sender, shutdown_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        owner.shutdown();
        shutdown_sender.send(()).unwrap();
    });
    let shutdown_finished_before_provider_release = shutdown_receiver
        .recv_timeout(Duration::from_secs(2))
        .is_ok();
    release.wait();

    assert!(shutdown_finished_before_provider_release);
    assert!(
        reload_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("reload should finish after the provider returns")
            .is_err()
    );
    assert!(manager.adapters().is_err());
}

#[test]
fn manager_weak_handle_fails_after_owner_shutdown() {
    let (runtime, _) = create_runtime();
    let owner = create_integration(
        runtime,
        Arc::new(FakeEnumerator::default()),
        Arc::new(FakeResolver::default()),
    );
    let manager = owner.manager();

    owner.shutdown();
    assert!(manager.adapters().is_err());
    assert!(manager.configured_adapters().is_err());
}

#[test]
fn store_load_rejects_an_unknown_version_without_migration() {
    let (runtime, _) = create_runtime();
    runtime
        .set_data(
            STORAGE_KEY,
            &json!({
                "version": STORAGE_VERSION + 1,
                "key": STORAGE_KEY,
                "data": {"configured_adapters": []}
            }),
        )
        .unwrap();

    let result = NetworkIntegration::start(
        runtime,
        Arc::new(FakeEnumerator::default()),
        Arc::new(FakeResolver::default()),
    );
    assert!(matches!(result, Err(Error::Storage(_))));
}

#[test]
fn scoped_ip_json_keeps_ipv6_scope_zero() {
    let source = ScopedIp::V6 {
        address: Ipv6Addr::LOCALHOST,
        scope_id: 0,
    };
    assert_eq!(serde_json::to_value(source).unwrap(), json!("::1%0"));
    assert_eq!(
        serde_json::from_value::<ScopedIp>(json!("::1%0")).unwrap(),
        source
    );
}
