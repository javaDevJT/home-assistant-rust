//! Native IPv4 PTR and neighbor discovery for the DHCP integration.
//!
//! The scanner intentionally emits a host only when it has both a usable PTR
//! hostname and a validated neighbor MAC. Its system adapter reads Linux route
//! and ARP tables. Other platforms can use the same engine through the
//! [`NetworkProvider`] seam.

mod dns;
mod system;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;

pub use dns::UdpPtrResolver;
pub use system::SystemNetworkProvider;

/// Maximum total addresses in a subnet eligible for scanning.
pub const MAX_ADDRESSES: u64 = 2048;
/// Match aiodiscover's concurrent PTR batch size.
pub const PTR_BATCH_SIZE: usize = 64;
/// Default DNS response budget, matching aiodiscover 3.3.2.
pub const DEFAULT_PTR_TIMEOUT: Duration = Duration::from_secs(2);
/// Default ARP cache population wait, matching aiodiscover 3.3.2.
pub const DEFAULT_NEIGHBOR_CACHE_WAIT: Duration = Duration::from_secs(10);
const FAILED_NAMESERVER_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Per-scan behavior. The address cap remains fixed at [`MAX_ADDRESSES`].
#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// Optional IPv4 address selected by the host application.
    pub local_ipv4: Option<Ipv4Addr>,
    /// Prefix used when the selected address has no interface prefix.
    pub fallback_prefix: u8,
    /// Deadline for one PTR request.
    pub ptr_timeout: Duration,
    /// Time to allow the OS neighbor cache to populate after the UDP probe.
    pub neighbor_cache_wait: Duration,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            local_ipv4: None,
            fallback_prefix: 24,
            ptr_timeout: DEFAULT_PTR_TIMEOUT,
            neighbor_cache_wait: DEFAULT_NEIGHBOR_CACHE_WAIT,
        }
    }
}

/// Network information needed by one IPv4 scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkSnapshot {
    pub local_ip: Ipv4Addr,
    /// Prefix reported for the interface that owns `local_ip`, when known.
    pub interface_prefix: Option<u8>,
    /// Gateway from the selected default route, when known.
    pub default_router: Option<Ipv4Addr>,
    /// Resolver addresses from the host's local resolver configuration.
    pub nameservers: Vec<IpAddr>,
}

/// One raw neighbor-table entry supplied by the platform adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeighborEntry {
    pub ip: Ipv4Addr,
    pub mac: String,
}

/// A host accepted by both reverse DNS and the neighbor table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostObservation {
    pub ip: Ipv4Addr,
    pub hostname: String,
    pub mac: String,
}

/// Why a scan returned without observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanStatus {
    Completed,
    SkippedOversize { address_count: u64 },
    SkippedOverlap,
    Cancelled,
}

/// Result of one scan attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanOutput {
    pub status: ScanStatus,
    pub observations: Vec<HostObservation>,
}

impl ScanOutput {
    fn empty(status: ScanStatus) -> Self {
        Self {
            status,
            observations: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("network provider failed: {0}")]
    Provider(String),
    #[error("invalid subnet prefix /{0}")]
    InvalidPrefix(u8),
}

impl From<std::io::Error> for ScanError {
    fn from(value: std::io::Error) -> Self {
        Self::Provider(value.to_string())
    }
}

/// Identity of the resolver configuration used by a network provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolverConfigRevision {
    pub modified_ns: u128,
    pub size: u64,
}

/// Native OS boundary for local network selection and neighbor-cache access.
/// Operations must be bounded; cancellation drops their futures.
pub trait NetworkProvider: Send + Sync + 'static {
    /// Optional configuration identity. A file replacement/touch clears failed
    /// resolvers even when the list of resolver addresses stays unchanged.
    fn resolver_config_revision(
        &self,
    ) -> impl Future<Output = Option<ResolverConfigRevision>> + Send + '_ {
        std::future::ready(None)
    }

    fn network_snapshot(
        &self,
        selected_local_ip: Option<Ipv4Addr>,
    ) -> impl Future<Output = Result<NetworkSnapshot, ScanError>> + Send + '_;

    fn read_neighbors<'a>(
        &'a self,
        ips: &'a [Ipv4Addr],
    ) -> impl Future<Output = Result<Vec<NeighborEntry>, ScanError>> + Send + 'a;

    /// Send empty UDP datagrams to port 80 for cache population, as upstream
    /// does. This method is called only for addresses lacking a valid neighbor.
    fn probe_neighbors<'a>(
        &'a self,
        ips: &'a [Ipv4Addr],
    ) -> impl Future<Output = Result<(), ScanError>> + Send + 'a;
}

/// PTR lookup boundary. Dropping a lookup future must close its socket or
/// release its resolver request.
pub trait PtrResolver: Send + Sync + 'static {
    fn lookup_ptr(
        &self,
        nameserver: IpAddr,
        ip: Ipv4Addr,
        timeout: Duration,
    ) -> impl Future<Output = Option<String>> + Send + '_;
}

/// One scanner can be shared safely by scheduled callers. Concurrent calls
/// return [`ScanStatus::SkippedOverlap`]; the active call remains the sole
/// owner of its providers and sockets.
pub struct Scanner<N, R> {
    network: Arc<N>,
    resolver: Arc<R>,
    config: ScanConfig,
    running: AtomicBool,
    failed_nameservers: Mutex<FailedNameservers>,
}

struct FailedNameservers {
    configuration: Vec<IpAddr>,
    revision: Option<ResolverConfigRevision>,
    failed: HashSet<IpAddr>,
    expires_at: Option<Instant>,
}

impl Default for FailedNameservers {
    fn default() -> Self {
        Self {
            configuration: Vec::new(),
            revision: None,
            failed: HashSet::new(),
            expires_at: Some(Instant::now() + FAILED_NAMESERVER_CACHE_TTL),
        }
    }
}

impl<N, R> Scanner<N, R>
where
    N: NetworkProvider,
    R: PtrResolver,
{
    pub fn new(network: Arc<N>, resolver: Arc<R>, config: ScanConfig) -> Self {
        Self {
            network,
            resolver,
            config,
            running: AtomicBool::new(false),
            failed_nameservers: Mutex::new(FailedNameservers::default()),
        }
    }

    /// Run one scan using the receiver as its shutdown signal. The owner
    /// should retain the matching sender and await this future's task on stop.
    pub async fn scan(
        &self,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<ScanOutput, ScanError> {
        let Some(_guard) = ScanGuard::try_acquire(&self.running) else {
            return Ok(ScanOutput::empty(ScanStatus::SkippedOverlap));
        };
        if is_cancelled(cancellation) {
            return Ok(ScanOutput::empty(ScanStatus::Cancelled));
        }

        let revision =
            match await_or_cancel(self.network.resolver_config_revision(), cancellation).await {
                Step::Cancelled => return Ok(ScanOutput::empty(ScanStatus::Cancelled)),
                Step::Ready(revision) => revision,
            };
        let snapshot = match await_or_cancel(
            self.network.network_snapshot(self.config.local_ipv4),
            cancellation,
        )
        .await
        {
            Step::Cancelled => return Ok(ScanOutput::empty(ScanStatus::Cancelled)),
            Step::Ready(result) => result?,
        };
        let prefix = snapshot
            .interface_prefix
            .unwrap_or(self.config.fallback_prefix);
        let subnet = Ipv4Subnet::new(snapshot.local_ip, prefix)?;
        if subnet.address_count > MAX_ADDRESSES {
            return Ok(ScanOutput::empty(ScanStatus::SkippedOversize {
                address_count: subnet.address_count,
            }));
        }
        let addresses = subnet.hosts();
        let mut nameservers = accepted_nameservers(&snapshot.nameservers);

        let router = snapshot
            .default_router
            .or_else(|| addresses.first().copied());
        if let Some(router) = router {
            let router_is_nameserver = nameservers.contains(&IpAddr::V4(router));
            let has_in_network_nameserver = nameservers
                .iter()
                .any(|server| matches!(server, IpAddr::V4(ip) if subnet.contains(*ip)));
            if !router_is_nameserver && !has_in_network_nameserver {
                let router_neighbors = match self.neighbors_for(&[router], cancellation).await {
                    Step::Cancelled => return Ok(ScanOutput::empty(ScanStatus::Cancelled)),
                    Step::Ready(result) => result?,
                };
                if router_neighbors.contains_key(&router) {
                    nameservers.push(IpAddr::V4(router));
                }
            }
        }

        if nameservers.is_empty() || addresses.is_empty() {
            return Ok(ScanOutput::empty(ScanStatus::Completed));
        }

        let failed = {
            let mut cache = self
                .failed_nameservers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            if cache.expires_at.is_none_or(|expires| now > expires) {
                cache.failed.clear();
                cache.expires_at = Some(now + FAILED_NAMESERVER_CACHE_TTL);
            }
            if cache.configuration != nameservers || cache.revision != revision {
                cache.configuration = nameservers.clone();
                cache.revision = revision;
                cache.failed.clear();
            }
            cache.failed.clone()
        };
        let mut failed_this_run = Vec::new();
        let mut hostnames = BTreeMap::<Ipv4Addr, String>::new();
        for nameserver in nameservers {
            if failed.contains(&nameserver) {
                continue;
            }
            let unresolved = addresses
                .iter()
                .copied()
                .filter(|ip| !hostnames.contains_key(ip))
                .collect::<Vec<_>>();
            for chunk in unresolved.chunks(PTR_BATCH_SIZE) {
                let lookups = chunk.iter().copied().map(|ip| {
                    let resolver = Arc::clone(&self.resolver);
                    let timeout = self.config.ptr_timeout;
                    async move {
                        let hostname = resolver.lookup_ptr(nameserver, ip, timeout).await;
                        (ip, hostname)
                    }
                });
                let results = match await_or_cancel(join_all(lookups), cancellation).await {
                    Step::Cancelled => return Ok(ScanOutput::empty(ScanStatus::Cancelled)),
                    Step::Ready(results) => results,
                };
                for (ip, name) in results {
                    if let Some(hostname) = name.and_then(|name| short_hostname(&name)) {
                        hostnames.entry(ip).or_insert(hostname);
                    }
                }
            }
            // Match upstream failover: the first local nameserver producing
            // any usable PTR answer ends the lookup pass.
            if !hostnames.is_empty() {
                self.failed_nameservers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .failed
                    .extend(failed_this_run);
                break;
            }
            failed_this_run.push(nameserver);
        }

        if hostnames.is_empty() {
            return Ok(ScanOutput::empty(ScanStatus::Completed));
        }
        let named_ips = hostnames.keys().copied().collect::<Vec<_>>();
        let neighbors = match self.neighbors_for(&named_ips, cancellation).await {
            Step::Cancelled => return Ok(ScanOutput::empty(ScanStatus::Cancelled)),
            Step::Ready(result) => result?,
        };
        let observations = hostnames
            .into_iter()
            .filter_map(|(ip, hostname)| {
                neighbors.get(&ip).map(|mac| HostObservation {
                    ip,
                    hostname,
                    mac: mac.clone(),
                })
            })
            .collect();
        Ok(ScanOutput {
            status: ScanStatus::Completed,
            observations,
        })
    }

    async fn neighbors_for(
        &self,
        ips: &[Ipv4Addr],
        cancellation: &mut watch::Receiver<bool>,
    ) -> Step<Result<HashMap<Ipv4Addr, String>, ScanError>> {
        if ips.is_empty() {
            return Step::Ready(Ok(HashMap::new()));
        }
        let initial = match await_or_cancel(self.network.read_neighbors(ips), cancellation).await {
            Step::Cancelled => return Step::Cancelled,
            Step::Ready(Ok(result)) => result,
            Step::Ready(Err(error)) => return Step::Ready(Err(error)),
        };
        let requested = ips.iter().copied().collect::<HashSet<_>>();
        let mut neighbors = accepted_neighbors(initial, &requested);
        let missing = ips
            .iter()
            .copied()
            .filter(|ip| !neighbors.contains_key(ip))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Step::Ready(Ok(neighbors));
        }

        match await_or_cancel(self.network.probe_neighbors(&missing), cancellation).await {
            Step::Cancelled => return Step::Cancelled,
            Step::Ready(Ok(())) => {}
            Step::Ready(Err(error)) => return Step::Ready(Err(error)),
        }
        if await_or_cancel(
            tokio::time::sleep(self.config.neighbor_cache_wait),
            cancellation,
        )
        .await
        .is_cancelled()
        {
            return Step::Cancelled;
        }
        let refreshed =
            match await_or_cancel(self.network.read_neighbors(&missing), cancellation).await {
                Step::Cancelled => return Step::Cancelled,
                Step::Ready(Ok(result)) => result,
                Step::Ready(Err(error)) => return Step::Ready(Err(error)),
            };
        neighbors.extend(accepted_neighbors(refreshed, &requested));
        Step::Ready(Ok(neighbors))
    }
}

fn accepted_nameservers(nameservers: &[IpAddr]) -> Vec<IpAddr> {
    let mut accepted = Vec::new();
    for nameserver in nameservers {
        let IpAddr::V4(ip) = nameserver else {
            continue;
        };
        if !is_local_nameserver(*ip) || accepted.contains(nameserver) {
            continue;
        }
        accepted.push(*nameserver);
    }
    accepted
}

fn is_local_nameserver(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
}

fn accepted_neighbors(
    entries: Vec<NeighborEntry>,
    requested: &HashSet<Ipv4Addr>,
) -> HashMap<Ipv4Addr, String> {
    entries
        .into_iter()
        .filter(|entry| requested.contains(&entry.ip) && usable_neighbor_ip(entry.ip))
        .filter_map(|entry| normalize_mac(&entry.mac).map(|mac| (entry.ip, mac)))
        .collect()
}

fn usable_neighbor_ip(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_link_local() && !ip.is_multicast() && !ip.is_unspecified()
}

fn normalize_mac(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 11 || bytes.len() > 17 {
        return None;
    }
    let separator = if value.contains(':') { ':' } else { '-' };
    let octets = value.split(separator).collect::<Vec<_>>();
    if octets.len() != 6
        || octets.iter().any(|octet| {
            octet.is_empty() || octet.len() > 2 || !octet.bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return None;
    }
    let normalized = octets
        .iter()
        .map(|octet| format!("{:0>2}", octet.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(&separator.to_string());
    let colon_form = normalized.replace('-', ":");
    if colon_form == "00:00:00:00:00:00" || colon_form == "ff:ff:ff:ff:ff:ff" {
        return None;
    }
    Some(normalized)
}

fn short_hostname(name: &str) -> Option<String> {
    let short = name.split('.').next()?;
    if short.is_empty()
        || short.len() > 63
        || !short
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !short.as_bytes()[0].is_ascii_alphanumeric()
        || !short.as_bytes()[short.len() - 1].is_ascii_alphanumeric()
    {
        return None;
    }
    if short.starts_with("xn--") {
        let (decoded, result) = idna::domain_to_unicode(short);
        if result.is_ok() {
            return Some(decoded);
        }
    }
    Some(short.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv4Subnet {
    network: u32,
    prefix: u8,
    address_count: u64,
}

impl Ipv4Subnet {
    fn new(address: Ipv4Addr, prefix: u8) -> Result<Self, ScanError> {
        if prefix > 32 {
            return Err(ScanError::InvalidPrefix(prefix));
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Self {
            network: u32::from(address) & mask,
            prefix,
            address_count: 1u64 << (32 - prefix),
        })
    }

    fn contains(self, ip: Ipv4Addr) -> bool {
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix)
        };
        u32::from(ip) & mask == self.network
    }

    fn hosts(self) -> Vec<Ipv4Addr> {
        let last = self.network + (self.address_count as u32) - 1;
        let (first_host, last_host) = if self.prefix <= 30 {
            (self.network + 1, last - 1)
        } else {
            (self.network, last)
        };
        (first_host..=last_host).map(Ipv4Addr::from).collect()
    }
}

enum Step<T> {
    Ready(T),
    Cancelled,
}

impl<T> Step<T> {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

async fn await_or_cancel<T>(
    future: impl Future<Output = T>,
    cancellation: &mut watch::Receiver<bool>,
) -> Step<T> {
    if is_cancelled(cancellation) {
        return Step::Cancelled;
    }
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancellation) => Step::Cancelled,
        value = future => Step::Ready(value),
    }
}

async fn wait_for_cancel(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if is_cancelled(cancellation) || cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn is_cancelled(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

struct ScanGuard<'a>(&'a AtomicBool);

impl<'a> ScanGuard<'a> {
    fn try_acquire(running: &'a AtomicBool) -> Option<Self> {
        running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(running))
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
