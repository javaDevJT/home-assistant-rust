use std::collections::BTreeMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use indexmap::IndexMap;
use regex::Regex;
use url::{Host, Url};

use crate::{DEFAULT_MAX_AGE_SECONDS, Headers, MessageKind, SsdpMessage};

const IGNORED_CHANGE_HEADERS: [&str; 5] = ["date", "cache-control", "server", "host", "location"];

/// Detailed or generic reason a device observation was emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SsdpSource {
    /// Generic source for any search response.
    Search,
    /// Generic source for any advertisement.
    Advertisement,
    /// First or materially changed search observation.
    SearchChanged,
    /// Repeated search observation with no relevant changes.
    SearchAlive,
    /// SSDP alive notification.
    AdvertisementAlive,
    /// SSDP byebye notification.
    AdvertisementByebye,
    /// SSDP update notification.
    AdvertisementUpdate,
}

impl SsdpSource {
    /// Return the generic category used for cache or callback filtering.
    pub fn category(self) -> Self {
        match self {
            Self::SearchAlive | Self::SearchChanged => Self::Search,
            Self::AdvertisementAlive | Self::AdvertisementByebye | Self::AdvertisementUpdate => {
                Self::Advertisement
            }
            source => source,
        }
    }

    /// Return the source string used by the reference package.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Advertisement => "advertisement",
            Self::SearchChanged => "search_changed",
            Self::SearchAlive => "search_alive",
            Self::AdvertisementAlive => "advertisement_alive",
            Self::AdvertisementByebye => "advertisement_byebye",
            Self::AdvertisementUpdate => "advertisement_update",
        }
    }
}

/// A device snapshot and the device/service type which produced an event.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackerEvent {
    /// Detailed source of the observation.
    pub source: SsdpSource,
    /// Device state after this observation was applied.
    pub device: DeviceSnapshot,
    /// Search target (`ST`) or notification type (`NT`).
    pub dst: String,
}

/// All currently retained information for a UDN.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceSnapshot {
    /// UDN extracted from the USN value, preserving spelling.
    pub udn: String,
    /// Expiry time derived from the most recent cache-control value.
    pub valid_to: DateTime<Utc>,
    /// Distinct adjusted locations and their individual expiry times.
    pub locations: BTreeMap<String, DateTime<Utc>>,
    /// Timestamp of the latest accepted search/alive/update observation.
    pub last_seen: Option<DateTime<Utc>>,
    /// Most recent search headers by `ST` value.
    pub search_headers: IndexMap<String, Headers>,
    /// Most recent advertisement headers by `NT` value.
    pub advertisement_headers: IndexMap<String, Headers>,
}

impl DeviceSnapshot {
    fn new(udn: String, valid_to: DateTime<Utc>) -> Self {
        Self {
            udn,
            valid_to,
            locations: BTreeMap::new(),
            last_seen: None,
            search_headers: IndexMap::new(),
            advertisement_headers: IndexMap::new(),
        }
    }

    /// Return the lexically first retained location, matching the reference getter.
    pub fn current_location(&self) -> Option<&str> {
        self.locations
            .first_key_value()
            .map(|(location, _)| location.as_str())
    }

    /// Return all retained locations and their individual expiry times.
    pub fn all_locations(&self) -> &BTreeMap<String, DateTime<Utc>> {
        &self.locations
    }

    /// Get the combined search/advertisement headers for a type.
    ///
    /// Search headers form the base; advertisement headers replace matching
    /// names. An internal `_source` marker is removed when both sets exist.
    pub fn combined_headers(&self, dst: &str) -> Headers {
        match (
            self.search_headers.get(dst),
            self.advertisement_headers.get(dst),
        ) {
            (Some(search), Some(advertisement)) => {
                let mut combined = search.merge(advertisement);
                combined.remove("_source");
                combined.metadata_mut().extra.remove("_source");
                combined
            }
            (Some(headers), None) | (None, Some(headers)) => headers.clone(),
            (None, None) => Headers::new(),
        }
    }

    /// Return combined headers for every known search or advertisement type.
    pub fn all_combined_headers(&self) -> IndexMap<String, Headers> {
        self.search_headers
            .keys()
            .chain(self.advertisement_headers.keys())
            .map(|dst| {
                let headers = self.combined_headers(dst);
                (dst.clone(), headers)
            })
            .collect()
    }
}

/// In-memory UDN-keyed tracker for decoded SSDP messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tracker {
    devices: IndexMap<String, DeviceSnapshot>,
}

impl Tracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one decoded packet and return a propagated event, if any.
    pub fn observation(
        &mut self,
        message: &SsdpMessage,
        now: DateTime<Utc>,
    ) -> Option<TrackerEvent> {
        match message.kind {
            MessageKind::SearchResponse => self.observe_search(message, now),
            MessageKind::Notification => self.observe_notification(message, now),
            MessageKind::SearchRequest => None,
        }
    }

    /// Get the tracked device map keyed by UDN.
    pub fn devices(&self) -> &IndexMap<String, DeviceSnapshot> {
        &self.devices
    }

    /// Get a device by its UDN.
    pub fn get(&self, udn: &str) -> Option<&DeviceSnapshot> {
        self.devices.get(udn)
    }

    /// Get a tracked device by a USN value.
    pub fn get_by_usn(&self, usn: &str) -> Option<&DeviceSnapshot> {
        self.devices.get(&udn_from_usn(usn)?)
    }

    /// Return every device that has search or advertisement headers for `dst`.
    pub fn by_type(&self, dst: &str) -> Vec<&DeviceSnapshot> {
        self.devices
            .values()
            .filter(|device| {
                device.search_headers.contains_key(dst)
                    || device.advertisement_headers.contains_key(dst)
            })
            .collect()
    }

    /// Remove expired devices and locations using the injected clock.
    ///
    /// Expiry is strict (`now > valid_to`), matching the reference tracker.
    pub fn purge(&mut self, now: DateTime<Utc>) {
        self.devices.retain(|_, device| {
            device.locations.retain(|_, valid_to| now <= *valid_to);
            now <= device.valid_to
        });
    }

    fn observe_search(
        &mut self,
        message: &SsdpMessage,
        now: DateTime<Utc>,
    ) -> Option<TrackerEvent> {
        let headers = &message.headers;
        let search_target = non_empty_header(headers, "st")?;
        let location = message.location.as_deref()?;
        if !message.udn.as_deref().is_some_and(|udn| !udn.is_empty())
            || !is_valid_location(message.original_location.as_deref().unwrap_or(location))
        {
            return None;
        }

        let udn = udn_from_usn(headers.get("usn")?)?;
        let is_new_device = !self.devices.contains_key(&udn);
        let (device, new_location) = self.see_device(message, now, udn)?;
        let is_new_service = !device.advertisement_headers.contains_key(search_target)
            && !device.search_headers.contains_key(search_target);
        let changed = is_new_device
            || is_new_service
            || new_location
            || device
                .search_headers
                .get(search_target)
                .is_some_and(|old| same_headers_differ(old, headers));

        let device = self.devices.get_mut(&device.udn)?;
        device
            .search_headers
            .insert(search_target.to_owned(), headers.clone());
        let source = if changed {
            SsdpSource::SearchChanged
        } else {
            SsdpSource::SearchAlive
        };
        Some(TrackerEvent {
            source,
            device: device.clone(),
            dst: search_target.to_owned(),
        })
    }

    fn observe_notification(
        &mut self,
        message: &SsdpMessage,
        now: DateTime<Utc>,
    ) -> Option<TrackerEvent> {
        match message.headers.get("nts")? {
            "ssdp:byebye" => self.observe_byebye(message),
            "ssdp:alive" => {
                self.observe_advertisement(message, now, SsdpSource::AdvertisementAlive)
            }
            "ssdp:update" => {
                self.observe_advertisement(message, now, SsdpSource::AdvertisementUpdate)
            }
            _ => None,
        }
    }

    fn observe_advertisement(
        &mut self,
        message: &SsdpMessage,
        now: DateTime<Utc>,
        source: SsdpSource,
    ) -> Option<TrackerEvent> {
        let headers = &message.headers;
        let notification_type = non_empty_header(headers, "nt")?;
        let location = message.location.as_deref()?;
        if !message.udn.as_deref().is_some_and(|udn| !udn.is_empty())
            || !is_valid_location(message.original_location.as_deref().unwrap_or(location))
        {
            return None;
        }

        let udn = udn_from_usn(headers.get("usn")?)?;
        let is_new_device = !self.devices.contains_key(&udn);
        let (device, new_location) = self.see_device(message, now, udn)?;
        let is_new_service = !device.advertisement_headers.contains_key(notification_type)
            && !device.search_headers.contains_key(notification_type);
        let changed = source == SsdpSource::AdvertisementUpdate
            || is_new_device
            || is_new_service
            || new_location
            || device
                .advertisement_headers
                .get(notification_type)
                .is_some_and(|old| same_headers_differ(old, headers));

        let device = self.devices.get_mut(&device.udn)?;
        device
            .advertisement_headers
            .insert(notification_type.to_owned(), headers.clone());
        changed.then(|| TrackerEvent {
            source,
            device: device.clone(),
            dst: notification_type.to_owned(),
        })
    }

    fn observe_byebye(&mut self, message: &SsdpMessage) -> Option<TrackerEvent> {
        let headers = &message.headers;
        let notification_type = non_empty_header(headers, "nt")?;
        if !message.udn.as_deref().is_some_and(|udn| !udn.is_empty()) {
            return None;
        }
        let udn = udn_from_usn(headers.get("usn")?)?;
        let mut device = self.devices.shift_remove(&udn)?;
        device
            .advertisement_headers
            .insert(notification_type.to_owned(), headers.clone());
        Some(TrackerEvent {
            source: SsdpSource::AdvertisementByebye,
            device,
            dst: notification_type.to_owned(),
        })
    }

    fn see_device(
        &mut self,
        message: &SsdpMessage,
        now: DateTime<Utc>,
        udn: String,
    ) -> Option<(DeviceSnapshot, bool)> {
        let location = message.location.as_ref()?;
        let usn = message.headers.get("usn")?;
        let parsed_udn = udn_from_usn(usn)?;
        if parsed_udn != udn {
            return None;
        }
        self.purge(now);

        let valid_to = extract_valid_to(&message.headers, message.received_at);
        let device = self
            .devices
            .entry(udn.clone())
            .or_insert_with(|| DeviceSnapshot::new(udn, valid_to));
        device.valid_to = valid_to;
        let new_location = location_changed(device, location, message.original_location.as_deref());
        device.locations.insert(location.clone(), valid_to);
        device.last_seen = Some(message.received_at);
        Some((device.clone(), new_location))
    }
}

fn non_empty_header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.get(name).filter(|value| !value.is_empty())
}

fn extract_valid_to(headers: &Headers, timestamp: DateTime<Utc>) -> DateTime<Utc> {
    static MAX_AGE: OnceLock<Regex> = OnceLock::new();
    let max_age_regex = MAX_AGE.get_or_init(|| {
        Regex::new(r"(?i)max-age\s*=\s*(\d+)").expect("max-age expression is valid")
    });
    let seconds = headers
        .get("cache-control")
        .and_then(|value| max_age_regex.captures(value))
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<u64>().ok())
        .map_or(DEFAULT_MAX_AGE_SECONDS, |value| {
            value.min(1_000_000_000_000) as i64
        });
    timestamp
        .checked_add_signed(Duration::seconds(seconds))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

fn same_headers_differ(old: &Headers, new: &Headers) -> bool {
    old.normalized().iter().any(|(name, old_value)| {
        if name.starts_with('_') || IGNORED_CHANGE_HEADERS.contains(&name.as_str()) {
            return false;
        }
        new.normalized()
            .get(name)
            .is_some_and(|new_value| new_value != old_value)
    })
}

fn location_changed(device: &DeviceSnapshot, new_location: &str, original: Option<&str>) -> bool {
    if device.locations.is_empty() {
        return true;
    }
    if device.locations.contains_key(new_location) {
        return false;
    }
    let Some(new_ip_version) = ip_version(original.unwrap_or(new_location)) else {
        return false;
    };
    device.locations.keys().any(|old_location| {
        ip_version(old_location).is_some_and(|old_ip_version| old_ip_version == new_ip_version)
    })
}

fn ip_version(location: &str) -> Option<u8> {
    let parsed = Url::parse(location).ok()?;
    match parsed.host()? {
        Host::Ipv4(_) => Some(4),
        Host::Ipv6(_) => Some(6),
        Host::Domain(hostname) => match parse_ip_literal(hostname) {
            Some(IpAddr::V4(_)) => Some(4),
            Some(IpAddr::V6(_)) => Some(6),
            None => None,
        },
    }
}

fn is_valid_location(location: &str) -> bool {
    if !location.starts_with("http") {
        return false;
    }
    let Ok(parsed) = Url::parse(location) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let hostname = host.trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty()
        || hostname == "localhost"
        || hostname == "localhost.localdomain"
        || hostname.ends_with(".localhost")
    {
        return false;
    }
    let Some(address) = parse_ip_literal(&hostname) else {
        return true;
    };
    match address {
        IpAddr::V4(ip) => !(ip.is_loopback() || ip.is_unspecified() || ip.is_link_local()),
        IpAddr::V6(ip) => {
            let address = ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4);
            match address {
                IpAddr::V4(ip) => !(ip.is_loopback() || ip.is_unspecified() || ip.is_link_local()),
                IpAddr::V6(ip) => !(ip.is_loopback() || ip.is_unspecified()),
            }
        }
    }
}

fn parse_ip_literal(hostname: &str) -> Option<IpAddr> {
    if let Ok(address) = IpAddr::from_str(hostname) {
        return Some(address);
    }
    parse_ipv4_compat(hostname).map(IpAddr::V4)
}

fn parse_ipv4_compat(hostname: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<_> = hostname.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let values: Vec<u64> = parts
        .iter()
        .map(|part| parse_legacy_number(part))
        .collect::<Option<_>>()?;
    let address = match values.as_slice() {
        [a] if *a <= u32::MAX as u64 => *a as u32,
        [a, b] if *a <= u8::MAX as u64 && *b <= 0x00ff_ffff => ((*a << 24) | *b) as u32,
        [a, b, c] if *a <= u8::MAX as u64 && *b <= u8::MAX as u64 && *c <= u16::MAX as u64 => {
            ((*a << 24) | (*b << 16) | *c) as u32
        }
        [a, b, c, d] if [a, b, c, d].iter().all(|part| **part <= u8::MAX as u64) => {
            ((*a << 24) | (*b << 16) | (*c << 8) | *d) as u32
        }
        _ => return None,
    };
    Some(std::net::Ipv4Addr::from(address))
}

fn parse_legacy_number(value: &str) -> Option<u64> {
    if value.is_empty() {
        return None;
    }
    let (digits, radix) = if value.len() > 2 && value[..2].eq_ignore_ascii_case("0x") {
        (&value[2..], 16)
    } else if value.len() > 1 && value.starts_with('0') {
        (&value[1..], 8)
    } else {
        (value, 10)
    };
    if digits.is_empty() {
        Some(0)
    } else {
        u64::from_str_radix(digits, radix).ok()
    }
}

fn udn_from_usn(usn: &str) -> Option<String> {
    crate::udn_from_usn(usn)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    const USN: &str = "uuid:Device-1::urn:schemas-upnp-org:device:Basic:1";
    const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:Basic:1";
    const REMOTE: &str = "192.168.1.10:1900";

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("valid timestamp")
    }

    fn decode(line: &str, fields: &[(&str, &str)], received_at: DateTime<Utc>) -> SsdpMessage {
        let mut packet = format!("{line}\r\n");
        for (name, value) in fields {
            packet.push_str(name);
            packet.push(':');
            packet.push_str(value);
            packet.push_str("\r\n");
        }
        packet.push_str("\r\n");
        crate::decode_ssdp_packet(
            packet.as_bytes(),
            None,
            REMOTE.parse().expect("valid remote"),
            received_at,
        )
        .expect("valid fixture")
    }

    fn search(
        received_at: DateTime<Utc>,
        location: &str,
        cache_control: Option<&str>,
        extra: &[(&str, &str)],
    ) -> SsdpMessage {
        let mut fields = vec![("USN", USN), ("ST", DEVICE_TYPE), ("LOCATION", location)];
        if let Some(cache_control) = cache_control {
            fields.push(("CACHE-CONTROL", cache_control));
        }
        fields.extend_from_slice(extra);
        decode("HTTP/1.1 200 OK", &fields, received_at)
    }

    fn notification(
        received_at: DateTime<Utc>,
        subtype: &str,
        location: Option<&str>,
        cache_control: Option<&str>,
        extra: &[(&str, &str)],
    ) -> SsdpMessage {
        let mut fields = vec![("USN", USN), ("NT", DEVICE_TYPE), ("NTS", subtype)];
        if let Some(location) = location {
            fields.push(("LOCATION", location));
        }
        if let Some(cache_control) = cache_control {
            fields.push(("CACHE-CONTROL", cache_control));
        }
        fields.extend_from_slice(extra);
        decode("NOTIFY * HTTP/1.1", &fields, received_at)
    }

    #[test]
    fn search_source_changes_only_for_new_device_type_location_or_relevant_header() {
        let mut tracker = Tracker::new();
        let now = at(1_700_000_000);
        let first = search(
            now,
            "http://192.168.1.10/device.xml",
            Some("max-age=30"),
            &[
                ("X-Device", "one"),
                ("SERVER", "old"),
                ("DATE", "old"),
                ("HOST", "old-host"),
                ("_internal", "old-meta"),
            ],
        );
        let event = tracker
            .observation(&first, now)
            .expect("new device propagates");
        assert_eq!(event.source, SsdpSource::SearchChanged);
        assert_eq!(event.dst, DEVICE_TYPE);

        let same = search(
            at(1_700_000_001),
            "http://192.168.1.10/device.xml",
            Some("MAX-AGE = 45"),
            &[
                ("X-Device", "one"),
                ("SERVER", "new"),
                ("DATE", "new"),
                ("HOST", "new-host"),
                ("_internal", "new-meta"),
            ],
        );
        let mut same = same;
        same.headers.metadata_mut().host = Some("192.168.1.99".to_owned());
        same.headers
            .metadata_mut()
            .extra
            .insert("_source".to_owned(), json!("different-interface"));
        assert_eq!(
            tracker
                .observation(&same, at(1_700_000_001))
                .unwrap()
                .source,
            SsdpSource::SearchAlive
        );

        let changed = search(
            at(1_700_000_002),
            "http://192.168.1.10/device.xml",
            Some("max-age=45"),
            &[("X-Device", "two"), ("SERVER", "new"), ("DATE", "new")],
        );
        assert_eq!(
            tracker
                .observation(&changed, at(1_700_000_002))
                .unwrap()
                .source,
            SsdpSource::SearchChanged
        );
        assert_eq!(SsdpSource::SearchChanged.category(), SsdpSource::Search);
    }

    #[test]
    fn advertisement_alive_update_and_byebye_have_distinct_transitions() {
        let mut tracker = Tracker::new();
        let now = at(1_700_000_100);
        let alive = notification(
            now,
            "ssdp:alive",
            Some("http://192.168.1.10/device.xml"),
            Some("max-age=60"),
            &[("X-Device", "one")],
        );
        assert_eq!(
            tracker.observation(&alive, now).unwrap().source,
            SsdpSource::AdvertisementAlive
        );
        assert!(
            tracker
                .observation(
                    &notification(
                        at(1_700_000_101),
                        "ssdp:alive",
                        Some("http://192.168.1.10/device.xml"),
                        Some("max-age=60"),
                        &[("X-Device", "one")],
                    ),
                    at(1_700_000_101)
                )
                .is_none()
        );

        let update = notification(
            at(1_700_000_102),
            "ssdp:update",
            Some("http://192.168.1.10/device.xml"),
            Some("max-age=60"),
            &[("X-Device", "one")],
        );
        assert_eq!(
            tracker
                .observation(&update, at(1_700_000_102))
                .unwrap()
                .source,
            SsdpSource::AdvertisementUpdate
        );

        let bye = notification(at(1_700_000_103), "ssdp:byebye", None, None, &[]);
        let event = tracker
            .observation(&bye, at(1_700_000_103))
            .expect("known bye propagates");
        assert_eq!(event.source, SsdpSource::AdvertisementByebye);
        assert_eq!(event.dst, DEVICE_TYPE);
        assert!(event.device.advertisement_headers.contains_key(DEVICE_TYPE));
        assert!(tracker.get("uuid:Device-1").is_none());
        assert!(tracker.observation(&bye, at(1_700_000_104)).is_none());
    }

    #[test]
    fn cache_control_default_zero_and_location_expiry_use_injected_time() {
        let base = at(1_700_000_200);
        for cache_control in [None, Some("max-age=invalid")] {
            let mut tracker = Tracker::new();
            let event = tracker
                .observation(
                    &search(base, "http://192.168.1.10/device.xml", cache_control, &[]),
                    base,
                )
                .expect("valid search");
            assert_eq!(event.device.valid_to, at(1_700_001_100));
        }

        let mut tracker = Tracker::new();
        let first = search(base, "http://192.168.1.10/old.xml", Some("max-age=1"), &[]);
        tracker.observation(&first, base).expect("first search");
        let refresh_time = at(1_700_000_201);
        let second = search(
            refresh_time,
            "http://192.168.1.20/new.xml",
            Some("max-age=100"),
            &[],
        );
        let refreshed = tracker
            .observation(&second, refresh_time)
            .expect("same-family location refresh");
        assert_eq!(refreshed.device.valid_to, at(1_700_000_301));
        assert_eq!(refreshed.source, SsdpSource::SearchChanged);

        tracker.purge(at(1_700_000_202));
        let device = tracker.get("uuid:Device-1").expect("device still valid");
        assert!(!device.locations.contains_key("http://192.168.1.10/old.xml"));
        assert!(device.locations.contains_key("http://192.168.1.20/new.xml"));

        let zero_time = at(1_700_000_400);
        let mut zero_ttl = Tracker::new();
        let zero = search(
            zero_time,
            "http://192.168.1.10/zero.xml",
            Some("max-age=0"),
            &[],
        );
        zero_ttl
            .observation(&zero, zero_time)
            .expect("zero TTL is accepted");
        zero_ttl.purge(zero_time);
        assert!(zero_ttl.get("uuid:Device-1").is_some());
        zero_ttl.purge(at(1_700_000_401));
        assert!(zero_ttl.get("uuid:Device-1").is_none());
    }

    #[test]
    fn alternate_address_family_is_tracked_without_marking_location_changed() {
        let mut tracker = Tracker::new();
        let first_time = at(1_700_000_500);
        let first = search(
            first_time,
            "http://192.168.1.10/device.xml",
            Some("max-age=300"),
            &[],
        );
        tracker
            .observation(&first, first_time)
            .expect("first search");

        let second_time = at(1_700_000_501);
        let second = search(
            second_time,
            "http://[fe80::1]/device.xml",
            Some("max-age=300"),
            &[],
        );
        let event = tracker
            .observation(&second, second_time)
            .expect("search always yields alive or changed");
        assert_eq!(event.source, SsdpSource::SearchAlive);
        assert_eq!(event.device.locations.len(), 2);
        assert_eq!(
            event.device.current_location(),
            Some("http://192.168.1.10/device.xml")
        );
    }

    #[test]
    fn combined_headers_prefer_advertisement_and_by_type_finds_devices() {
        let now = at(1_700_000_600);
        let mut tracker = Tracker::new();
        let mut search_message = search(
            now,
            "http://192.168.1.10/device.xml",
            Some("max-age=60"),
            &[("X-Origin", "search")],
        );
        search_message
            .headers
            .metadata_mut()
            .extra
            .insert("_source".to_owned(), json!("search"));
        tracker
            .observation(&search_message, now)
            .expect("search is tracked");

        let mut alive = notification(
            at(1_700_000_601),
            "ssdp:update",
            Some("http://192.168.1.10/device.xml"),
            Some("max-age=60"),
            &[("X-Origin", "advertisement")],
        );
        alive
            .headers
            .metadata_mut()
            .extra
            .insert("_source".to_owned(), json!("advertisement"));
        let event = tracker
            .observation(&alive, at(1_700_000_601))
            .expect("update advertisement propagates");

        assert_eq!(tracker.by_type(DEVICE_TYPE).len(), 1);
        let combined = event.device.combined_headers(DEVICE_TYPE);
        assert_eq!(combined.get("x-origin"), Some("advertisement"));
        assert!(!combined.metadata_json().contains_key("_source"));
        assert_eq!(event.device.all_combined_headers().len(), 1);
    }

    #[test]
    fn device_and_service_iteration_follow_first_seen_order() {
        let mut tracker = Tracker::new();
        for (udn, dst) in [
            ("uuid:z", "urn:z"),
            ("uuid:a", "urn:a"),
            ("uuid:z", "urn:a"),
            ("uuid:b", "urn:b"),
        ] {
            let usn = format!("{udn}::{dst}");
            let message = decode(
                "HTTP/1.1 200 OK",
                &[
                    ("USN", &usn),
                    ("ST", dst),
                    ("LOCATION", "http://192.0.2.1/device.xml"),
                ],
                at(0),
            );
            tracker.observation(&message, at(0)).unwrap();
        }
        assert_eq!(
            tracker
                .devices()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["uuid:z", "uuid:a", "uuid:b"]
        );
        assert_eq!(
            tracker
                .get("uuid:z")
                .unwrap()
                .all_combined_headers()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["urn:z", "urn:a"]
        );
        let bye = decode(
            "NOTIFY * HTTP/1.1",
            &[
                ("USN", "uuid:z::urn:z"),
                ("NT", "urn:z"),
                ("NTS", "ssdp:byebye"),
            ],
            at(1),
        );
        tracker.observation(&bye, at(1)).unwrap();
        assert_eq!(
            tracker
                .devices()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["uuid:a", "uuid:b"]
        );
    }

    #[test]
    fn invalid_loopback_location_is_not_accepted_for_search() {
        let mut tracker = Tracker::new();
        let message = search(
            at(1_700_000_700),
            "http://127.0.0.1/device.xml",
            Some("max-age=60"),
            &[],
        );
        assert!(tracker.observation(&message, at(1_700_000_700)).is_none());
        assert!(tracker.devices().is_empty());
    }
}
