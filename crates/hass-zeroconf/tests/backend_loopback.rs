use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::thread::JoinHandle;
use std::time::Duration;

use hass_core::Result as CoreResult;
use hass_zeroconf::backend::MdnsWireOwner;
use hass_zeroconf::{RawResolvedServiceInfo, ServiceStateChange, ZeroconfBackend};
use mdns_sd::{
    DaemonStatus, IfAddr, IfOperStatus, Ifv4Addr, NetworkInterface, ServiceDaemon, ServiceInfo,
    TxtProperty, UnregisterStatus,
};
use tokio::sync::mpsc;

const SERVICE_TYPE: &str = "_wire-adapter._tcp.local.";
const HOST_NAME: &str = "wire-adapter.local.";
const APP_PORT: u16 = 8123;

#[derive(Debug)]
struct Update {
    service_type: String,
    name: String,
    state: ServiceStateChange,
    cache_present: bool,
    name_present: bool,
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
        self.daemon.as_ref().expect("test daemon still running")
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

fn custom_port() -> u16 {
    loop {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve custom UDP port");
        let port = socket.local_addr().expect("read custom UDP port").port();
        drop(socket);
        if port != mdns_sd::MDNS_PORT {
            return port;
        }
    }
}

#[tokio::test]
async fn empty_interface_snapshot_remains_idle_and_joins_without_binding_udp() {
    let port = custom_port();
    let owner = MdnsWireOwner::start(Vec::new(), port).expect("start idle daemon");
    // The explicit empty selection must never fall back to host enumeration.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        .expect("no mDNS socket is bound for an empty interface snapshot");
    owner.shutdown().await.expect("join idle daemon");
    drop(socket);
}

fn start_owner() -> (MdnsWireOwner, u16) {
    for _ in 0..20 {
        let port = custom_port();
        match MdnsWireOwner::start(loopback_snapshot(), port) {
            Ok(owner) => return (owner, port),
            Err(_) => continue,
        }
    }
    panic!("could not start an isolated loopback mDNS daemon on a custom port");
}

fn service_info(instance: &str, host_name: &str) -> ServiceInfo {
    let txt = vec![
        TxtProperty::from(("Case", b"first".as_slice())),
        TxtProperty::from(("case", b"second".as_slice())),
        TxtProperty::from("Flag"),
        TxtProperty::from(("Empty", Vec::<u8>::new())),
    ];
    ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        host_name,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        APP_PORT,
        txt,
    )
    .expect("build test advertisement")
}

fn update_sender(
    sender: mpsc::UnboundedSender<Update>,
    backend: std::sync::Arc<hass_zeroconf::backend::MdnsWireBackend>,
) -> impl Fn(&str, &str, ServiceStateChange) -> CoreResult<()> + Send + Sync + 'static {
    move |service_type, name, state| {
        let cache_present = backend.complete_cached(service_type, name).is_some();
        let name_present = backend
            .cached_names(service_type)
            .iter()
            .any(|cached| cached.eq_ignore_ascii_case(name));
        let _ = sender.send(Update {
            service_type: service_type.to_owned(),
            name: name.to_owned(),
            state,
            cache_present,
            name_present,
        });
        Ok(())
    }
}

async fn next_update_matching(
    receiver: &mut mpsc::UnboundedReceiver<Update>,
    mut predicate: impl FnMut(&Update) -> bool,
) -> Update {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let update = receiver
                .recv()
                .await
                .expect("mDNS callback channel remains open");
            if predicate(&update) {
                return update;
            }
        }
    })
    .await
    .expect("timed out waiting for an mDNS service event")
}

async fn assert_custom_port_released(port: u16) {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        .expect("custom UDP port is released after daemon joins");
    drop(socket);
}

#[test]
fn drop_fallback_requests_shutdown_and_releases_the_worker_port() {
    let (owner, port) = start_owner();
    drop(owner);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        .expect("Drop fallback shuts down and joins the daemon worker");
    drop(socket);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_browse_resolves_raw_data_notifies_waiters_and_removes_cache() {
    let (owner, port) = start_owner();
    let mut publisher = TestDaemon::start(port);
    let backend = owner.backend();
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    owner
        .start_browsing([SERVICE_TYPE], update_sender(updates_tx, owner.backend()))
        .expect("start only the requested browse");

    let instance = "wire-fixture";
    let fullname = format!("{instance}.{SERVICE_TYPE}");
    let lookup = tokio::spawn(backend.resolve(SERVICE_TYPE.to_owned(), fullname.clone()));
    publisher
        .daemon()
        .register(service_info(instance, HOST_NAME))
        .expect("register loopback service");

    let found_callback = next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Added && update.name.eq_ignore_ascii_case(&fullname)
    })
    .await;
    assert!(
        !found_callback.cache_present,
        "PTR discovery is not a resolved cache record"
    );
    assert!(
        found_callback.name_present,
        "PTR discovery appears in cached_names"
    );

    let raw: RawResolvedServiceInfo = tokio::time::timeout(Duration::from_secs(10), lookup)
        .await
        .expect("resolver waiter receives an actual ServiceResolved event")
        .expect("resolver task joins")
        .expect("backend resolver succeeds")
        .expect("service resolves");
    assert_eq!(raw.service_type, SERVICE_TYPE);
    assert_eq!(raw.name, fullname);
    assert_eq!(raw.hostname.as_deref(), Some(HOST_NAME));
    assert_eq!(raw.port, Some(APP_PORT));
    assert_eq!(raw.ip_addresses.len(), 1);
    assert_eq!(raw.ip_addresses[0].address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(raw.ip_addresses[0].scope_id, 0);
    assert_eq!(raw.txt_entries.len(), 4);
    assert_eq!(raw.txt_entries[0].key, b"Case");
    assert_eq!(raw.txt_entries[0].value.as_deref(), Some(&b"first"[..]));
    assert_eq!(raw.txt_entries[1].key, b"case");
    assert_eq!(raw.txt_entries[1].value.as_deref(), Some(&b"second"[..]));
    assert_eq!(raw.txt_entries[2].key, b"Flag");
    assert_eq!(raw.txt_entries[2].value, None);
    assert_eq!(raw.txt_entries[3].key, b"Empty");
    assert_eq!(raw.txt_entries[3].value.as_deref(), Some(&[][..]));

    assert_eq!(
        backend.complete_cached(SERVICE_TYPE, &fullname),
        Some(raw.clone())
    );
    assert_eq!(
        backend.complete_cached(SERVICE_TYPE, &fullname.to_uppercase()),
        Some(raw.clone())
    );
    assert_eq!(backend.cached_names(SERVICE_TYPE), vec![fullname.clone()]);

    let resolved_callback = next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Updated && update.cache_present
    })
    .await;
    assert_eq!(resolved_callback.name, fullname);
    assert_eq!(resolved_callback.service_type, SERVICE_TYPE);

    let unregister = publisher
        .daemon()
        .unregister(&fullname)
        .expect("queue goodbye for loopback service");
    assert!(matches!(
        unregister
            .recv_async()
            .await
            .expect("receive unregister status"),
        UnregisterStatus::OK
    ));

    let removed = next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Removed && update.name.eq_ignore_ascii_case(&fullname)
    })
    .await;
    assert!(!removed.cache_present);
    assert!(!removed.name_present);
    assert_eq!(backend.complete_cached(SERVICE_TYPE, &fullname), None);
    assert!(backend.cached_names(SERVICE_TYPE).is_empty());

    owner
        .stop_browsing()
        .await
        .expect("join browse receiver jobs");
    publisher.shutdown();
    owner
        .shutdown()
        .await
        .expect("wait for and join owner daemon thread");
    drop(owner);
    assert_custom_port_released(port).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_uses_conflict_renamed_name_and_unregistration_sends_goodbye() {
    let (owner, port) = start_owner();
    let mut peer = TestDaemon::start(port);
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    owner
        .start_browsing([SERVICE_TYPE], update_sender(updates_tx, owner.backend()))
        .expect("browse the service type before advertisements");

    let original = format!("collision.{SERVICE_TYPE}");
    peer.daemon()
        .register(service_info("collision", "peer.local."))
        .expect("peer announces the conflicting service");
    next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Updated && update.cache_present
    })
    .await;

    let mut owner_service = service_info("collision", "owner.local.");
    owner_service.set_requires_probe(false);
    let actual = owner
        .register_service(owner_service)
        .await
        .expect("conflict probing reports the actual announced fullname");
    assert_ne!(actual.to_ascii_lowercase(), original.to_ascii_lowercase());
    assert!(actual.to_ascii_lowercase().starts_with("collision (2)."));
    let owner_resolved = next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Updated
            && update.cache_present
            && update.name.eq_ignore_ascii_case(&actual)
    })
    .await;
    assert_eq!(
        owner_resolved.name.to_ascii_lowercase(),
        actual.to_ascii_lowercase()
    );
    assert!(
        owner
            .backend()
            .complete_cached(SERVICE_TYPE, &actual)
            .is_some()
    );

    owner
        .unregister_service(&actual)
        .await
        .expect("unregister by conflict-renamed fullname uses original daemon key");
    let removed = next_update_matching(&mut updates_rx, |update| {
        update.state == ServiceStateChange::Removed && update.name.eq_ignore_ascii_case(&actual)
    })
    .await;
    assert!(!removed.cache_present);
    assert!(!removed.name_present);
    assert!(
        owner
            .backend()
            .complete_cached(SERVICE_TYPE, &actual)
            .is_none()
    );
    assert!(
        owner
            .backend()
            .complete_cached(SERVICE_TYPE, &original)
            .is_some(),
        "the peer's original service remains discoverable"
    );

    let peer_unreg = peer
        .daemon()
        .unregister(&original)
        .expect("unregister peer service");
    assert!(matches!(
        peer_unreg
            .recv_async()
            .await
            .expect("receive peer unregister status"),
        UnregisterStatus::OK
    ));

    owner
        .shutdown()
        .await
        .expect("clean shutdown joins daemon worker");
    peer.shutdown();
    drop(owner);
    assert_custom_port_released(port).await;
}
