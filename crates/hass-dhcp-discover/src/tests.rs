use std::collections::HashMap;
use std::future::pending;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{Notify, watch};

use crate::dns::{UdpPtrResolver, parse_ptr_response, test_build_ptr_query};
use crate::system::{
    test_mask_prefix, test_parse_proc_neighbors, test_parse_proc_routes, test_parse_resolv_conf,
};
use crate::{
    HostObservation, Ipv4Subnet, MAX_ADDRESSES, NeighborEntry, NetworkProvider, NetworkSnapshot,
    PtrResolver, ScanConfig, ScanError, ScanStatus, Scanner,
};

#[derive(Clone)]
struct FakeNetwork {
    snapshot: NetworkSnapshot,
    neighbors: Vec<NeighborEntry>,
    reads: Arc<Mutex<Vec<Vec<Ipv4Addr>>>>,
    probes: Arc<Mutex<Vec<Vec<Ipv4Addr>>>>,
    events: Arc<Mutex<Vec<String>>>,
    probe_signal: Option<Arc<Notify>>,
    revision: Arc<Mutex<Option<crate::ResolverConfigRevision>>>,
}

impl FakeNetwork {
    fn new(snapshot: NetworkSnapshot, neighbors: Vec<NeighborEntry>) -> Self {
        Self {
            snapshot,
            neighbors,
            reads: Arc::new(Mutex::new(Vec::new())),
            probes: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            probe_signal: None,
            revision: Arc::new(Mutex::new(None)),
        }
    }
}

impl NetworkProvider for FakeNetwork {
    fn resolver_config_revision(
        &self,
    ) -> impl std::future::Future<Output = Option<crate::ResolverConfigRevision>> + Send + '_ {
        std::future::ready(self.revision.lock().unwrap().clone())
    }
    async fn network_snapshot(
        &self,
        selected_local_ip: Option<Ipv4Addr>,
    ) -> Result<NetworkSnapshot, ScanError> {
        if let Some(selected) = selected_local_ip {
            assert_eq!(self.snapshot.local_ip, selected);
        }
        self.events.lock().unwrap().push("snapshot".to_owned());
        Ok(self.snapshot.clone())
    }

    async fn read_neighbors<'a>(
        &'a self,
        ips: &'a [Ipv4Addr],
    ) -> Result<Vec<NeighborEntry>, ScanError> {
        self.reads.lock().unwrap().push(ips.to_vec());
        self.events.lock().unwrap().push("neighbors".to_owned());
        Ok(self
            .neighbors
            .iter()
            .filter(|entry| ips.contains(&entry.ip))
            .cloned()
            .collect())
    }

    async fn probe_neighbors<'a>(&'a self, ips: &'a [Ipv4Addr]) -> Result<(), ScanError> {
        self.probes.lock().unwrap().push(ips.to_vec());
        self.events.lock().unwrap().push("probe".to_owned());
        if let Some(signal) = &self.probe_signal {
            signal.notify_one();
        }
        Ok(())
    }
}

struct FakeResolver {
    answers: HashMap<Ipv4Addr, String>,
    calls: Arc<Mutex<Vec<(IpAddr, Ipv4Addr)>>>,
    events: Arc<Mutex<Vec<String>>>,
    only_nameserver: Option<IpAddr>,
    started: Option<Arc<Notify>>,
    block: bool,
}

impl FakeResolver {
    fn new(answers: impl IntoIterator<Item = (Ipv4Addr, String)>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            calls: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            only_nameserver: None,
            started: None,
            block: false,
        }
    }

    fn blocking(started: Arc<Notify>) -> Self {
        Self {
            answers: HashMap::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            only_nameserver: None,
            started: Some(started),
            block: true,
        }
    }
}

impl PtrResolver for FakeResolver {
    async fn lookup_ptr(
        &self,
        nameserver: IpAddr,
        ip: Ipv4Addr,
        _timeout: Duration,
    ) -> Option<String> {
        self.calls.lock().unwrap().push((nameserver, ip));
        self.events.lock().unwrap().push("ptr".to_owned());
        if self.block {
            if let Some(started) = &self.started {
                started.notify_one();
            }
            pending::<()>().await;
        }
        if self
            .only_nameserver
            .is_some_and(|expected| expected != nameserver)
        {
            None
        } else {
            self.answers.get(&ip).cloned()
        }
    }
}

fn snapshot(
    local_ip: Ipv4Addr,
    prefix: Option<u8>,
    router: Option<Ipv4Addr>,
    nameservers: Vec<IpAddr>,
) -> NetworkSnapshot {
    NetworkSnapshot {
        local_ip,
        interface_prefix: prefix,
        default_router: router,
        nameservers,
    }
}

fn config() -> ScanConfig {
    ScanConfig {
        ptr_timeout: Duration::from_millis(100),
        neighbor_cache_wait: Duration::ZERO,
        ..ScanConfig::default()
    }
}

fn cancellation_pair() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

#[tokio::test]
async fn failed_resolver_cache_skips_only_after_success_and_resets_on_revision_or_expiry() {
    let local = Ipv4Addr::new(10, 0, 0, 1);
    let host = Ipv4Addr::new(10, 0, 0, 2);
    let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53));
    let second = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 54));
    let network = Arc::new(FakeNetwork::new(
        snapshot(local, Some(30), None, vec![first, second]),
        vec![NeighborEntry {
            ip: host,
            mac: "02:00:00:00:00:02".into(),
        }],
    ));
    *network.revision.lock().unwrap() = Some(crate::ResolverConfigRevision {
        modified_ns: 1,
        size: 100,
    });
    let mut resolver = FakeResolver::new([(host, "unit.local".into())]);
    resolver.only_nameserver = Some(second);
    let resolver = Arc::new(resolver);
    let scanner = Scanner::new(network.clone(), resolver.clone(), config());
    let (_cancel, mut cancellation) = cancellation_pair();
    for _ in 0..2 {
        assert_eq!(
            scanner
                .scan(&mut cancellation)
                .await
                .unwrap()
                .observations
                .len(),
            1
        );
    }
    assert_eq!(
        resolver
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(server, ip)| *server == first && *ip == host)
            .count(),
        1
    );
    let expiry_before_config_change = scanner.failed_nameservers.lock().unwrap().expires_at;
    *network.revision.lock().unwrap() = Some(crate::ResolverConfigRevision {
        modified_ns: 2,
        size: 100,
    });
    scanner.scan(&mut cancellation).await.unwrap();
    assert_eq!(
        scanner.failed_nameservers.lock().unwrap().expires_at,
        expiry_before_config_change
    );
    assert_eq!(
        resolver
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(server, ip)| *server == first && *ip == host)
            .count(),
        2
    );
    scanner.failed_nameservers.lock().unwrap().expires_at = Some(tokio::time::Instant::now());
    scanner.scan(&mut cancellation).await.unwrap();
    assert_eq!(
        resolver
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(server, ip)| *server == first && *ip == host)
            .count(),
        3
    );
}

#[tokio::test]
async fn an_entirely_unresponsive_resolver_set_is_retried_on_the_next_scan() {
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(10, 0, 0, 1),
            Some(30),
            None,
            vec![
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53)),
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 54)),
            ],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::new([]));
    let scanner = Scanner::new(network, resolver.clone(), config());
    let (_cancel, mut cancellation) = cancellation_pair();
    for _ in 0..2 {
        assert!(
            scanner
                .scan(&mut cancellation)
                .await
                .unwrap()
                .observations
                .is_empty()
        );
    }
    // Two /30 hosts (including the local address), two resolvers, two scans.
    assert_eq!(resolver.calls.lock().unwrap().len(), 8);
}

#[tokio::test]
async fn scan_observations_follow_ascending_host_order() {
    let first = Ipv4Addr::new(10, 0, 0, 2);
    let second = Ipv4Addr::new(10, 0, 0, 4);
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(10, 0, 0, 1),
            Some(29),
            None,
            vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53))],
        ),
        vec![
            NeighborEntry {
                ip: second,
                mac: "02:00:00:00:00:04".into(),
            },
            NeighborEntry {
                ip: first,
                mac: "02:00:00:00:00:02".into(),
            },
        ],
    ));
    let resolver = Arc::new(FakeResolver::new([
        (second, "second.local".into()),
        (first, "first.local".into()),
    ]));
    let scanner = Scanner::new(network, resolver, config());
    let (_cancel, mut cancellation) = cancellation_pair();
    let output = scanner.scan(&mut cancellation).await.unwrap();
    assert_eq!(
        output
            .observations
            .iter()
            .map(|row| row.ip)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
}

#[tokio::test]
async fn scan_uses_fallback_prefix_and_emits_only_complete_hosts() {
    let printer = Ipv4Addr::new(192, 168, 50, 7);
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(192, 168, 50, 10),
            None,
            None,
            vec![IpAddr::V4(Ipv4Addr::new(192, 168, 50, 1))],
        ),
        vec![NeighborEntry {
            ip: printer,
            mac: "a:b:c:d:e:f".to_owned(),
        }],
    ));
    let resolver = Arc::new(FakeResolver::new([(
        printer,
        "printer.office.example".to_owned(),
    )]));
    let mut scan_config = config();
    scan_config.local_ipv4 = Some(Ipv4Addr::new(192, 168, 50, 10));
    let scanner = Scanner::new(network.clone(), resolver.clone(), scan_config);
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(output.status, ScanStatus::Completed);
    assert_eq!(
        output.observations,
        vec![HostObservation {
            ip: printer,
            hostname: "printer".to_owned(),
            mac: "0a:0b:0c:0d:0e:0f".to_owned(),
        }]
    );
    assert_eq!(resolver.calls.lock().unwrap().len(), 254);
    assert!(network.probes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn neighbors_are_read_and_probed_only_for_ptr_named_addresses() {
    let named = Ipv4Addr::new(192, 168, 4, 9);
    let unnamed = Ipv4Addr::new(192, 168, 4, 10);
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            unnamed,
            Some(30),
            None,
            vec![IpAddr::V4(Ipv4Addr::new(192, 168, 4, 9))],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::new([(named, "doorbell.home".to_owned())]));
    let scanner = Scanner::new(network.clone(), resolver, config());
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(output.status, ScanStatus::Completed);
    assert!(output.observations.is_empty());
    assert_eq!(
        *network.reads.lock().unwrap(),
        vec![vec![named], vec![named]]
    );
    assert_eq!(*network.probes.lock().unwrap(), vec![vec![named]]);
    assert!(!network.probes.lock().unwrap()[0].contains(&unnamed));
}

#[tokio::test]
async fn router_neighbor_is_checked_before_ptr_and_can_supply_nameserver() {
    let router = Ipv4Addr::new(192, 168, 1, 9);
    let named = Ipv4Addr::new(192, 168, 1, 10);
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            named,
            Some(30),
            Some(router),
            vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53))],
        ),
        vec![
            NeighborEntry {
                ip: router,
                mac: "02:00:00:00:00:01".to_owned(),
            },
            NeighborEntry {
                ip: named,
                mac: "02:00:00:00:00:02".to_owned(),
            },
        ],
    ));
    let mut fake_resolver = FakeResolver::new([(named, "sensor.home".to_owned())]);
    fake_resolver.only_nameserver = Some(IpAddr::V4(router));
    fake_resolver.events = network.events.clone();
    let resolver = Arc::new(fake_resolver);
    let events = network.events.clone();
    let scanner = Scanner::new(network, resolver.clone(), config());
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(output.status, ScanStatus::Completed);
    assert_eq!(
        output.observations,
        vec![HostObservation {
            ip: named,
            hostname: "sensor".to_owned(),
            mac: "02:00:00:00:00:02".to_owned(),
        }]
    );
    assert!(
        resolver
            .calls
            .lock()
            .unwrap()
            .contains(&(IpAddr::V4(router), named))
    );
    let events = events.lock().unwrap();
    let router_read = events
        .iter()
        .position(|event| event == "neighbors")
        .unwrap();
    let first_ptr = events.iter().position(|event| event == "ptr").unwrap();
    assert!(router_read < first_ptr);
}

#[tokio::test]
async fn oversized_subnets_are_skipped_before_dns_or_neighbor_work() {
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(10, 2, 3, 4),
            Some(20),
            None,
            vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            ],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::new([]));
    let scanner = Scanner::new(network.clone(), resolver.clone(), config());
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(
        output.status,
        ScanStatus::SkippedOversize {
            address_count: MAX_ADDRESSES * 2,
        }
    );
    assert!(resolver.calls.lock().unwrap().is_empty());
    assert!(network.reads.lock().unwrap().is_empty());
    assert!(network.probes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn public_nameservers_are_ignored() {
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(192, 168, 9, 10),
            Some(30),
            None,
            vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            ],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::new([]));
    let scanner = Scanner::new(network.clone(), resolver.clone(), config());
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(output.status, ScanStatus::Completed);
    assert!(resolver.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_2048_address_boundary_is_scanned() {
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(10, 10, 0, 1),
            Some(21),
            None,
            vec![IpAddr::V4(Ipv4Addr::new(10, 10, 0, 53))],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::new([]));
    let scanner = Scanner::new(network, resolver.clone(), config());
    let (_cancel_tx, mut cancel_rx) = cancellation_pair();

    let output = scanner.scan(&mut cancel_rx).await.unwrap();
    assert_eq!(output.status, ScanStatus::Completed);
    assert_eq!(resolver.calls.lock().unwrap().len(), 2046);
}

#[tokio::test]
async fn overlap_is_skipped_and_cancellation_releases_the_scan_gate() {
    let started = Arc::new(Notify::new());
    let network = Arc::new(FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(192, 168, 2, 10),
            Some(30),
            None,
            vec![IpAddr::V4(Ipv4Addr::new(192, 168, 2, 9))],
        ),
        Vec::new(),
    ));
    let resolver = Arc::new(FakeResolver::blocking(started.clone()));
    let scanner = Arc::new(Scanner::new(network, resolver, config()));
    let (cancel_tx, mut cancel_rx) = cancellation_pair();
    let running_scanner = Arc::clone(&scanner);
    let task = tokio::spawn(async move { running_scanner.scan(&mut cancel_rx).await });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("scan never reached PTR lookup");

    let (_other_tx, mut other_rx) = cancellation_pair();
    let overlap = scanner.scan(&mut other_rx).await.unwrap();
    assert_eq!(overlap.status, ScanStatus::SkippedOverlap);

    cancel_tx.send(true).unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled scan did not join")
        .unwrap()
        .unwrap();
    assert_eq!(stopped.status, ScanStatus::Cancelled);

    let (closed_tx, mut closed_rx) = cancellation_pair();
    closed_tx.send(true).unwrap();
    let after_stop = scanner.scan(&mut closed_rx).await.unwrap();
    assert_eq!(after_stop.status, ScanStatus::Cancelled);
}

#[tokio::test]
async fn cancellation_during_neighbor_cache_wait_stops_before_reread() {
    let named = Ipv4Addr::new(192, 168, 3, 9);
    let probe_signal = Arc::new(Notify::new());
    let mut fake_network = FakeNetwork::new(
        snapshot(
            Ipv4Addr::new(192, 168, 3, 10),
            Some(30),
            None,
            vec![IpAddr::V4(named)],
        ),
        Vec::new(),
    );
    fake_network.probe_signal = Some(probe_signal.clone());
    let network = Arc::new(fake_network);
    let resolver = Arc::new(FakeResolver::new([(named, "sensor.home".to_owned())]));
    let scanner = Arc::new(Scanner::new(
        network.clone(),
        resolver,
        ScanConfig {
            ptr_timeout: Duration::from_millis(100),
            neighbor_cache_wait: Duration::from_secs(30),
            ..ScanConfig::default()
        },
    ));
    let (cancel_tx, mut cancel_rx) = cancellation_pair();
    let running_scanner = Arc::clone(&scanner);
    let task = tokio::spawn(async move { running_scanner.scan(&mut cancel_rx).await });
    tokio::time::timeout(Duration::from_secs(1), probe_signal.notified())
        .await
        .expect("scan never reached the neighbor cache wait");

    cancel_tx.send(true).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled neighbor wait did not join")
        .unwrap()
        .unwrap();
    assert_eq!(output.status, ScanStatus::Cancelled);
    assert_eq!(*network.reads.lock().unwrap(), vec![vec![named]]);
}

#[tokio::test]
async fn loopback_dns_uses_rd_zero_and_decodes_ptr_response() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let mut request = [0u8; 512];
        let (length, peer) = server.recv_from(&mut request).await.unwrap();
        assert_eq!(u16::from_be_bytes([request[2], request[3]]) & 0x0100, 0);
        assert_eq!(
            &request[12..length - 4],
            wire_name("31.2.0.192.in-addr.arpa").as_slice()
        );
        let response = ptr_response(&request[..length], "printer.office.example");
        server.send_to(&response, peer).await.unwrap();
    });

    let answer = UdpPtrResolver::query_ptr_at(
        address,
        Ipv4Addr::new(192, 0, 2, 31),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(answer.as_deref(), Some("printer"));
    server_task.await.unwrap();
}

#[tokio::test]
async fn loopback_dns_rejects_malformed_answers() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let mut request = [0u8; 512];
        let (_, peer) = server.recv_from(&mut request).await.unwrap();
        server.send_to(&[0, 1, 0], peer).await.unwrap();
    });
    let answer = UdpPtrResolver::query_ptr_at(
        address,
        Ipv4Addr::new(192, 0, 2, 32),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(answer, None);
    server_task.await.unwrap();
}

#[tokio::test]
async fn loopback_dns_timeout_and_cancellation_close_lookup_future() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let mut request = [0u8; 512];
        let (length, _peer) = server.recv_from(&mut request).await.unwrap();
        length
    });
    let answer = UdpPtrResolver::query_ptr_at(
        address,
        Ipv4Addr::new(192, 0, 2, 33),
        Duration::from_millis(25),
    )
    .await;
    assert_eq!(answer, None);
    assert!(server_task.await.unwrap() > 12);

    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let mut request = [0u8; 512];
        let (length, _peer) = server.recv_from(&mut request).await.unwrap();
        length
    });
    let (cancel_tx, mut cancel_rx) = cancellation_pair();
    let task = tokio::spawn(async move {
        tokio::select! {
            answer = UdpPtrResolver::query_ptr_at(
                address,
                Ipv4Addr::new(192, 0, 2, 34),
                Duration::from_secs(30),
            ) => Some(answer),
            changed = cancel_rx.changed() => {
                assert!(changed.is_ok());
                None
            }
        }
    });
    assert!(server_task.await.unwrap() > 12);
    cancel_tx.send(true).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap(),
        None
    );
}

#[test]
fn route_resolver_and_neighbor_parsers_are_bounded_and_synthetic() {
    assert_eq!(test_mask_prefix(Ipv4Addr::new(255, 255, 255, 0)), Some(24));
    assert_eq!(test_mask_prefix(Ipv4Addr::new(255, 0, 255, 0)), None);
    let routes = "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\neth0 00000000 0102A8C0 0003 0 0 100 00000000 0 0 0\neth0 0002A8C0 00000000 0001 0 0 100 00FFFFFF 0 0 0\n";
    assert_eq!(
        test_parse_proc_routes(routes, Ipv4Addr::new(192, 168, 2, 50)),
        (Some(Ipv4Addr::new(192, 168, 2, 1)), Some(24))
    );
    let arp = "IP address HW type Flags HW address Mask Device\n192.168.2.1 0x1 0x2 aa:bb:cc:dd:ee:ff * eth0\n192.168.2.9 0x1 0x0 00:00:00:00:00:00 * eth0\n";
    assert_eq!(
        test_parse_proc_neighbors(arp),
        vec![NeighborEntry {
            ip: Ipv4Addr::new(192, 168, 2, 1),
            mac: "aa:bb:cc:dd:ee:ff".to_owned(),
        }]
    );
    assert_eq!(
        test_parse_resolv_conf(
            "# comment\nnameserver 192.168.1.1\nnameserver 8.8.8.8\nnameserver 192.168.1.1\nnameserver not-an-ip\n"
        ),
        vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        ]
    );
}

#[test]
fn cidr_address_count_and_host_edges_match_ipv4_network_rules() {
    let subnet = Ipv4Subnet::new(Ipv4Addr::new(192, 168, 7, 19), 24).unwrap();
    assert_eq!(subnet.address_count, 256);
    assert_eq!(
        subnet.hosts(),
        (1..=254)
            .map(|last| Ipv4Addr::new(192, 168, 7, last))
            .collect::<Vec<_>>()
    );

    let point_to_point = Ipv4Subnet::new(Ipv4Addr::new(192, 0, 2, 10), 31).unwrap();
    assert_eq!(point_to_point.address_count, 2);
    assert_eq!(
        point_to_point.hosts(),
        vec![Ipv4Addr::new(192, 0, 2, 10), Ipv4Addr::new(192, 0, 2, 11)]
    );
    let single = Ipv4Subnet::new(Ipv4Addr::new(192, 0, 2, 12), 32).unwrap();
    assert_eq!(single.hosts(), vec![Ipv4Addr::new(192, 0, 2, 12)]);
    assert_eq!(
        Ipv4Subnet::new(Ipv4Addr::new(10, 0, 0, 1), 21)
            .unwrap()
            .address_count,
        MAX_ADDRESSES
    );
    assert_eq!(
        Ipv4Subnet::new(Ipv4Addr::new(10, 0, 0, 1), 20)
            .unwrap()
            .address_count,
        MAX_ADDRESSES * 2
    );
}

#[test]
fn malformed_compression_pointer_is_rejected() {
    let query = test_build_ptr_query(0x1234, "31.2.0.192.in-addr.arpa").unwrap();
    let mut response = ptr_response(&query, "host.local");
    let mut question_end = 12;
    loop {
        let length = response[question_end] as usize;
        question_end += 1;
        if length == 0 {
            break;
        }
        question_end += length;
    }
    question_end += 4;
    let answer_data = question_end + 2 + 10;
    response[answer_data] = 0xc0;
    response[answer_data + 1] = answer_data as u8;
    assert_eq!(
        parse_ptr_response(&response, 0x1234, "31.2.0.192.in-addr.arpa"),
        None
    );
}

#[test]
fn ptr_hostname_and_mac_validation_match_consumer_expectations() {
    assert_eq!(
        crate::short_hostname("device.office.example"),
        Some("device".to_owned())
    );
    assert_eq!(
        crate::short_hostname("xn--caf-dma.local"),
        Some("café".to_owned())
    );
    assert_eq!(crate::short_hostname("_printer.local"), None);
    assert_eq!(crate::short_hostname("-invalid.local"), None);
    assert_eq!(
        crate::normalize_mac("a-b-c-d-e-f"),
        Some("0a-0b-0c-0d-0e-0f".to_owned())
    );
    assert_eq!(crate::normalize_mac("00:00:00:00:00:00"), None);
    assert_eq!(crate::normalize_mac("ff:ff:ff:ff:ff:ff"), None);
    assert_eq!(crate::normalize_mac("01:02:03:04:05:gg"), None);
}

fn wire_name(name: &str) -> Vec<u8> {
    let mut encoded = Vec::new();
    for label in name.split('.') {
        encoded.push(label.len() as u8);
        encoded.extend_from_slice(label.as_bytes());
    }
    encoded.push(0);
    encoded
}

fn ptr_response(request: &[u8], target: &str) -> Vec<u8> {
    let mut question_end = 12;
    loop {
        let length = request[question_end] as usize;
        question_end += 1;
        if length == 0 {
            break;
        }
        question_end += length;
    }
    question_end += 4;
    let mut response = Vec::with_capacity(request.len() + target.len() + 20);
    response.extend_from_slice(&request[0..2]);
    response.extend_from_slice(&0x8000u16.to_be_bytes()); // QR=1, RD=0, RA=0.
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&request[12..question_end]);
    response.extend_from_slice(&[0xc0, 0x0c]); // Owner points at the question.
    response.extend_from_slice(&12u16.to_be_bytes()); // PTR
    response.extend_from_slice(&1u16.to_be_bytes()); // IN
    response.extend_from_slice(&60u32.to_be_bytes());
    let mut rdata = Vec::new();
    for label in target.split('.') {
        rdata.push(label.len() as u8);
        rdata.extend_from_slice(label.as_bytes());
    }
    rdata.push(0);
    response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    response.extend_from_slice(&rdata);
    response
}
