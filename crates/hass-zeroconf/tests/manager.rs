use std::{
    collections::HashMap,
    future::pending,
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use hass_config::{DiscoveryKey, DiscoveryKeyValue};
use hass_core::{Error, Result};
use hass_zeroconf::{
    DiscoveryAction, DiscoveryActionSink, DiscoverySource, MatcherIndex, RawResolvedServiceInfo,
    RawTxtEntry, ResolverFuture, ScopedIpAddress, ServiceStateChange, ZeroconfBackend,
    ZeroconfIntegration,
};
use tokio::sync::Notify;

#[derive(Clone)]
struct FakeBackend {
    state: Arc<FakeBackendState>,
}

#[derive(Default)]
struct FakeBackendState {
    cache: Mutex<HashMap<(String, String), RawResolvedServiceInfo>>,
    names: Mutex<HashMap<String, Vec<String>>>,
    requests: Mutex<Vec<(String, String)>>,
    response: Mutex<FakeResponse>,
    resolve_calls: AtomicUsize,
    future_polls: AtomicUsize,
}

#[derive(Clone, Default)]
enum FakeResponse {
    #[default]
    None,
    Immediate(Option<RawResolvedServiceInfo>),
    Pending(Option<Arc<AtomicBool>>),
    WaitForNotify(Arc<Notify>, RawResolvedServiceInfo),
    Fail(String),
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            state: Arc::new(FakeBackendState::default()),
        }
    }

    fn with_cached(records: impl IntoIterator<Item = RawResolvedServiceInfo>) -> Self {
        let backend = Self::new();
        for record in records {
            backend.insert(record);
        }
        backend
    }

    fn insert(&self, record: RawResolvedServiceInfo) {
        let key = (record.service_type.clone(), record.name.clone());
        self.state.cache.lock().unwrap().insert(key, record.clone());
        let mut names = self.state.names.lock().unwrap();
        let service_names = names.entry(record.service_type).or_default();
        if !service_names.contains(&record.name) {
            service_names.push(record.name);
        }
    }

    fn set_names(&self, service_type: &str, names: &[&str]) {
        self.state.names.lock().unwrap().insert(
            service_type.to_owned(),
            names.iter().map(|name| (*name).to_owned()).collect(),
        );
    }

    fn set_response(&self, response: FakeResponse) {
        *self.state.response.lock().unwrap() = response;
    }

    fn resolve_calls(&self) -> usize {
        self.state.resolve_calls.load(Ordering::SeqCst)
    }

    fn future_polls(&self) -> usize {
        self.state.future_polls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.state.requests.lock().unwrap().clone()
    }
}

impl ZeroconfBackend for FakeBackend {
    fn complete_cached(&self, service_type: &str, name: &str) -> Option<RawResolvedServiceInfo> {
        self.state
            .cache
            .lock()
            .unwrap()
            .get(&(service_type.to_owned(), name.to_owned()))
            .cloned()
    }

    fn cached_names(&self, service_type: &str) -> Vec<String> {
        self.state
            .names
            .lock()
            .unwrap()
            .get(service_type)
            .cloned()
            .unwrap_or_default()
    }

    fn resolve(&self, service_type: String, name: String) -> ResolverFuture {
        self.state.resolve_calls.fetch_add(1, Ordering::SeqCst);
        self.state
            .requests
            .lock()
            .unwrap()
            .push((service_type, name));
        let state = Arc::clone(&self.state);
        let response = self.state.response.lock().unwrap().clone();
        Box::pin(async move {
            state.future_polls.fetch_add(1, Ordering::SeqCst);
            match response {
                FakeResponse::None => Ok(None),
                FakeResponse::Immediate(record) => Ok(record),
                FakeResponse::Pending(drop_flag) => {
                    let _drop_signal = drop_flag.map(DropSignal);
                    pending::<Result<Option<RawResolvedServiceInfo>>>().await
                }
                FakeResponse::WaitForNotify(notify, record) => {
                    notify.notified().await;
                    Ok(Some(record))
                }
                FakeResponse::Fail(message) => Err(Error::Service(message)),
            }
        })
    }
}

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn record(
    service_type: &str,
    name: &str,
    addresses: Vec<ScopedIpAddress>,
) -> RawResolvedServiceInfo {
    RawResolvedServiceInfo {
        ip_addresses: addresses,
        port: Some(80),
        hostname: Some("host.local.".to_owned()),
        service_type: service_type.to_owned(),
        name: name.to_owned(),
        txt_entries: vec![RawTxtEntry {
            key: b"model".to_vec(),
            value: Some(b"demo".to_vec()),
        }],
    }
}

fn usable_record(service_type: &str, name: &str) -> RawResolvedServiceInfo {
    record(
        service_type,
        name,
        vec![ScopedIpAddress::new(
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)),
            0,
        )],
    )
}

fn matcher_index(service_type: &str) -> MatcherIndex {
    MatcherIndex::from_json(&format!(r#"{{"{service_type}":[{{"domain":"demo"}}]}}"#)).unwrap()
}

fn capture_sink() -> (DiscoveryActionSink, Arc<Mutex<Vec<DiscoveryAction>>>) {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let capture = Arc::clone(&actions);
    let sink: DiscoveryActionSink = Arc::new(move |action| {
        capture.lock().unwrap().push(action);
        Ok(())
    });
    (sink, actions)
}

fn integration(
    backend: Arc<FakeBackend>,
    matchers: MatcherIndex,
    sink: Option<DiscoveryActionSink>,
) -> ZeroconfIntegration {
    ZeroconfIntegration::with_matchers(backend, matchers, sink, tokio::runtime::Handle::current())
}

#[tokio::test]
async fn complete_cache_hit_is_synchronous_and_does_not_resolve() {
    let service_type = "_demo._tcp.local.";
    let item = usable_record(service_type, "device._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::with_cached([item.clone()]));
    let (sink, actions) = capture_sink();
    let owner = integration(
        Arc::clone(&backend),
        matcher_index(service_type),
        Some(sink),
    );
    let manager = owner.manager();

    manager
        .service_update(service_type, &item.name, ServiceStateChange::Added)
        .unwrap();

    assert_eq!(backend.resolve_calls(), 0);
    let actions = actions.lock().unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        DiscoveryAction::Create {
            domain,
            source: DiscoverySource::Zeroconf,
            discovery_key: DiscoveryKey {
                domain: key_domain,
                key: DiscoveryKeyValue::Tuple(key),
                version: 1,
            },
            info,
            alternative_domain: None,
        } if domain == "demo"
            && key_domain == "zeroconf"
            && key == &[service_type.to_owned(), item.name.clone()]
            && info.service_type == service_type
            && info.name == item.name
    ));
}

#[tokio::test(start_paused = true)]
async fn incomplete_cache_miss_uses_owned_ten_second_lookup() {
    let service_type = "_demo._tcp.local.";
    let drop_flag = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(FakeBackend::new());
    backend.set_names(service_type, &["partial._demo._tcp.local."]);
    backend.set_response(FakeResponse::Pending(Some(Arc::clone(&drop_flag))));
    let owner = integration(Arc::clone(&backend), matcher_index(service_type), None);
    let manager = owner.manager();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_listener = Arc::clone(&seen);
    let _listener = manager
        .register_service_update_listener(move |_| {
            seen_listener.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

    manager
        .service_update(
            service_type,
            "partial._demo._tcp.local.",
            ServiceStateChange::Added,
        )
        .unwrap();
    assert_eq!(backend.resolve_calls(), 0, "the transferred future is cold");

    tokio::task::yield_now().await;
    assert_eq!(backend.resolve_calls(), 1);
    assert_eq!(backend.future_polls(), 1);
    assert_eq!(
        backend.requests(),
        vec![(
            service_type.to_owned(),
            "partial._demo._tcp.local.".to_owned()
        )]
    );
    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    assert!(
        drop_flag.load(Ordering::SeqCst),
        "timeout drops the pending future"
    );

    owner.shutdown().await;
}

#[tokio::test]
async fn raw_update_listeners_observe_unusable_records_before_address_filtering() {
    let service_type = "_demo._tcp.local.";
    let invalid = record(service_type, "no-address._demo._tcp.local.", Vec::new());
    let backend = Arc::new(FakeBackend::with_cached([invalid.clone()]));
    let (sink, actions) = capture_sink();
    let owner = integration(
        Arc::clone(&backend),
        matcher_index(service_type),
        Some(sink),
    );
    let manager = owner.manager();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_listener = Arc::clone(&observed);
    let _listener = manager
        .register_service_update_listener(move |raw| {
            observed_listener.lock().unwrap().push(raw.clone());
            Ok(())
        })
        .unwrap();

    manager
        .service_update(service_type, &invalid.name, ServiceStateChange::Updated)
        .unwrap();

    assert_eq!(*observed.lock().unwrap(), vec![invalid]);
    assert!(actions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn removal_is_name_scoped_and_keeps_the_backend_cache_untouched() {
    let cached = usable_record("_first._tcp.local.", "shared-name");
    let backend = Arc::new(FakeBackend::with_cached([cached.clone()]));
    let (sink, actions) = capture_sink();
    let owner = integration(Arc::clone(&backend), MatcherIndex::default(), Some(sink));
    let manager = owner.manager();
    let removed = Arc::new(Mutex::new(Vec::new()));
    let removed_listener = Arc::clone(&removed);
    let _listener = manager
        .register_service_removed_listener(move |name| {
            removed_listener.lock().unwrap().push(name.to_owned());
            Ok(())
        })
        .unwrap();

    manager
        .service_update(
            "_different._udp.local.",
            "shared-name",
            ServiceStateChange::Removed,
        )
        .unwrap();

    assert_eq!(*removed.lock().unwrap(), vec!["shared-name"]);
    assert_eq!(
        manager
            .get_cached(&cached.service_type, &cached.name)
            .unwrap(),
        Some(cached)
    );
    assert_eq!(
        *actions.lock().unwrap(),
        vec![DiscoveryAction::Dismiss {
            name: "shared-name".to_owned(),
            source: DiscoverySource::Zeroconf,
        }]
    );
}

#[tokio::test]
async fn rediscovery_reuses_cache_then_owned_lookup_without_publishing_events() {
    let service_type = "_demo._tcp.local.";
    let cached = usable_record(service_type, "cached._demo._tcp.local.");
    let resolved = usable_record(service_type, "missed._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::with_cached([cached.clone()]));
    let notify = Arc::new(Notify::new());
    backend.set_response(FakeResponse::WaitForNotify(
        Arc::clone(&notify),
        resolved.clone(),
    ));
    let (sink, actions) = capture_sink();
    let owner = integration(
        Arc::clone(&backend),
        matcher_index(service_type),
        Some(sink),
    );
    let manager = owner.manager();

    manager.rediscover(service_type, &cached.name).unwrap();
    assert_eq!(backend.resolve_calls(), 0);
    manager.rediscover(service_type, &resolved.name).unwrap();
    assert_eq!(
        backend.resolve_calls(),
        0,
        "lookup starts in the owned task"
    );
    tokio::task::yield_now().await;
    assert_eq!(backend.resolve_calls(), 1);
    notify.notify_one();
    tokio::task::yield_now().await;

    let actions = actions.lock().unwrap();
    assert_eq!(actions.len(), 2);
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, DiscoveryAction::Create { .. }))
    );
    assert_eq!(backend.resolve_calls(), 1);
}

#[tokio::test]
async fn dropped_guard_removes_only_its_listener() {
    let service_type = "_demo._tcp.local.";
    let item = usable_record(service_type, "guarded._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::with_cached([item.clone()]));
    let owner = integration(Arc::clone(&backend), matcher_index(service_type), None);
    let manager = owner.manager();
    let first_count = Arc::new(AtomicUsize::new(0));
    let second_count = Arc::new(AtomicUsize::new(0));
    let first = Arc::clone(&first_count);
    let first_guard = manager
        .register_service_update_listener(move |_| {
            first.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    let second = Arc::clone(&second_count);
    let _second_guard = manager
        .register_service_update_listener(move |_| {
            second.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    drop(first_guard);

    manager
        .service_update(service_type, &item.name, ServiceStateChange::Added)
        .unwrap();

    assert_eq!(first_count.load(Ordering::SeqCst), 0);
    assert_eq!(second_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_thread_callback_uses_captured_runtime_handle() {
    let service_type = "_demo._tcp.local.";
    let resolved = usable_record(service_type, "threaded._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::new());
    let notify = Arc::new(Notify::new());
    backend.set_response(FakeResponse::WaitForNotify(
        Arc::clone(&notify),
        resolved.clone(),
    ));
    let owner = integration(Arc::clone(&backend), matcher_index(service_type), None);
    let manager = owner.manager();

    let callback_manager = manager.clone();
    std::thread::spawn(move || {
        callback_manager.service_update(
            service_type,
            "threaded._demo._tcp.local.",
            ServiceStateChange::Added,
        )
    })
    .join()
    .unwrap()
    .unwrap();

    tokio::task::yield_now().await;
    assert_eq!(backend.resolve_calls(), 1);
    notify.notify_one();
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_aborts_and_joins_owned_resolver_future() {
    let service_type = "_demo._tcp.local.";
    let drop_flag = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(FakeBackend::new());
    backend.set_response(FakeResponse::Pending(Some(Arc::clone(&drop_flag))));
    let owner = integration(Arc::clone(&backend), matcher_index(service_type), None);
    let manager = owner.manager();

    manager
        .service_update(
            service_type,
            "pending._demo._tcp.local.",
            ServiceStateChange::Added,
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.resolve_calls(), 1);
    assert_eq!(backend.future_polls(), 1);

    owner.shutdown().await;

    assert!(drop_flag.load(Ordering::SeqCst));
    assert!(
        manager
            .service_update(
                service_type,
                "late._demo._tcp.local.",
                ServiceStateChange::Added,
            )
            .is_err()
    );
    assert!(
        manager
            .register_service_removed_listener(|_| Ok(()))
            .is_err()
    );
}

#[tokio::test]
async fn cache_snapshot_walks_generated_types_but_browser_includes_special_types() {
    let generated = "_generated._tcp.local.";
    let special = "_hap._tcp.local.";
    let generated_record = usable_record(generated, "generated._generated._tcp.local.");
    let special_record = usable_record(special, "special._hap._tcp.local.");
    let backend = Arc::new(FakeBackend::with_cached([
        generated_record.clone(),
        special_record,
    ]));
    let owner = integration(Arc::clone(&backend), matcher_index(generated), None);
    let manager = owner.manager();

    assert_eq!(manager.service_types().unwrap(), vec![generated]);
    let browser_types = manager.browse_types().unwrap();
    assert!(browser_types.contains(&generated.to_owned()));
    assert!(browser_types.contains(&"_hap._tcp.local.".to_owned()));
    assert!(browser_types.contains(&"_hap._udp.local.".to_owned()));
    assert!(browser_types.contains(&"_home-assistant._tcp.local.".to_owned()));
    assert_eq!(
        manager.snapshot_names().unwrap(),
        vec![(generated.to_owned(), generated_record.name.clone())]
    );
    assert_eq!(manager.cached_snapshot().unwrap(), vec![generated_record]);
}

#[tokio::test]
async fn callback_errors_and_panics_do_not_break_other_listeners_or_dispatch() {
    let service_type = "_demo._tcp.local.";
    let item = usable_record(service_type, "resilient._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::with_cached([item.clone()]));
    let (sink, actions) = capture_sink();
    let owner = integration(
        Arc::clone(&backend),
        matcher_index(service_type),
        Some(sink),
    );
    let manager = owner.manager();
    let observed = Arc::new(AtomicUsize::new(0));
    let _error_listener = manager
        .register_service_update_listener(|_| Err(Error::Service("listener detail".to_owned())))
        .unwrap();
    let _panic_listener = manager
        .register_service_update_listener(|_| panic!("listener panic detail"))
        .unwrap();
    let observed_listener = Arc::clone(&observed);
    let _good_listener = manager
        .register_service_update_listener(move |_| {
            observed_listener.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

    manager
        .service_update(service_type, &item.name, ServiceStateChange::Added)
        .unwrap();

    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(actions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn public_resolve_returns_records_and_preserves_backend_errors() {
    let service_type = "_demo._tcp.local.";
    let item = usable_record(service_type, "direct._demo._tcp.local.");
    let backend = Arc::new(FakeBackend::new());
    backend.set_response(FakeResponse::Immediate(Some(item.clone())));
    let owner = integration(Arc::clone(&backend), matcher_index(service_type), None);
    let manager = owner.manager();

    assert_eq!(
        manager.resolve(service_type, &item.name).await.unwrap(),
        Some(item.clone())
    );
    assert_eq!(
        backend.requests(),
        vec![(service_type.to_owned(), item.name.clone())]
    );

    backend.set_response(FakeResponse::Fail("resolver detail".to_owned()));
    let error = manager.resolve(service_type, &item.name).await.unwrap_err();
    assert_eq!(error.to_string(), "service error: resolver detail");
}
