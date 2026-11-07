//! Source-compatible service information for resolved DNS-SD records.

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::collections::HashSet;
use std::fmt;
use std::net::IpAddr;

/// An IP address with the interface scope retained for scoped IPv6 values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ScopedIpAddress {
    /// The address bytes and family.
    pub address: IpAddr,
    /// The numeric IPv6 zone/interface index, or zero when unscoped.
    pub scope_id: u32,
}

impl ScopedIpAddress {
    /// Creates an address from its family-specific address and IPv6 scope ID.
    pub const fn new(address: IpAddr, scope_id: u32) -> Self {
        Self { address, scope_id }
    }

    fn is_usable_as_host(&self) -> bool {
        match self.address {
            IpAddr::V4(address) => !address.is_link_local() && !address.is_unspecified(),
            IpAddr::V6(address) => !address.is_unicast_link_local() && !address.is_unspecified(),
        }
    }
}

impl fmt::Display for ScopedIpAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.address {
            IpAddr::V6(address) if self.scope_id != 0 => {
                write!(formatter, "{address}%{}", self.scope_id)
            }
            address => write!(formatter, "{address}"),
        }
    }
}

impl Serialize for ScopedIpAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// One decoded service property, retained in source iteration order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZeroconfProperty {
    /// ASCII-decoded key, with one replacement character for each non-ASCII byte.
    pub key: String,
    /// UTF-8-decoded value; `None` represents a valueless TXT key.
    pub value: Option<String>,
}

/// An insertion-ordered property mapping with exact, case-sensitive keys.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ZeroconfProperties {
    entries: Vec<ZeroconfProperty>,
}

impl ZeroconfProperties {
    /// Returns the entry for an exact key, including a valueless entry.
    pub fn get_entry(&self, key: &str) -> Option<&ZeroconfProperty> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// Returns the value for an exact key, or `None` for a missing or valueless key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.get_entry(key)?.value.as_deref()
    }

    /// Returns whether the exact key is present, even if it has no value.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get_entry(key).is_some()
    }

    /// Returns the number of distinct decoded property keys.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the property mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates entries in their original decoded insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &ZeroconfProperty> {
        self.entries.iter()
    }

    /// Decode TXT properties without requiring a usable service address. Raw
    /// update subscribers use this before discovery's address validation.
    pub fn from_txt_entries(entries: Vec<RawTxtEntry>) -> Self {
        let mut seen_raw_keys = HashSet::with_capacity(entries.len());
        let mut decoded = Self {
            entries: Vec::with_capacity(entries.len()),
        };

        for entry in entries {
            // zeroconf 0.152.0 keeps the first occurrence of each raw byte key.
            if !seen_raw_keys.insert(entry.key.clone()) {
                continue;
            }

            let key = decode_ascii_with_replacement(&entry.key);
            let value = entry
                .value
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());

            // Distinct raw byte keys can decode to the same replacement-filled
            // String. Python's decoded-properties dict comprehension keeps the
            // first insertion slot while the later value replaces the earlier one.
            if let Some(existing) = decoded.entries.iter_mut().find(|item| item.key == key) {
                existing.value = value;
            } else {
                decoded.entries.push(ZeroconfProperty { key, value });
            }
        }

        decoded
    }
}

impl Serialize for ZeroconfProperties {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for entry in &self.entries {
            map.serialize_entry(&entry.key, &entry.value)?;
        }
        map.end()
    }
}

fn decode_ascii_with_replacement(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii() {
                char::from(*byte)
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

/// Raw TXT property bytes supplied by the DNS-SD backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTxtEntry {
    /// The property key bytes, without the first `=` separator.
    pub key: Vec<u8>,
    /// The bytes after the first `=`, or `None` when the key had no separator.
    pub value: Option<Vec<u8>>,
}

/// Unconverted service fields from a completed DNS-SD resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawResolvedServiceInfo {
    /// Addresses already ordered like zeroconf `IPVersion.All`: IPv4 first,
    /// followed by IPv6, with most recently observed addresses first per family.
    pub ip_addresses: Vec<ScopedIpAddress>,
    /// SRV port; incomplete source data may leave it absent.
    pub port: Option<u16>,
    /// SRV target hostname; the source runtime can retain an absent value.
    pub hostname: Option<String>,
    /// Original DNS-SD service type, including its source spelling and suffix.
    pub service_type: String,
    /// Original service instance name, including its source spelling and suffix.
    pub name: String,
    /// Ordered raw TXT key/value entries.
    pub txt_entries: Vec<RawTxtEntry>,
}

/// A prepared, source-compatible service record passed to discovery consumers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ZeroconfServiceInfo {
    /// The first address that is neither link-local nor unspecified.
    pub ip_address: ScopedIpAddress,
    /// All resolved addresses in source `IPVersion.All` order.
    pub ip_addresses: Vec<ScopedIpAddress>,
    /// SRV port, if present.
    pub port: Option<u16>,
    /// Original SRV target hostname, or `None` when source data omitted it.
    pub hostname: Option<String>,
    /// Original DNS-SD service type.
    #[serde(rename = "type")]
    pub service_type: String,
    /// Original service instance name.
    pub name: String,
    /// Decoded TXT properties with exact case-sensitive keys.
    pub properties: ZeroconfProperties,
}

impl ZeroconfServiceInfo {
    /// Converts resolved backend data using Home Assistant's source rules.
    ///
    /// Returns `None` when there are no addresses or every address is
    /// link-local/unspecified. A missing hostname is retained as `None`: the
    /// pinned source's hostname assertion is guarded by `TYPE_CHECKING` only.
    pub fn from_raw(raw: RawResolvedServiceInfo) -> Option<Self> {
        let ip_address = raw
            .ip_addresses
            .iter()
            .copied()
            .find(ScopedIpAddress::is_usable_as_host)?;

        Some(Self {
            ip_address,
            ip_addresses: raw.ip_addresses,
            port: raw.port,
            hostname: raw.hostname,
            service_type: raw.service_type,
            name: raw.name,
            properties: ZeroconfProperties::from_txt_entries(raw.txt_entries),
        })
    }

    /// Returns the selected primary address as text, including an IPv6 zone.
    pub fn host(&self) -> String {
        self.ip_address.to_string()
    }

    /// Returns all source-ordered addresses as text, including IPv6 zones.
    pub fn addresses(&self) -> Vec<String> {
        self.ip_addresses.iter().map(ToString::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{RawResolvedServiceInfo, RawTxtEntry, ScopedIpAddress, ZeroconfServiceInfo};
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn ip(value: &str, scope_id: u32) -> ScopedIpAddress {
        ScopedIpAddress::new(value.parse().expect("valid test IP address"), scope_id)
    }

    fn raw(ip_addresses: Vec<ScopedIpAddress>) -> RawResolvedServiceInfo {
        RawResolvedServiceInfo {
            ip_addresses,
            port: Some(8123),
            hostname: Some("host.local.".to_owned()),
            service_type: "_sample._tcp.local.".to_owned(),
            name: "Kitchen._sample._tcp.local.".to_owned(),
            txt_entries: Vec::new(),
        }
    }

    #[test]
    fn selects_first_usable_address_without_reordering_source_vector() {
        let source_order = vec![
            ip("169.254.10.2", 0),
            ip("0.0.0.0", 0),
            ip("192.168.1.20", 0),
            ip("10.0.0.4", 0),
            ip("fd00::20", 0),
            ip("fd00::10", 0),
        ];
        let info = ZeroconfServiceInfo::from_raw(raw(source_order.clone()))
            .expect("at least one usable address");

        assert_eq!(info.ip_address, ip("192.168.1.20", 0));
        assert_eq!(info.ip_addresses, source_order);
        assert_eq!(info.host(), "192.168.1.20");
        assert_eq!(
            info.addresses(),
            [
                "169.254.10.2",
                "0.0.0.0",
                "192.168.1.20",
                "10.0.0.4",
                "fd00::20",
                "fd00::10"
            ]
        );
    }

    #[test]
    fn returns_none_for_missing_or_only_unusable_addresses() {
        let mut no_addresses = raw(Vec::new());
        no_addresses.hostname = None;
        assert_eq!(ZeroconfServiceInfo::from_raw(no_addresses), None);

        let mut unusable = raw(vec![
            ip("0.0.0.0", 0),
            ip("169.254.2.3", 0),
            ip("fe80::1", 4),
            ip("::", 0),
        ]);
        unusable.hostname = None;
        assert_eq!(ZeroconfServiceInfo::from_raw(unusable), None);
    }

    #[test]
    fn missing_hostname_is_preserved_when_a_usable_address_exists() {
        let mut raw = raw(vec![ip("192.168.1.2", 0)]);
        raw.hostname = None;
        let info = ZeroconfServiceInfo::from_raw(raw).expect("usable address");
        assert_eq!(info.hostname, None);
    }

    #[test]
    fn keeps_scoped_ipv6_addresses_and_numeric_zone_in_text() {
        let info = ZeroconfServiceInfo::from_raw(raw(vec![ip("fe80::1", 7), ip("2001:db8::5", 0)]))
            .expect("global IPv6 address is usable");

        assert_eq!(info.host(), "2001:db8::5");
        assert_eq!(info.addresses(), ["fe80::1%7", "2001:db8::5"]);
    }

    #[test]
    fn decodes_txt_keys_and_values_and_preserves_presence_semantics() {
        let mut source = raw(vec![ip("192.168.1.2", 0)]);
        source.txt_entries = vec![
            RawTxtEntry {
                key: b"Case".to_vec(),
                value: Some(b"first=second".to_vec()),
            },
            RawTxtEntry {
                key: b"flag".to_vec(),
                value: None,
            },
            RawTxtEntry {
                key: b"empty".to_vec(),
                value: Some(Vec::new()),
            },
            RawTxtEntry {
                key: b"Case".to_vec(),
                value: Some(b"duplicate-loses".to_vec()),
            },
            RawTxtEntry {
                key: vec![b'k', 0xff],
                value: Some(b"bad\xffvalue".to_vec()),
            },
        ];
        let info = ZeroconfServiceInfo::from_raw(source).expect("usable address");

        assert_eq!(info.properties.get("Case"), Some("first=second"));
        assert!(!info.properties.contains_key("case"));
        assert_eq!(info.properties.get_entry("flag").unwrap().value, None);
        assert_eq!(
            info.properties.get_entry("empty").unwrap().value.as_deref(),
            Some("")
        );
        assert_eq!(info.properties.get("k\u{fffd}"), Some("bad\u{fffd}value"));
        assert_eq!(
            info.properties
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["Case", "flag", "empty", "k\u{fffd}"]
        );
    }

    #[test]
    fn decoded_replacement_collisions_keep_first_slot_and_last_value() {
        let mut source = raw(vec![ip("192.168.1.2", 0)]);
        source.txt_entries = vec![
            RawTxtEntry {
                key: vec![0xfe],
                value: Some(b"first".to_vec()),
            },
            RawTxtEntry {
                key: b"stable".to_vec(),
                value: Some(b"middle".to_vec()),
            },
            RawTxtEntry {
                key: vec![0xff],
                value: Some(b"last".to_vec()),
            },
        ];
        let info = ZeroconfServiceInfo::from_raw(source).expect("usable address");

        assert_eq!(info.properties.get("\u{fffd}"), Some("last"));
        assert_eq!(
            info.properties
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["\u{fffd}", "stable"]
        );
    }

    #[test]
    fn serializes_source_field_names_and_address_and_property_strings() {
        let mut source = raw(vec![
            ScopedIpAddress::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 0),
            ScopedIpAddress::new(IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap()), 3),
        ]);
        source.txt_entries = vec![
            RawTxtEntry {
                key: b"flag".to_vec(),
                value: None,
            },
            RawTxtEntry {
                key: b"empty".to_vec(),
                value: Some(Vec::new()),
            },
        ];
        let info = ZeroconfServiceInfo::from_raw(source).expect("usable address");
        let serialized = serde_json::to_value(info).expect("serializes to JSON");

        assert_eq!(
            serialized,
            json!({
                "ip_address": "192.168.1.2",
                "ip_addresses": ["192.168.1.2", "fe80::1%3"],
                "port": 8123,
                "hostname": "host.local.",
                "type": "_sample._tcp.local.",
                "name": "Kitchen._sample._tcp.local.",
                "properties": {"flag": null, "empty": ""}
            })
        );
    }
}
