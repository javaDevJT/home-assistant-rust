//! Native Home Assistant network adapter selection and source-address helpers.
//!
//! The integration owns a selected, point-in-time adapter snapshot and the
//! persisted adapter-name preference. Consumers receive a weak [`NetworkManager`]
//! handle so dropping [`NetworkIntegration`] deactivates that access.

use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, UdpSocket},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use hass_core::{Error, Result, Runtime};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Home Assistant Store key for Network selection.
pub const STORAGE_KEY: &str = "core.network";
/// Home Assistant Store schema version for Network selection.
pub const STORAGE_VERSION: u8 = 1;

const MDNS_TARGET: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const SOURCE_TARGETS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(8, 8, 8, 8),
    MDNS_TARGET,
    Ipv4Addr::new(127, 0, 0, 1),
];

/// One IPv4 address and its interface prefix length.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ipv4AddressRecord {
    /// Interface address.
    pub address: Ipv4Addr,
    /// Prefix length in bits.
    pub network_prefix: u8,
}

/// One IPv6 address and the scope values reported by the enumerator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ipv6AddressRecord {
    /// Interface address.
    pub address: Ipv6Addr,
    /// Address flow information.
    pub flowinfo: u32,
    /// IPv6 scope identifier. Zero is retained as a meaningful value.
    pub scope_id: u32,
    /// Prefix length in bits.
    pub network_prefix: u8,
}

/// Address-only result for one enumerated adapter, before Network selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AdapterAddresses {
    /// Exact adapter name used as the selection identity.
    pub name: String,
    /// Interface index when supplied by the enumerator.
    pub index: Option<u32>,
    /// IPv4 records in enumerator order.
    pub ipv4: Vec<Ipv4AddressRecord>,
    /// IPv6 records in enumerator order.
    pub ipv6: Vec<Ipv6AddressRecord>,
}

/// Selected Network adapter snapshot, shaped for native consumers and APIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkAdapter {
    /// Exact adapter name used as the selection identity.
    pub name: String,
    /// Interface index when supplied by the enumerator.
    pub index: Option<u32>,
    /// Whether the current configured or automatic selection enables it.
    pub enabled: bool,
    /// Whether Home Assistant's fallback auto-selection includes it.
    pub auto: bool,
    /// Whether its IPv4 address matched the mDNS route probe.
    pub default: bool,
    /// IPv4 records in enumerator order.
    pub ipv4: Vec<Ipv4AddressRecord>,
    /// IPv6 records in enumerator order.
    pub ipv6: Vec<Ipv6AddressRecord>,
}

/// An enabled source address, retaining an IPv6 zone identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScopedIp {
    /// An IPv4 source address.
    V4(Ipv4Addr),
    /// An IPv6 source address and its scope identifier, including scope zero.
    V6 {
        /// IPv6 source address.
        address: Ipv6Addr,
        /// Scope identifier preserved from the selected adapter record.
        scope_id: u32,
    },
}

impl ScopedIp {
    /// Return the address without its IPv6 scope identifier.
    pub const fn address(self) -> IpAddr {
        match self {
            Self::V4(address) => IpAddr::V4(address),
            Self::V6 { address, .. } => IpAddr::V6(address),
        }
    }

    /// Return the IPv6 scope identifier, when this is an IPv6 address.
    pub const fn scope_id(self) -> Option<u32> {
        match self {
            Self::V4(_) => None,
            Self::V6 { scope_id, .. } => Some(scope_id),
        }
    }
}

impl fmt::Display for ScopedIp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(address) => address.fmt(formatter),
            Self::V6 { address, scope_id } => write!(formatter, "{address}%{scope_id}"),
        }
    }
}

impl Serialize for ScopedIp {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ScopedIp {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if let Ok(address) = value.parse::<Ipv4Addr>() {
            return Ok(Self::V4(address));
        }

        if let Some((address, scope_id)) = value.rsplit_once('%') {
            let address = address.parse::<Ipv6Addr>().map_err(D::Error::custom)?;
            let scope_id = scope_id.parse::<u32>().map_err(D::Error::custom)?;
            return Ok(Self::V6 { address, scope_id });
        }

        value
            .parse::<Ipv6Addr>()
            .map(|address| Self::V6 {
                address,
                scope_id: 0,
            })
            .map_err(D::Error::custom)
    }
}

/// Injectable adapter enumeration boundary.
pub trait AdapterEnumerator: Send + Sync {
    /// Enumerate current adapter names, indices, and addresses.
    fn enumerate(&self) -> Result<Vec<AdapterAddresses>>;
}

/// Injectable route-source probe boundary.
pub trait RouteSourceResolver: Send + Sync {
    /// Return the IPv4 source address selected for a target, or `None` on failure.
    fn source_ip(&self, target: Ipv4Addr) -> Option<Ipv4Addr>;
}

/// The production adapter enumerator backed by `if-addrs`.
///
/// `if-addrs` reports operating-system interface names rather than `ifaddr`'s
/// `nice_name`. It provides an interface index and IPv6 prefix, but not IPv6
/// flow information or per-address scope IDs; this adapter reports flowinfo
/// zero and uses the interface index as the scope ID for link-local IPv6 (zero
/// otherwise).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemAdapterEnumerator;

impl AdapterEnumerator for SystemAdapterEnumerator {
    fn enumerate(&self) -> Result<Vec<AdapterAddresses>> {
        let interfaces = if_addrs::get_if_addrs().map_err(|error| {
            Error::Service(format!("could not enumerate network adapters: {error}"))
        })?;

        let mut adapters = Vec::<AdapterAddresses>::new();
        let mut positions = HashMap::<String, usize>::new();
        for interface in interfaces {
            let index = interface.index;
            let position = if let Some(position) = positions.get(&interface.name) {
                *position
            } else {
                let position = adapters.len();
                positions.insert(interface.name.clone(), position);
                adapters.push(AdapterAddresses {
                    name: interface.name.clone(),
                    index,
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                });
                position
            };

            let adapter = &mut adapters[position];
            if adapter.index.is_none() {
                adapter.index = index;
            }
            match interface.addr {
                if_addrs::IfAddr::V4(address) => adapter.ipv4.push(Ipv4AddressRecord {
                    address: address.ip,
                    network_prefix: address.prefixlen,
                }),
                if_addrs::IfAddr::V6(address) => adapter.ipv6.push(Ipv6AddressRecord {
                    address: address.ip,
                    flowinfo: 0,
                    scope_id: if is_ipv6_link_local(address.ip) {
                        index.unwrap_or(0)
                    } else {
                        0
                    },
                    network_prefix: address.prefixlen,
                }),
            }
        }

        Ok(adapters)
    }
}

/// The production IPv4 route-source resolver.
///
/// It connects a UDP socket and reads its local address. UDP `connect` selects
/// a route without sending a datagram, and targets are numeric IPv4 addresses
/// so this probe does not perform DNS lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRouteSourceResolver;

impl RouteSourceResolver for SystemRouteSourceResolver {
    fn source_ip(&self, target: Ipv4Addr) -> Option<Ipv4Addr> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        socket.connect(SocketAddrV4::new(target, 1)).ok()?;
        match socket.local_addr().ok()?.ip() {
            IpAddr::V4(address) => Some(address),
            IpAddr::V6(_) => None,
        }
    }
}

/// Strong owner for the Network integration snapshot.
pub struct NetworkIntegration {
    inner: Arc<NetworkState>,
}

impl NetworkIntegration {
    /// Load Network preferences and take the initial injected-provider snapshot.
    pub fn start(
        runtime: Runtime,
        enumerator: Arc<dyn AdapterEnumerator>,
        resolver: Arc<dyn RouteSourceResolver>,
    ) -> Result<Self> {
        let configured_adapters = load_configured_adapters(&runtime)?;
        // Provider callbacks are intentionally outside all Network locks.
        let addresses = enumerator.enumerate()?;
        let default_source = resolver.source_ip(MDNS_TARGET);
        let adapters = build_adapter_snapshot(addresses, default_source, &configured_adapters);

        Ok(Self {
            inner: Arc::new(NetworkState {
                runtime,
                enumerator,
                resolver,
                snapshot: RwLock::new(NetworkSnapshot {
                    configured_adapters,
                    adapters,
                }),
                operation_lock: Mutex::new(()),
                active: AtomicBool::new(true),
            }),
        })
    }

    /// Return a weak read and mutation handle for this integration.
    pub fn manager(&self) -> NetworkManager {
        NetworkManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Deactivate this integration and invalidate its weak manager handles.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for NetworkIntegration {
    fn drop(&mut self) {
        let _operation = lock_unpoisoned(&self.inner.operation_lock);
        self.inner.active.store(false, Ordering::Release);
    }
}

/// Weak access to the selected Network snapshot and preferences.
#[derive(Clone)]
pub struct NetworkManager {
    inner: Weak<NetworkState>,
}

impl NetworkManager {
    /// Return the current adapter snapshot in provider order.
    pub fn adapters(&self) -> Result<Vec<NetworkAdapter>> {
        let inner = self.upgrade()?;
        ensure_active(&inner)?;
        Ok(read_unpoisoned(&inner.snapshot).adapters.clone())
    }

    /// Return the complete persisted configured-name list, including unknown or duplicate names.
    pub fn configured_adapters(&self) -> Result<Vec<String>> {
        let inner = self.upgrade()?;
        ensure_active(&inner)?;
        Ok(read_unpoisoned(&inner.snapshot).configured_adapters.clone())
    }

    /// Persist and apply a configured-name list atomically with respect to the visible snapshot.
    ///
    /// The complete input list is retained, including names that do not match
    /// an enumerated adapter and repeated names. Adapter selection is published
    /// only after `Runtime::set_data` succeeds.
    pub fn configure(&self, configured_adapters: Vec<String>) -> Result<Vec<String>> {
        let inner = self.upgrade()?;
        let _operation = lock_unpoisoned(&inner.operation_lock);
        ensure_active(&inner)?;
        let current = read_unpoisoned(&inner.snapshot).clone();
        let adapters = apply_selection(current.adapters, &configured_adapters);
        let value = stored_value(&configured_adapters)?;

        inner.runtime.set_data(STORAGE_KEY, &value)?;
        *write_unpoisoned(&inner.snapshot) = NetworkSnapshot {
            configured_adapters: configured_adapters.clone(),
            adapters,
        };
        Ok(configured_adapters)
    }

    /// Re-enumerate adapters and apply the latest configured-name list.
    pub fn reload_adapters(&self) -> Result<()> {
        let inner = self.upgrade()?;
        // Call injected providers before taking the operation lock. A provider
        // may re-enter this manager; the names are read after it returns.
        let addresses = inner.enumerator.enumerate()?;
        let default_source = inner.resolver.source_ip(MDNS_TARGET);

        let _operation = lock_unpoisoned(&inner.operation_lock);
        ensure_active(&inner)?;
        let current = read_unpoisoned(&inner.snapshot).clone();
        let adapters =
            build_adapter_snapshot(addresses, default_source, &current.configured_adapters);
        *write_unpoisoned(&inner.snapshot) = NetworkSnapshot {
            configured_adapters: current.configured_adapters,
            adapters,
        };
        Ok(())
    }

    /// Return enabled IPv4 and IPv6 source addresses in adapter/address order.
    ///
    /// IPv6 flowinfo is not part of a source address and is omitted; its scope
    /// ID is retained, including when the ID is zero. This helper does not
    /// apply SSDP, Zeroconf, deduplication, or protocol-specific filtering.
    pub fn enabled_source_ips(&self) -> Result<Vec<ScopedIp>> {
        let inner = self.upgrade()?;
        ensure_active(&inner)?;
        let snapshot = read_unpoisoned(&inner.snapshot);
        let mut sources = Vec::new();
        for adapter in &snapshot.adapters {
            if !adapter.enabled {
                continue;
            }
            sources.extend(
                adapter
                    .ipv4
                    .iter()
                    .map(|record| ScopedIp::V4(record.address)),
            );
            sources.extend(adapter.ipv6.iter().map(|record| ScopedIp::V6 {
                address: record.address,
                scope_id: record.scope_id,
            }));
        }
        Ok(sources)
    }

    /// Return the route-selected IPv4 source for an optional target.
    ///
    /// When `target` is absent, probes run in order for `8.8.8.8`,
    /// `224.0.0.251`, and `127.0.0.1`. If enabled IPv4 records exist, a route
    /// result outside that set falls back to the first enabled record. Without
    /// enabled IPv4 records, the route result is returned as-is; if all probes
    /// fail this method returns an error.
    pub fn source_ip(&self, target: Option<Ipv4Addr>) -> Result<Ipv4Addr> {
        let inner = self.upgrade()?;
        ensure_active(&inner)?;
        // Capture a stable source list and drop the state lock before invoking
        // the resolver, which is an injected callback.
        let enabled_ipv4: Vec<Ipv4Addr> = read_unpoisoned(&inner.snapshot)
            .adapters
            .iter()
            .filter(|adapter| adapter.enabled)
            .flat_map(|adapter| adapter.ipv4.iter().map(|record| record.address))
            .collect();

        let route_source = if let Some(target) = target {
            inner.resolver.source_ip(target)
        } else {
            SOURCE_TARGETS
                .into_iter()
                .find_map(|candidate| inner.resolver.source_ip(candidate))
        };
        ensure_active(&inner)?;

        if let Some(first) = enabled_ipv4.first() {
            Ok(route_source
                .filter(|source| enabled_ipv4.contains(source))
                .unwrap_or(*first))
        } else {
            route_source.ok_or_else(|| {
                Error::Service("could not determine network source IPv4 address".into())
            })
        }
    }

    fn upgrade(&self) -> Result<Arc<NetworkState>> {
        self.inner
            .upgrade()
            .ok_or_else(|| Error::NotFound("network integration is shut down".into()))
    }
}

struct NetworkState {
    runtime: Runtime,
    enumerator: Arc<dyn AdapterEnumerator>,
    resolver: Arc<dyn RouteSourceResolver>,
    snapshot: RwLock<NetworkSnapshot>,
    operation_lock: Mutex<()>,
    active: AtomicBool,
}

#[derive(Clone)]
struct NetworkSnapshot {
    configured_adapters: Vec<String>,
    adapters: Vec<NetworkAdapter>,
}

#[derive(Deserialize)]
struct StoredEnvelope {
    version: u8,
    key: String,
    data: StoredNetworkData,
}

#[derive(Deserialize)]
struct StoredNetworkData {
    #[serde(default)]
    configured_adapters: Vec<String>,
}

#[derive(Serialize)]
struct StoredValue<'a> {
    version: u8,
    key: &'static str,
    data: StoredValueData<'a>,
}

#[derive(Serialize)]
struct StoredValueData<'a> {
    configured_adapters: &'a [String],
}

fn load_configured_adapters(runtime: &Runtime) -> Result<Vec<String>> {
    let Some(value) = runtime.get_data(STORAGE_KEY)? else {
        return Ok(Vec::new());
    };
    let envelope: StoredEnvelope = serde_json::from_value(value).map_err(|error| {
        Error::Storage(format!("invalid {STORAGE_KEY} store envelope: {error}"))
    })?;
    if envelope.version != STORAGE_VERSION || envelope.key != STORAGE_KEY {
        return Err(Error::Storage(format!(
            "unsupported {STORAGE_KEY} store envelope identity: version {}, key {:?}",
            envelope.version, envelope.key
        )));
    }
    Ok(envelope.data.configured_adapters)
}

fn ensure_active(inner: &NetworkState) -> Result<()> {
    if inner.active.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(Error::NotFound("network integration is shut down".into()))
    }
}

fn stored_value(configured_adapters: &[String]) -> Result<serde_json::Value> {
    serde_json::to_value(StoredValue {
        version: STORAGE_VERSION,
        key: STORAGE_KEY,
        data: StoredValueData {
            configured_adapters,
        },
    })
    .map_err(|error| Error::Storage(format!("could not serialize {STORAGE_KEY} data: {error}")))
}

fn build_adapter_snapshot(
    addresses: Vec<AdapterAddresses>,
    default_source: Option<Ipv4Addr>,
    configured_adapters: &[String],
) -> Vec<NetworkAdapter> {
    let mut adapters: Vec<_> = addresses
        .into_iter()
        .map(|adapter| {
            let default = default_source
                .is_some_and(|source| adapter.ipv4.iter().any(|record| record.address == source));
            let auto =
                default && default_source.is_some_and(|source| is_external(IpAddr::V4(source)));
            NetworkAdapter {
                name: adapter.name,
                index: adapter.index,
                enabled: false,
                auto,
                default,
                ipv4: adapter.ipv4,
                ipv6: adapter.ipv6,
            }
        })
        .collect();

    if !adapters
        .iter()
        .any(|adapter| adapter.default && adapter.auto)
    {
        for adapter in &mut adapters {
            adapter.auto = adapter
                .ipv4
                .iter()
                .any(|record| is_external(IpAddr::V4(record.address)))
                || adapter
                    .ipv6
                    .iter()
                    .any(|record| is_external(IpAddr::V6(record.address)));
        }
    }

    apply_selection(adapters, configured_adapters)
}

fn apply_selection(
    mut adapters: Vec<NetworkAdapter>,
    configured_adapters: &[String],
) -> Vec<NetworkAdapter> {
    let has_configured_match = adapters
        .iter()
        .any(|adapter| configured_adapters.iter().any(|name| name == &adapter.name));
    for adapter in &mut adapters {
        adapter.enabled = if has_configured_match {
            configured_adapters.iter().any(|name| name == &adapter.name)
        } else {
            adapter.auto
        };
    }
    adapters
}

fn is_external(address: IpAddr) -> bool {
    !address.is_multicast() && !address.is_loopback() && !is_link_local(address)
}

fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => is_ipv6_link_local(address),
    }
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
}

fn lock_unpoisoned<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
