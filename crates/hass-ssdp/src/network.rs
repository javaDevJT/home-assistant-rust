//! Native UDP transport. All interfaces feed one [`SsdpManager`].

use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    time::Duration,
};

use chrono::{DateTime, Utc};
use hass_core::{Error, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::{
    net::UdpSocket,
    runtime::Handle,
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{self, MissedTickBehavior},
};

use crate::{SsdpManager, build_search_packet, decode_ssdp_packet};

const MAX_PACKET_SIZE: usize = u16::MAX as usize;

macro_rules! io_result {
    ($result:expr, $action:literal) => {
        $result.map_err(|error| service_error($action, error))?
    };
}

/// A local unicast address and (for scoped IPv6) the owning interface index.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SsdpInterface {
    pub address: IpAddr,
    pub index: Option<u32>,
}

impl SsdpInterface {
    /// Convert this source to a socket address, retaining the IPv6 zone index.
    pub fn socket_addr(self, port: u16) -> Result<SocketAddr> {
        match self.address {
            IpAddr::V4(address) => Ok(SocketAddr::V4(SocketAddrV4::new(address, port))),
            IpAddr::V6(address) => {
                let index = self.index.filter(|index| *index != 0).ok_or_else(|| {
                    Error::Service(format!(
                        "SSDP IPv6 source {address} requires a nonzero interface index"
                    ))
                })?;
                Ok(SocketAddr::V6(SocketAddrV6::new(address, port, 0, index)))
            }
        }
    }
}

/// Select enabled, non-loopback, non-global IPv4 and scoped link-local IPv6 sources.
pub fn enabled_sources() -> Result<Vec<SsdpInterface>> {
    let interfaces = io_result!(if_addrs::get_if_addrs(), "enumerate network interfaces");
    let mut sources = Vec::new();
    for interface in interfaces {
        if !interface.is_oper_up() || interface.is_loopback() {
            continue;
        }
        let source = SsdpInterface {
            address: interface.ip(),
            index: interface.index,
        };
        if source_is_eligible(source) && !sources.contains(&source) {
            sources.push(source);
        }
    }
    Ok(sources)
}

/// Search destinations, bind ports, timing, and bounded receive-work settings.
#[derive(Clone, Debug)]
pub struct SsdpScannerOptions {
    pub search_targets: Vec<SocketAddr>,
    pub search_port: u16,
    pub advertisement_port: u16,
    pub search_interval: Duration,
    pub mx: u32,
    pub search_target: String,
    pub max_concurrent_observations: usize,
    pub receive_queue_capacity: usize,
}

impl Default for SsdpScannerOptions {
    fn default() -> Self {
        Self {
            search_targets: vec![
                SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(239, 255, 255, 250),
                    crate::SSDP_PORT,
                )),
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, crate::SSDP_PORT)),
                SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x000c),
                    crate::SSDP_PORT,
                    0,
                    0,
                )),
            ],
            search_port: 0,
            advertisement_port: crate::SSDP_PORT,
            search_interval: Duration::from_secs(10 * 60),
            mx: 4,
            search_target: "ssdp:all".into(),
            max_concurrent_observations: 8,
            receive_queue_capacity: 64,
        }
    }
}

/// A running scanner. Dropping it aborts its tasks; prefer [`shutdown`](Self::shutdown)
/// to signal cancellation and join them cleanly.
pub struct SsdpScanner {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl SsdpScanner {
    /// Bind all usable listeners, start receivers, and send the first search batch.
    pub async fn start(
        manager: SsdpManager,
        sources: Vec<SsdpInterface>,
        options: SsdpScannerOptions,
    ) -> Result<Self> {
        validate_options(&options)?;
        let runtime = Handle::try_current().map_err(|error| {
            Error::Service(format!("start SSDP scanner without Tokio: {error}"))
        })?;
        let mut unique_sources = Vec::new();
        let mut seen = HashSet::new();
        for source in sources {
            if seen.insert(source) {
                unique_sources.push(source);
            }
        }
        if unique_sources.is_empty() {
            tracing::debug!("SSDP scanner started without enabled network sources");
            return Ok(Self::idle());
        }

        // Stage every socket before spawning anything. A failed interface is
        // logged and discarded as a unit, so it cannot leave a receiver behind.
        let mut ready = Vec::new();
        let mut failures = Vec::new();
        for source in unique_sources {
            match prepare_interface(source, &options) {
                Ok(interface) => ready.push(interface),
                Err(error) => {
                    tracing::warn!(?source, %error, "could not start SSDP sockets for interface");
                    failures.push(format!("{source:?}: {error}"));
                }
            }
        }
        if ready.is_empty() {
            tracing::debug!(failures = ?failures, "SSDP scanner started without usable listeners");
            return Ok(Self::idle());
        }
        if !failures.is_empty() {
            tracing::warn!(failures = ?failures, "some SSDP interfaces were skipped");
        }

        let (datagram_tx, datagram_rx) = mpsc::channel(options.receive_queue_capacity);
        let (stop, stop_rx) = watch::channel(false);
        let mut tasks = Vec::with_capacity(ready.len() + 1);
        tasks.push(runtime.spawn(observe_loop(
            manager,
            datagram_rx,
            stop_rx.clone(),
            options.max_concurrent_observations,
        )));

        for interface in ready {
            tasks.push(runtime.spawn(search_loop(
                interface,
                options.clone(),
                datagram_tx.clone(),
                stop_rx.clone(),
            )));
        }
        drop(datagram_tx);

        Ok(Self { stop, tasks })
    }

    /// Stop receivers and in-flight observations, then join every owned task.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        for mut task in self.tasks.drain(..) {
            if time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }

    fn idle() -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            stop,
            tasks: Vec::new(),
        }
    }
}

impl Drop for SsdpScanner {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct ReadyInterface {
    source: SsdpInterface,
    search: UdpSocket,
    search_local: SocketAddr,
    advertisement: UdpSocket,
    advertisement_local: SocketAddr,
}

struct Datagram {
    bytes: Vec<u8>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    received_at: DateTime<Utc>,
}

fn validate_options(options: &SsdpScannerOptions) -> Result<()> {
    if options.search_interval.is_zero() {
        return Err(Error::Service(
            "SSDP search interval must be nonzero".into(),
        ));
    }
    if options.max_concurrent_observations == 0 || options.receive_queue_capacity == 0 {
        return Err(Error::Service(
            "SSDP observation concurrency and queue capacity must be nonzero".into(),
        ));
    }
    if options.search_target.trim().is_empty() {
        return Err(Error::Service(
            "SSDP search target must not be empty".into(),
        ));
    }
    Ok(())
}

fn prepare_interface(
    source: SsdpInterface,
    options: &SsdpScannerOptions,
) -> Result<ReadyInterface> {
    let source_addr = source.socket_addr(options.search_port)?;
    let search = configured_socket(source, &options.search_targets)?;
    io_result!(
        search.bind(&SockAddr::from(source_addr)),
        "bind SSDP search socket"
    );
    let search = into_tokio_socket(search)?;
    let search_local = io_result!(search.local_addr(), "read SSDP search socket address");

    let advertisement = configured_socket(source, &options.search_targets)?;
    let bind_addr = match source.address {
        IpAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            options.advertisement_port,
        )),
        IpAddr::V6(_) => SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::UNSPECIFIED,
            options.advertisement_port,
            0,
            0,
        )),
    };
    io_result!(
        advertisement.bind(&SockAddr::from(bind_addr)),
        "bind SSDP advertisement listener"
    );
    let advertisement = into_tokio_socket(advertisement)?;
    let local_port = io_result!(
        advertisement.local_addr(),
        "read SSDP advertisement address"
    )
    .port();
    let advertisement_local = source.socket_addr(local_port)?;
    Ok(ReadyInterface {
        source,
        search,
        search_local,
        advertisement,
        advertisement_local,
    })
}

fn configured_socket(source: SsdpInterface, targets: &[SocketAddr]) -> Result<Socket> {
    let domain = match source.address {
        IpAddr::V4(_) => Domain::IPV4,
        IpAddr::V6(_) => Domain::IPV6,
    };
    let socket = io_result!(
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)),
        "create SSDP UDP socket"
    );
    io_result!(socket.set_reuse_address(true), "set SSDP SO_REUSEADDR");
    #[cfg(unix)]
    io_result!(socket.set_reuse_port(true), "set SSDP SO_REUSEPORT");

    if matches!(source.address, IpAddr::V4(_)) {
        io_result!(socket.set_broadcast(true), "enable SSDP IPv4 broadcast");
    } else {
        io_result!(socket.set_only_v6(true), "restrict SSDP socket to IPv6");
    }

    for target in targets_for(source, targets) {
        match (source.address, target) {
            (IpAddr::V4(source_ip), SocketAddr::V4(target)) if target.ip().is_multicast() => {
                io_result!(
                    socket.set_multicast_if_v4(&source_ip),
                    "select V4 multicast interface"
                );
                io_result!(socket.set_multicast_ttl_v4(2), "set V4 multicast TTL");
                io_result!(
                    socket.join_multicast_v4(target.ip(), &source_ip),
                    "join V4 multicast group"
                );
            }
            (IpAddr::V6(_), SocketAddr::V6(target)) if target.ip().is_multicast() => {
                let index = source.index.filter(|index| *index != 0).ok_or_else(|| {
                    Error::Service("SSDP IPv6 multicast requires an interface index".into())
                })?;
                io_result!(
                    socket.set_multicast_if_v6(index),
                    "select V6 multicast interface"
                );
                io_result!(socket.set_multicast_hops_v6(2), "set V6 multicast hops");
                io_result!(
                    socket.join_multicast_v6(target.ip(), index),
                    "join V6 multicast group"
                );
            }
            _ => {}
        }
    }
    io_result!(socket.set_nonblocking(true), "set SSDP socket nonblocking");
    Ok(socket)
}

fn into_tokio_socket(socket: Socket) -> Result<UdpSocket> {
    let socket: std::net::UdpSocket = socket.into();
    Ok(io_result!(
        UdpSocket::from_std(socket),
        "adopt SSDP UDP socket"
    ))
}

fn search_targets_for(source: SsdpInterface, targets: &[SocketAddr]) -> Vec<SocketAddr> {
    targets_for(source, targets).collect()
}

fn targets_for(
    source: SsdpInterface,
    targets: &[SocketAddr],
) -> impl Iterator<Item = SocketAddr> + '_ {
    targets
        .iter()
        .filter_map(move |target| match (source.address, target) {
            (IpAddr::V4(_), SocketAddr::V4(target)) => Some(SocketAddr::V4(*target)),
            (IpAddr::V6(_), SocketAddr::V6(target)) => {
                let scope = if target.scope_id() == 0 {
                    source.index.unwrap_or(0)
                } else {
                    target.scope_id()
                };
                Some(SocketAddr::V6(SocketAddrV6::new(
                    *target.ip(),
                    target.port(),
                    target.flowinfo(),
                    scope,
                )))
            }
            _ => None,
        })
}

async fn search_loop(
    interface: ReadyInterface,
    options: SsdpScannerOptions,
    tx: mpsc::Sender<Datagram>,
    mut stop: watch::Receiver<bool>,
) {
    let ReadyInterface {
        source,
        search: search_socket,
        search_local,
        advertisement: advertisement_socket,
        advertisement_local,
    } = interface;
    let targets = search_targets_for(source, &options.search_targets);
    let mx = options.mx;
    let search_target = options.search_target;
    let mut timer = time::interval(options.search_interval);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    timer.tick().await; // consume interval's immediate first tick
    send_search_batch(&search_socket, &targets, mx, &search_target).await;
    let mut search_buffer = vec![0; MAX_PACKET_SIZE];
    let mut advertisement_buffer = vec![0; MAX_PACKET_SIZE];
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
            }
            _ = timer.tick() => send_search_batch(&search_socket, &targets, mx, &search_target).await,
            (received, role) = async {
                tokio::select! {
                    packet = receive_packet(&search_socket, search_local, &mut search_buffer) => (packet, "search"),
                    packet = receive_packet(&advertisement_socket, advertisement_local, &mut advertisement_buffer) => (packet, "advertisement"),
                }
            } => match received {
                Ok(packet) => if !forward_packet(&tx, &mut stop, packet).await { break; },
                Err(error) => {
                    tracing::warn!(%error, role, "SSDP receiver stopped");
                    break;
                }
            }
        }
    }
}

async fn send_search_batch(socket: &UdpSocket, targets: &[SocketAddr], mx: u32, st: &str) {
    for target in targets {
        let packet = build_search_packet(*target, mx, st);
        if let Err(error) = socket.send_to(&packet, target).await {
            tracing::warn!(%error, %target, "failed to send SSDP M-SEARCH");
        }
    }
}

async fn receive_packet(
    socket: &UdpSocket,
    local_addr: SocketAddr,
    buffer: &mut [u8],
) -> io::Result<Datagram> {
    let (length, remote_addr) = socket.recv_from(buffer).await?;
    Ok(Datagram {
        bytes: buffer[..length].to_vec(),
        local_addr,
        remote_addr,
        received_at: Utc::now(),
    })
}

async fn forward_packet(
    tx: &mpsc::Sender<Datagram>,
    stop: &mut watch::Receiver<bool>,
    packet: Datagram,
) -> bool {
    tokio::select! {
        biased;
        changed = stop.changed() => changed.is_ok() && !*stop.borrow(),
        sent = tx.send(packet) => sent.is_ok(),
    }
}

async fn observe_loop(
    manager: SsdpManager,
    mut rx: mpsc::Receiver<Datagram>,
    mut stop: watch::Receiver<bool>,
    concurrency: usize,
) {
    let mut observations = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
            }
            result = observations.join_next(), if !observations.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::debug!(%error, "SSDP observation task ended");
                }
            }
            packet = rx.recv(), if observations.len() < concurrency => match packet {
                Some(packet) => match decode_ssdp_packet(&packet.bytes, Some(packet.local_addr), packet.remote_addr, packet.received_at) {
                    Ok(message) => {
                        let manager = manager.clone();
                        observations.spawn(async move {
                            let now = message.received_at;
                            if let Err(error) = manager.observe(&message, now).await {
                                tracing::debug!(%error, "SSDP message observation failed");
                            }
                        });
                    }
                    Err(error) => tracing::debug!(%error, remote = %packet.remote_addr, "ignored invalid SSDP datagram"),
                },
                None => break,
            }
        }
    }
    observations.abort_all();
    while let Some(result) = observations.join_next().await {
        if let Err(error) = result {
            tracing::debug!(%error, "joined cancelled SSDP observation");
        }
    }
}

fn source_is_eligible(source: SsdpInterface) -> bool {
    match source.address {
        IpAddr::V4(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !address.is_broadcast()
                && ipv4_is_non_global(address)
        }
        IpAddr::V6(address) => {
            address.is_unicast_link_local() && source.index.is_some_and(|index| index != 0)
        }
    }
}

// Match the non-global IPv4 classes used for local source selection. Rust's
// `Ipv4Addr::is_global` is still unstable; these are the IANA special-purpose,
// private, shared, and documentation ranges that are not globally routable.
fn ipv4_is_non_global(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    if octets == [192, 0, 0, 9] || octets == [192, 0, 0, 10] {
        return false;
    }
    address.is_private()
        || address.is_link_local()
        || matches!(
            octets,
            [0, _, _, _]
                | [100, 64..=127, _, _]
                | [192, 0, 0, _]
                | [192, 0, 2, _]
                | [192, 88, 99, _]
                | [198, 18..=19, _, _]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
                | [240..=255, _, _, _]
        )
}

fn service_error(action: &str, error: impl std::fmt::Display) -> Error {
    Error::Service(format!("failed to {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IntegrationMatchersConfig, SsdpChange, SsdpIntegration};
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, UdpSocket as TokioUdpSocket},
        time::timeout,
    };

    const SEARCH_ST: &str = "urn:schemas-upnp-org:device:Basic:1";
    const UDN: &str = "uuid:loopback-ssdp-fixture";

    #[test]
    fn source_filter_keeps_only_non_global_ipv4_and_scoped_link_local_ipv6() {
        let ipv4 = |address| SsdpInterface {
            address: IpAddr::V4(address),
            index: None,
        };
        let ipv6 = |address, index| SsdpInterface {
            address: IpAddr::V6(address),
            index,
        };
        assert!(source_is_eligible(ipv4(Ipv4Addr::new(192, 168, 1, 12))));
        assert!(source_is_eligible(ipv4(Ipv4Addr::new(169, 254, 10, 2))));
        assert!(!source_is_eligible(ipv4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!source_is_eligible(ipv4(Ipv4Addr::LOCALHOST)));
        assert!(source_is_eligible(ipv6(
            "fe80::1234".parse().unwrap(),
            Some(7)
        )));
        assert!(!source_is_eligible(ipv6(
            "fe80::1234".parse().unwrap(),
            None
        )));
        assert!(!source_is_eligible(ipv6(
            "2001:4860::1".parse().unwrap(),
            Some(7)
        )));
    }

    #[tokio::test]
    async fn an_empty_source_set_starts_and_shuts_down_cleanly() {
        let integration = SsdpIntegration::with_client(
            reqwest::Client::new(),
            IntegrationMatchersConfig::new(),
            None,
        );
        let scanner = SsdpScanner::start(
            integration.manager(),
            Vec::new(),
            SsdpScannerOptions::default(),
        )
        .await
        .unwrap();
        scanner.shutdown().await;
        integration.shutdown().await;
    }

    #[test]
    fn ipv6_socket_address_requires_and_retains_scope() {
        let source = SsdpInterface {
            address: IpAddr::V6("fe80::1234".parse().unwrap()),
            index: Some(9),
        };
        let SocketAddr::V6(address) = source.socket_addr(1900).unwrap() else {
            panic!("expected IPv6")
        };
        assert_eq!(address.scope_id(), 9);
        assert!(
            SsdpInterface {
                index: None,
                ..source
            }
            .socket_addr(1900)
            .is_err()
        );
    }

    #[tokio::test]
    async fn loopback_search_response_is_cached_and_scanner_stops_and_restarts() {
        let udp_server = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let udp_target = udp_server.local_addr().unwrap();
        let http_server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let http_target = http_server.local_addr().unwrap();
        let http_hits = Arc::new(AtomicUsize::new(0));
        let hits = http_hits.clone();
        let description = tokio::spawn(async move {
            let (mut stream, _) = http_server.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            hits.fetch_add(1, Ordering::SeqCst);
            let body = "<root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device><deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType><friendlyName>Loopback fixture</friendlyName><manufacturer>Fixture</manufacturer><modelName>Fixture</modelName><UDN>uuid:loopback-ssdp-fixture</UDN></device></root>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .resolve("device.invalid", http_target)
            .no_proxy()
            .build()
            .unwrap();
        let integration =
            SsdpIntegration::with_client(client, IntegrationMatchersConfig::new(), None);
        let manager = integration.manager();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let callback_count = callbacks.clone();
        let matcher = BTreeMap::from([("st".to_owned(), SEARCH_ST.to_owned())]);
        let _callback = manager
            .register_callback(
                matcher,
                Arc::new(move |_, change| {
                    if change == SsdpChange::Alive {
                        callback_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                }),
            )
            .await
            .unwrap();

        let listener_reservation = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let advertisement_port = listener_reservation.local_addr().unwrap().port();
        drop(listener_reservation);
        let source = SsdpInterface {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            index: None,
        };
        let options = SsdpScannerOptions {
            search_targets: vec![udp_target],
            advertisement_port,
            ..SsdpScannerOptions::default()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://device.invalid:{}/desc.xml\r\nSERVER: loopback-fixture/1.0 UPnP/1.0\r\nST: {SEARCH_ST}\r\nUSN: {UDN}::{SEARCH_ST}\r\n\r\n",
            http_target.port()
        );

        let scanner = SsdpScanner::start(manager.clone(), vec![source], options.clone())
            .await
            .unwrap();
        let mut packet = vec![0; MAX_PACKET_SIZE];
        let (length, old_peer) = timeout(Duration::from_secs(2), udp_server.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
        let expected = build_search_packet(udp_target, options.mx, &options.search_target);
        assert_eq!(&packet[..length], expected.as_slice());
        udp_server
            .send_to(response.as_bytes(), old_peer)
            .await
            .unwrap();
        timeout(Duration::from_secs(3), async {
            while callbacks.load(Ordering::SeqCst) == 0 {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        description.await.unwrap();
        let info = manager
            .get_by_udn_and_st(UDN, SEARCH_ST)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.ssdp_st, SEARCH_ST);
        assert_eq!(
            info.ssdp_location.as_deref(),
            Some(format!("http://device.invalid:{}/desc.xml", http_target.port()).as_str())
        );
        assert_eq!(http_hits.load(Ordering::SeqCst), 1);

        scanner.shutdown().await;
        udp_server
            .send_to(response.as_bytes(), old_peer)
            .await
            .unwrap();
        time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            callbacks.load(Ordering::SeqCst),
            1,
            "no callback after shutdown"
        );
        let released =
            std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, advertisement_port)).unwrap();
        drop(released);

        let restarted = SsdpScanner::start(manager.clone(), vec![source], options)
            .await
            .unwrap();
        let (length, peer) = timeout(Duration::from_secs(2), udp_server.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&packet[..length], expected.as_slice());
        udp_server.send_to(response.as_bytes(), peer).await.unwrap();
        timeout(Duration::from_secs(3), async {
            while callbacks.load(Ordering::SeqCst) < 2 {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        restarted.shutdown().await;
        assert_eq!(
            http_hits.load(Ordering::SeqCst),
            1,
            "cached description is reused on restart"
        );
        integration.shutdown().await;
    }
}
