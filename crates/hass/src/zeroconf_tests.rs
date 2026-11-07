use super::*;
use crate::discovery::DiscoveryDispatcher;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult, CoreConfig,
    SetupOutcome,
};
use hass_core::{MemoryStore, Runtime};
use hass_repairs::RepairsIntegration;
use mdns_sd::{
    DaemonStatus, IfAddr, IfOperStatus, Ifv4Addr, NetworkInterface, ServiceDaemon, ServiceEvent,
    ServiceInfo, TxtProperty, UnregisterStatus,
};
use serde_json::json;
use std::{
    net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

const LOOPBACK_SERVICE_TYPE: &str = "_hap._tcp.local.";

fn loopback_snapshot() -> Vec<NetworkInterface> {
    vec![NetworkInterface {
        name: "lo".to_owned(),
        addr: IfAddr::V4(Ifv4Addr {
            ip: Ipv4Addr::LOCALHOST,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            broadcast: None,
            prefixlen: 8,
        }),
        index: Some(1),
        oper_status: IfOperStatus::Up,
        is_p2p: false,
        #[cfg(windows)]
        adapter_name: String::new(),
    }]
}

fn custom_mdns_port() -> u16 {
    loop {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve UDP port");
        let port = socket.local_addr().expect("read UDP port").port();
        drop(socket);
        if port != mdns_sd::MDNS_PORT {
            return port;
        }
    }
}

struct TestDaemon {
    daemon: Option<ServiceDaemon>,
    worker: Option<JoinHandle<()>>,
}

impl TestDaemon {
    fn start(port: u16) -> Self {
        let (daemon, worker) = ServiceDaemon::new_with_interfaces(loopback_snapshot(), port)
            .expect("start loopback daemon");
        Self {
            daemon: Some(daemon),
            worker: Some(worker),
        }
    }

    fn daemon(&self) -> &ServiceDaemon {
        self.daemon.as_ref().expect("test daemon is running")
    }

    fn shutdown(&mut self) {
        if let Some(daemon) = self.daemon.take()
            && let Ok(status) = daemon.shutdown()
        {
            assert!(matches!(
                status
                    .recv_timeout(Duration::from_secs(5))
                    .expect("daemon shutdown status"),
                DaemonStatus::Shutdown
            ));
        }
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join test daemon thread");
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn next_matching_event<F>(
    receiver: mdns_sd::Receiver<ServiceEvent>,
    mut matches: F,
) -> (mdns_sd::Receiver<ServiceEvent>, ServiceEvent)
where
    F: FnMut(&ServiceEvent) -> bool + Send + 'static,
{
    tokio::time::timeout(
        Duration::from_secs(12),
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_secs(11);
            let receiver = receiver;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let event = receiver
                    .recv_timeout(remaining)
                    .expect("mDNS event arrived before its deadline");
                if matches(&event) {
                    return (receiver, event);
                }
            }
        }),
    )
    .await
    .expect("mDNS event wait timed out")
    .expect("mDNS event waiter panicked")
}

async fn wait_for_cached_service(host: &ZeroconfHost, service_type: &str, name: &str) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if host
                .manager()
                .get_cached(service_type, name)
                .expect("read Zeroconf cache")
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("host resolved the loopback service before deadline");
}

async fn wait_for_requests(probe: &FlowProbe, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if probe.requests().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("configuration flow reached its registered native driver");
}

#[test]
fn advertisement_bounds_preserve_utf8_and_enforce_txt_value_limit() {
    let label = valid_location_name(&"🧭".repeat(16));
    assert_eq!(label.len(), 60);
    assert_eq!(label.chars().count(), 15);
    assert!(label.is_char_boundary(label.len()));

    let at_limit = "x".repeat(230);
    let over_limit = "x".repeat(231);
    assert_eq!(bounded_property(&at_limit), at_limit);
    assert_eq!(bounded_property(&over_limit), "");
}

fn host_dependencies() -> (
    Runtime,
    ConfigEntries,
    RepairsIntegration,
    DiscoveryDispatcher,
) {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).expect("create runtime");
    let entries = ConfigEntries::new(runtime.clone()).expect("create config entries");
    let repairs = RepairsIntegration::start(runtime.clone(), entries.clone(), hass_core::VERSION)
        .expect("start repairs integration");
    let dispatcher = DiscoveryDispatcher::new(entries.clone());
    (runtime, entries, repairs, dispatcher)
}

struct FlowProbe {
    domain: &'static str,
    requests: Mutex<Vec<ConfigFlowRequest>>,
}

impl FlowProbe {
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

impl ConfigEntryDriver for FlowProbe {
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
        Ok(ConfigFlowResult::form("confirm", None))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_advertises_shared_identity_bound_http_port_and_goodbye_on_loopback() {
    let (runtime, entries, _repairs, dispatcher) = host_dependencies();
    let http_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind app listener");
    let bound_address = http_listener
        .local_addr()
        .expect("read app listener address");
    let location_name = "🧭".repeat(20);
    let mut config: CoreConfig = entries.core_config();
    config.location_name = location_name.clone();
    config.external_url = Some("https://ha.example.test/".into());
    config.internal_url = Some(format!("http://127.0.0.1:{}/", bound_address.port()));
    entries
        .update_core_config(config)
        .expect("persist advertisement config");

    let expected_uuid = crate::instance_id::get(&runtime).expect("load shared instance UUID");
    let mdns_port = custom_mdns_port();
    assert_ne!(mdns_port, mdns_sd::MDNS_PORT);
    let mut host = Some(
        ZeroconfHost::start_with_transport(
            runtime.clone(),
            entries.clone(),
            _repairs.manager(),
            dispatcher.handle(),
            bound_address,
            loopback_snapshot(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            mdns_port,
        )
        .await
        .expect("start host on loopback and custom mDNS port"),
    );
    let mut peer = TestDaemon::start(mdns_port);
    let mut events = peer
        .daemon()
        .browse(HOME_ASSISTANT_SERVICE_TYPE)
        .expect("browse Home Assistant service on loopback");
    host.as_mut()
        .unwrap()
        .mark_started()
        .await
        .expect("advertise after startup");

    let expected_name = valid_location_name(&location_name);
    assert_eq!(expected_name.len(), 60, "truncate only at a UTF-8 boundary");
    let expected_fullname = format!("{expected_name}.{HOME_ASSISTANT_SERVICE_TYPE}");
    let expected_fullname_for_resolve = expected_fullname.clone();
    let (next_events, event) = next_matching_event(events, move |event| {
        matches!(event, ServiceEvent::ServiceResolved(service)
            if service.fullname.eq_ignore_ascii_case(&expected_fullname_for_resolve))
    })
    .await;
    events = next_events;
    let ServiceEvent::ServiceResolved(service) = event else {
        unreachable!("predicate selected a resolved service")
    };
    assert_eq!(service.fullname, expected_fullname);
    assert!(expected_name.len() <= 63);
    assert_eq!(service.port, bound_address.port());
    assert_eq!(
        service.get_property_val_str("uuid"),
        Some(expected_uuid.as_str())
    );
    assert_eq!(crate::instance_id::get(&runtime).unwrap(), expected_uuid);
    assert_eq!(
        service.get_property_val_str("version"),
        Some(hass_core::VERSION)
    );
    assert_eq!(
        service.get_property_val_str("location_name"),
        Some(expected_name.as_str())
    );
    assert_eq!(
        service.get_property_val_str("external_url"),
        Some("https://ha.example.test/")
    );
    assert_eq!(
        service.get_property_val_str("internal_url"),
        Some(format!("http://127.0.0.1:{}/", bound_address.port()).as_str())
    );
    assert_eq!(
        service.get_property_val_str("base_url"),
        Some("https://ha.example.test/")
    );

    host.take()
        .unwrap()
        .shutdown()
        .await
        .expect("send goodbye and join host daemon");
    let expected_fullname_for_remove = expected_fullname.clone();
    let (_events, removed) = next_matching_event(events, move |event| {
        matches!(event, ServiceEvent::ServiceRemoved(_, name)
            if name.eq_ignore_ascii_case(&expected_fullname_for_remove))
    })
    .await;
    assert!(matches!(removed, ServiceEvent::ServiceRemoved(_, _)));
    peer.shutdown();
    dispatcher.shutdown().await;
    drop(http_listener);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_homekit_service_waits_for_startup_and_goodbye_dismisses_all_source_flows() {
    let (runtime, entries, _repairs, dispatcher) = host_dependencies();
    let hue = FlowProbe::new("hue");
    let controller = FlowProbe::new("homekit_controller");
    entries.register_driver(hue.clone()).unwrap();
    entries.register_driver(controller.clone()).unwrap();

    let http_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind app listener");
    let bound_address = http_listener
        .local_addr()
        .expect("read app listener address");
    let mdns_port = custom_mdns_port();
    assert_ne!(mdns_port, mdns_sd::MDNS_PORT);
    let mut host = Some(
        ZeroconfHost::start_with_transport(
            runtime,
            entries.clone(),
            _repairs.manager(),
            dispatcher.handle(),
            bound_address,
            loopback_snapshot(),
            Vec::new(),
            mdns_port,
        )
        .await
        .expect("start loopback host without a local advert"),
    );
    let mut publisher = TestDaemon::start(mdns_port);
    let instance = "homekit Hue bridge";
    let fullname = format!("{instance}.{LOOPBACK_SERVICE_TYPE}");
    let properties = vec![
        TxtProperty::from(("md", b"BSB002".as_slice())),
        TxtProperty::from(("sf", b"0".as_slice())),
    ];
    let external_service = ServiceInfo::new(
        LOOPBACK_SERVICE_TYPE,
        instance,
        "hue-bridge.local.",
        &[IpAddr::V4(Ipv4Addr::LOCALHOST)][..],
        5688,
        properties,
    )
    .expect("construct realistic HomeKit Hue advertisement");
    publisher
        .daemon()
        .register(external_service)
        .expect("publish external loopback HomeKit device");
    wait_for_cached_service(host.as_mut().unwrap(), LOOPBACK_SERVICE_TYPE, &fullname).await;

    // The browser has resolved the real service, but admission remains held
    // until the application startup signal drains the discovery queue.
    let early_flow = tokio::time::timeout(Duration::from_millis(350), async {
        loop {
            if !hue.requests().is_empty() || !controller.requests().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        early_flow.is_err(),
        "no config flow should run before startup"
    );

    dispatcher.mark_started().await;
    wait_for_requests(&hue, 1).await;
    wait_for_requests(&controller, 1).await;
    let homekit_request = hue.requests().pop().expect("HomeKit vendor flow request");
    let ordinary_request = controller
        .requests()
        .pop()
        .expect("ordinary HomeKit controller flow request");
    let discovery_key = json!({
        "domain": "zeroconf",
        "key": [LOOPBACK_SERVICE_TYPE, fullname.clone()],
        "version": 1
    });
    assert_eq!(homekit_request.domain, "hue");
    assert_eq!(homekit_request.source, "homekit");
    assert_eq!(
        homekit_request.context.get("discovery_key"),
        Some(&discovery_key)
    );
    assert_eq!(ordinary_request.domain, "homekit_controller");
    assert_eq!(ordinary_request.source, "zeroconf");
    assert_eq!(
        ordinary_request.context.get("discovery_key"),
        Some(&discovery_key)
    );
    assert_eq!(
        ordinary_request.context.get("alternative_domain"),
        Some(&json!("hue"))
    );
    assert_ne!(homekit_request.flow_id, ordinary_request.flow_id);

    let mut protected_context = serde_json::Map::new();
    protected_context.insert("discovery_key".into(), discovery_key.clone());
    protected_context.insert("dismiss_protected".into(), json!(true));
    let protected_result = entries
        .init_config_flow_with_input(
            "homekit_controller",
            "zeroconf",
            protected_context,
            ordinary_request.user_input.clone(),
        )
        .expect("start a matching user-protected flow");
    let protected_flow_id = protected_result["flow_id"]
        .as_str()
        .expect("protected flow has an id")
        .to_owned();

    let unregister = publisher
        .daemon()
        .unregister(&fullname)
        .expect("queue the external service goodbye");
    assert!(matches!(
        unregister
            .recv_timeout(Duration::from_secs(5))
            .expect("unregister status"),
        UnregisterStatus::OK
    ));
    let ordinary_flow_id = ordinary_request.flow_id.clone();
    let homekit_flow_id = homekit_request.flow_id.clone();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let ordinary_gone = entries.flow_result(&ordinary_flow_id).is_err();
            let homekit_gone = entries.flow_result(&homekit_flow_id).is_err();
            let protected_remains = entries.flow_result(&protected_flow_id).is_ok();
            if ordinary_gone && homekit_gone && protected_remains {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("goodbye dismisses both service flows while preserving a protected flow");

    host.take()
        .unwrap()
        .shutdown()
        .await
        .expect("stop host browser and join its daemon");
    publisher.shutdown();
    dispatcher.shutdown().await;
    drop(http_listener);
}
