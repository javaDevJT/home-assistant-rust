//! Weak-handle DHCP observation cache and discovery-flow admission.
use crate::DhcpMatcherIndex;
use hass_config::{DiscoveryKey, DiscoveryKeyValue};
use hass_core::{Error, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
/// Home Assistant's DHCP discovery-key domain.
pub const DHCP_DOMAIN: &str = "dhcp";
/// Home Assistant's DHCP discovery-flow source.
pub const DHCP_SOURCE: &str = "dhcp";
/// A provider's raw observation. The incoming IP string is retained for flow
/// data; only the cache uses the parsed compressed form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DhcpObservation {
    pub ip: String,
    pub hostname: String,
    pub mac: String,
}
/// One cached address-data row, keyed by the canonical MAC string.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DhcpAddressData {
    /// Compressed IP text, as stored by Home Assistant's DHCP cache.
    pub ip: String,
    /// Original hostname casing, as stored by the cache.
    pub hostname: String,
}
/// Source-compatible service information sent to a config flow or snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DhcpServiceInfo {
    pub ip: String,
    pub hostname: String,
    /// Rust-facing shorthand; Home Assistant's service-info field is named
    /// `macaddress` and remains so in serialized discovery data.
    #[serde(rename = "macaddress")]
    pub mac: String,
}
/// An accepted add/update notification, matching the one-record callback sent
/// by Home Assistant's internal DHCP callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DhcpUpdate {
    pub mac: String,
    pub data: DhcpAddressData,
}
/// One target integration's admission request to the shared flow queue.
#[derive(Clone, Debug, PartialEq)]
pub struct DhcpDiscoveryAction {
    /// Target integration domain, not the discovery-key domain.
    pub domain: String,
    pub source: String,
    pub discovery_key: DiscoveryKey,
    /// Extra caller context. Kept empty for DHCP; `DiscoveryHandle::create`
    /// adds `source` and `discovery_key` to its context.
    pub context: Map<String, Value>,
    /// Serialized source-compatible `DhcpServiceInfo` data.
    pub data: Map<String, Value>,
}
/// Synchronous admission callback for the shared native discovery queue.
pub type DiscoveryActionSink =
    Arc<dyn Fn(DhcpDiscoveryAction) -> Result<()> + Send + Sync + 'static>;
/// Weak adapter over registered device connections and their config-entry
/// integration domains. Return every domain associated with this formatted
/// network-MAC connection; the SDK filters them through the generated
/// `registered_devices` matcher set.
pub trait RegisteredDeviceProvider: Send + Sync + 'static {
    fn domains_for_network_mac(&self, formatted_mac: &str) -> Vec<String>;
}
/// Result of one provider observation or forced cache replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestReport {
    /// False means invalid IP or a source-compatible duplicate was ignored.
    pub accepted: bool,
    /// Present for accepted observations, even when no integration matched.
    pub service_info: Option<DhcpServiceInfo>,
    /// Unique target domains, in deterministic first-match order.
    pub matched_domains: Vec<String>,
}
impl IngestReport {
    fn ignored() -> Self {
        Self {
            accepted: false,
            service_info: None,
            matched_domains: Vec::new(),
        }
    }
}
type UpdateListener = Arc<dyn Fn(DhcpUpdate) + Send + Sync + 'static>;
struct State {
    stopped: bool,
    address_data: IndexMap<String, DhcpAddressData>,
    listeners: IndexMap<u64, UpdateListener>,
}
struct Inner {
    state: Mutex<State>,
    matchers: DhcpMatcherIndex,
    registered_devices: Option<Weak<dyn RegisteredDeviceProvider>>,
    sink: Option<DiscoveryActionSink>,
    next_listener_id: AtomicU64,
}
/// Strong owner for the process-lifetime DHCP cache and matcher manager.
///
/// Provider adapters retain only the weak [`DhcpManager`] returned here, so
/// dropping the owner stops further ingestion and update registration.
pub struct DhcpIntegration {
    inner: Arc<Inner>,
}
/// Weak provider handle for observations, snapshots, replay, and listeners.
#[derive(Clone)]
pub struct DhcpManager {
    inner: Weak<Inner>,
}
/// Registration for exactly one update listener.
///
/// The guard does not keep the DHCP owner alive and dropping it removes only
/// the listener ID it installed.
pub struct DhcpUpdateGuard {
    inner: Weak<Inner>,
    id: u64,
}
impl DhcpIntegration {
    /// Create an SDK instance using the pinned generated matcher list.
    pub fn new(
        registered_devices: Option<Weak<dyn RegisteredDeviceProvider>>,
        sink: Option<DiscoveryActionSink>,
    ) -> Self {
        Self::with_matchers(DhcpMatcherIndex::generated(), registered_devices, sink)
    }
    /// Create an SDK instance with an explicit generated-plus-custom matcher
    /// index, weak registry adapter, and optional shared discovery sink.
    pub fn with_matchers(
        matchers: DhcpMatcherIndex,
        registered_devices: Option<Weak<dyn RegisteredDeviceProvider>>,
        sink: Option<DiscoveryActionSink>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    stopped: false,
                    address_data: IndexMap::new(),
                    listeners: IndexMap::new(),
                }),
                matchers,
                registered_devices,
                sink,
                next_listener_id: AtomicU64::new(1),
            }),
        }
    }
    /// Return a weak SDK handle for providers and read surfaces.
    pub fn manager(&self) -> DhcpManager {
        DhcpManager {
            inner: Arc::downgrade(&self.inner),
        }
    }
}
impl Drop for DhcpIntegration {
    fn drop(&mut self) {
        let mut state = lock(&self.inner.state);
        state.stopped = true;
        state.listeners.clear();
    }
}
impl DhcpManager {
    /// Ingest an observation through source validation, cache, matching,
    /// update notification, and flow admission. `force` bypasses only the
    /// source-compatible duplicate check.
    pub fn ingest(&self, observation: DhcpObservation, force: bool) -> Result<IngestReport> {
        let inner = self.inner()?;
        let Some(compressed_ip) = normalize_ip(&observation.ip) else {
            return Ok(IngestReport::ignored());
        };
        let formatted_mac = format_mac(&observation.mac);
        let cache_mac = formatted_mac.replace(':', "");
        let uppercase_mac = cache_mac.to_uppercase();
        let lowercase_hostname = observation.hostname.to_lowercase();
        let data = DhcpAddressData {
            ip: compressed_ip.clone(),
            hostname: observation.hostname.clone(),
        };
        let listeners = {
            let mut state = lock(&inner.state);
            if state.stopped {
                return Err(stopped_error());
            }
            if !force
                && let Some(current) = state.address_data.get(&cache_mac)
                && current.ip == compressed_ip
                && current.hostname.starts_with(&observation.hostname)
            {
                return Ok(IngestReport::ignored());
            }
            // IndexMap retains the first-seen MAC position when an existing
            // row is updated, matching Python dict insertion order.
            state.address_data.insert(cache_mac.clone(), data.clone());
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        let mut registered_domains = inner
            .registered_devices
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|provider| provider.domains_for_network_mac(&formatted_mac))
            .unwrap_or_default();
        registered_domains.sort();
        registered_domains.dedup();
        let matched_domains = inner.matchers.matching_domains(
            &lowercase_hostname,
            &uppercase_mac,
            registered_domains,
        );
        let update = DhcpUpdate {
            mac: cache_mac.clone(),
            data,
        };
        for listener in listeners {
            // A listener is an observation surface, not an owner of the cache.
            // Isolate a panic so it cannot suppress other listeners or flows.
            let _ = catch_unwind(AssertUnwindSafe(|| listener(update.clone())));
        }
        let service_info = DhcpServiceInfo {
            ip: observation.ip,
            hostname: lowercase_hostname,
            mac: cache_mac.clone(),
        };
        if let Some(sink) = &inner.sink {
            let discovery_key = DiscoveryKey {
                domain: DHCP_DOMAIN.to_owned(),
                key: DiscoveryKeyValue::String(cache_mac),
                version: 1,
            };
            let data = serde_json::to_value(&service_info)
                .map_err(|error| Error::Service(format!("invalid DHCP service info: {error}")))?
                .as_object()
                .cloned()
                .ok_or_else(|| Error::Service("invalid DHCP service-info data".into()))?;
            for domain in &matched_domains {
                sink(DhcpDiscoveryAction {
                    domain: domain.clone(),
                    source: DHCP_SOURCE.to_owned(),
                    discovery_key: discovery_key.clone(),
                    context: Map::new(),
                    data: data.clone(),
                })?;
            }
        }
        Ok(IngestReport {
            accepted: true,
            service_info: Some(service_info),
            matched_domains,
        })
    }
    /// Return one normalized service-info row per cached MAC, in cache order.
    pub fn service_info_snapshot(&self) -> Result<Vec<DhcpServiceInfo>> {
        let inner = self.inner()?;
        let state = lock(&inner.state);
        if state.stopped {
            return Err(stopped_error());
        }
        Ok(state
            .address_data
            .iter()
            .map(|(mac, data)| DhcpServiceInfo {
                ip: data.ip.clone(),
                hostname: data.hostname.to_lowercase(),
                mac: mac.clone(),
            })
            .collect())
    }
    /// Return a cloned raw cache snapshot in MAC insertion order.
    pub fn address_data_snapshot(&self) -> Result<Vec<(String, DhcpAddressData)>> {
        let inner = self.inner()?;
        let state = lock(&inner.state);
        if state.stopped {
            return Err(stopped_error());
        }
        Ok(state
            .address_data
            .iter()
            .map(|(mac, data)| (mac.clone(), data.clone()))
            .collect())
    }
    /// Register one add/update listener. Dropping the returned guard removes
    /// exactly this listener and does not retain the SDK owner.
    pub fn register_update_listener<F>(&self, listener: F) -> Result<DhcpUpdateGuard>
    where
        F: Fn(DhcpUpdate) + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let mut state = lock(&inner.state);
        if state.stopped {
            return Err(stopped_error());
        }
        state.listeners.insert(id, Arc::new(listener));
        Ok(DhcpUpdateGuard {
            inner: Arc::downgrade(&inner),
            id,
        })
    }
    /// Force cached DHCP version-1 string keys through normal matching and
    /// admission. Other key domains/types/versions and uncached MACs are
    /// ignored; cache rows are never removed.
    pub fn rediscover(&self, keys: &[DiscoveryKey]) -> Result<Vec<IngestReport>> {
        let inner = self.inner()?;
        let mut reports = Vec::new();
        for key in keys {
            if key.domain != DHCP_DOMAIN || key.version != 1 {
                continue;
            }
            let DiscoveryKeyValue::String(mac) = &key.key else {
                continue;
            };
            let data = {
                let state = lock(&inner.state);
                if state.stopped {
                    return Err(stopped_error());
                }
                state.address_data.get(mac).cloned()
            };
            let Some(data) = data else {
                continue;
            };
            reports.push(self.ingest(
                DhcpObservation {
                    ip: data.ip,
                    hostname: data.hostname,
                    mac: mac.clone(),
                },
                true,
            )?);
        }
        Ok(reports)
    }
    fn inner(&self) -> Result<Arc<Inner>> {
        let inner = self.inner.upgrade().ok_or_else(stopped_error)?;
        if lock(&inner.state).stopped {
            return Err(stopped_error());
        }
        Ok(inner)
    }
}
impl Drop for DhcpUpdateGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        lock(&inner.state).listeners.shift_remove(&self.id);
    }
}
fn stopped_error() -> Error {
    Error::Service("DHCP discovery owner is no longer available".into())
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// Convert an accepted address to Python `ipaddress` compressed form while
/// applying the DHCP integration's exact rejection predicates.
fn normalize_ip(input: &str) -> Option<String> {
    let (address_text, zone) = match input.split_once('%') {
        Some((address, zone)) if !zone.is_empty() && !zone.contains('%') => (address, Some(zone)),
        Some(_) => return None,
        None => (input, None),
    };
    let address = address_text.parse::<IpAddr>().ok()?;
    match address {
        IpAddr::V4(address) => {
            if zone.is_some()
                || is_ipv4_link_local(address)
                || address.is_loopback()
                || address.is_unspecified()
            {
                return None;
            }
            Some(address.to_string())
        }
        IpAddr::V6(address) => {
            if is_ipv6_link_local(address) || address.is_loopback() || address.is_unspecified() {
                return None;
            }
            let mut compressed = compress_ipv6(address);
            if let Some(zone) = zone {
                compressed.push('%');
                compressed.push_str(zone);
            }
            Some(compressed)
        }
    }
}
fn is_ipv4_link_local(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 169 && octets[1] == 254
}
fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    (u128::from(address) & 0xffc0_0000_0000_0000_0000_0000_0000_0000)
        == 0xfe80_0000_0000_0000_0000_0000_0000_0000
}
fn compress_ipv6(address: Ipv6Addr) -> String {
    let segments = address.segments();
    let mut best_run: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < segments.len() {
        if segments[index] != 0 {
            index += 1;
            continue;
        }
        let start = index;
        while index < segments.len() && segments[index] == 0 {
            index += 1;
        }
        let length = index - start;
        let current_best = best_run.map(|(_, best_length)| best_length).unwrap_or(0);
        if length >= 2 && length > current_best {
            best_run = Some((start, index));
        }
    }
    let Some((start, end)) = best_run else {
        return segments
            .iter()
            .map(|segment| format!("{segment:x}"))
            .collect::<Vec<_>>()
            .join(":");
    };
    let left = segments[..start]
        .iter()
        .map(|segment| format!("{segment:x}"))
        .collect::<Vec<_>>()
        .join(":");
    let right = segments[end..]
        .iter()
        .map(|segment| format!("{segment:x}"))
        .collect::<Vec<_>>()
        .join(":");
    match (left.is_empty(), right.is_empty()) {
        (true, true) => "::".to_owned(),
        (true, false) => format!("::{right}"),
        (false, true) => format!("{left}::"),
        (false, false) => format!("{left}::{right}"),
    }
}
/// Home Assistant's `device_registry.format_mac` implementation, including
/// its permissive fallback for unknown formats.
/// Format a MAC address with Home Assistant's pinned `device_registry`
/// behavior. Known common forms are lowercased and colon-delimited; unknown
/// formats are deliberately returned unchanged rather than rejected.
pub fn format_mac(mac: &str) -> String {
    let mut to_test = mac.to_owned();
    let length = to_test.chars().count();
    if length == 17 && to_test.matches(':').count() == 5 {
        return to_test.to_lowercase();
    }
    if length == 17 && to_test.matches('-').count() == 5 {
        to_test = to_test.replace('-', "");
    } else if length == 14 && to_test.matches('.').count() == 2 {
        to_test = to_test.replace('.', "");
    }
    if to_test.chars().count() == 12 {
        let lowercase: Vec<char> = to_test.to_lowercase().chars().collect();
        return (0..12)
            .step_by(2)
            .map(|offset| lowercase.iter().skip(offset).take(2).collect::<String>())
            .collect::<Vec<_>>()
            .join(":");
    }
    mac.to_owned()
}
