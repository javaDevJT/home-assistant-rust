use std::collections::BTreeMap;
use std::net::SocketAddr;

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use serde_json::{Map, Value, json};

/// Internal packet metadata kept separately from protocol header strings.
///
/// `metadata_json` exposes the same values with the underscore-prefixed keys
/// used by the reference implementation's case-insensitive header mapping.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InternalMetadata {
    /// Time supplied by the caller when the datagram was received.
    pub timestamp: Option<DateTime<Utc>>,
    /// Remote host text, including an IPv6 scope identifier when present.
    pub host: Option<String>,
    /// Remote UDP port.
    pub port: Option<u16>,
    /// Remote datagram address.
    pub remote_addr: Option<SocketAddr>,
    /// Local datagram address, if the caller knows it.
    pub local_addr: Option<SocketAddr>,
    /// UDN parsed from the `USN` field.
    pub udn: Option<String>,
    /// Original, unadjusted `LOCATION` field value.
    pub location_original: Option<String>,
    /// Additional internal JSON metadata supplied by an adapter.
    pub extra: BTreeMap<String, Value>,
}

impl InternalMetadata {
    /// Return a JSON object using the reference package's internal key names.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut values = Map::new();
        for (name, value) in &self.extra {
            values.insert(name.to_ascii_lowercase(), value.clone());
        }
        if let Some(timestamp) = self.timestamp {
            // async-upnp-client records datetime.now(), a naive local time.
            // Preserve that public shape while keeping a UTC instant internally.
            values.insert(
                "_timestamp".into(),
                json!(format_naive_timestamp(
                    timestamp.with_timezone(&Local).naive_local()
                )),
            );
        }
        if let Some(host) = &self.host {
            values.insert("_host".into(), json!(host));
        }
        if let Some(port) = self.port {
            values.insert("_port".into(), json!(port));
        }
        if let Some(remote_addr) = self.remote_addr {
            values.insert("_remote_addr".into(), address_json(remote_addr));
        }
        if let Some(local_addr) = self.local_addr {
            values.insert("_local_addr".into(), address_json(local_addr));
        } else if self.remote_addr.is_some() {
            values.insert("_local_addr".into(), Value::Null);
        }
        if let Some(udn) = &self.udn {
            values.insert("_udn".into(), json!(udn));
        }
        if let Some(location) = &self.location_original {
            values.insert("_location_original".into(), json!(location));
        }
        values
    }

    pub(crate) fn merge(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        if other.timestamp.is_some() {
            merged.timestamp = other.timestamp;
        }
        if other.host.is_some() {
            merged.host.clone_from(&other.host);
        }
        if other.port.is_some() {
            merged.port = other.port;
        }
        if other.remote_addr.is_some() {
            merged.remote_addr = other.remote_addr;
        }
        if other.local_addr.is_some() {
            merged.local_addr = other.local_addr;
        }
        if other.udn.is_some() {
            merged.udn.clone_from(&other.udn);
        }
        if other.location_original.is_some() {
            merged
                .location_original
                .clone_from(&other.location_original);
        }
        merged.extra.extend(other.extra.clone());
        merged
    }
}

fn format_naive_timestamp(timestamp: NaiveDateTime) -> String {
    if timestamp.and_utc().timestamp_subsec_micros() == 0 {
        timestamp.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        timestamp.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

fn address_json(address: SocketAddr) -> Value {
    match address {
        SocketAddr::V4(address) => json!([address.ip().to_string(), address.port()]),
        SocketAddr::V6(address) => json!([
            address.ip().to_string(),
            address.port(),
            address.flowinfo(),
            address.scope_id()
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_timestamp_uses_naive_local_python_microsecond_shape() {
        for (source, expected) in [
            ("2026-11-07T12:34:56", "2026-11-07T12:34:56"),
            ("2026-11-07T12:34:56.000123", "2026-11-07T12:34:56.000123"),
            (
                "2026-11-07T12:34:56.123456789",
                "2026-11-07T12:34:56.123456",
            ),
        ] {
            let value = NaiveDateTime::parse_from_str(source, "%Y-%m-%dT%H:%M:%S%.f").unwrap();
            assert_eq!(format_naive_timestamp(value), expected);
        }
        let metadata = InternalMetadata {
            timestamp: Some(Utc::now()),
            ..Default::default()
        };
        let text = metadata.to_json()["_timestamp"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(DateTime::parse_from_rfc3339(&text).is_err());
        assert!(NaiveDateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M:%S%.f").is_ok());
    }
}

/// Case-insensitive SSDP header collection with original field-name spelling.
///
/// Header keys are stored in lowercase; repeated keys replace the previous
/// value. Internal packet metadata remains typed and is available through
/// [`Headers::metadata`] or [`Headers::metadata_json`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Headers {
    values: BTreeMap<String, String>,
    original_names: BTreeMap<String, String>,
    metadata: InternalMetadata,
}

impl Headers {
    /// Create an empty header collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a header without regard to its case.
    pub fn insert(&mut self, name: impl AsRef<str>, value: impl Into<String>) -> Option<String> {
        let name = name.as_ref();
        let key = name.to_ascii_lowercase();
        self.original_names.insert(key.clone(), name.to_owned());
        self.values.insert(key, value.into())
    }

    /// Get a header value using a case-insensitive name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Remove a header using a case-insensitive name.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        self.original_names.remove(&key);
        self.values.remove(&key)
    }

    /// Return the normalized lowercase-key view of protocol headers.
    pub fn normalized(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    /// Iterate over protocol headers in normalized-key order while preserving
    /// the spelling used for each field name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(key, value)| {
            let name = self
                .original_names
                .get(key)
                .map_or(key.as_str(), String::as_str);
            (name, value.as_str())
        })
    }

    /// Number of protocol headers.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no protocol headers.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Return typed packet metadata.
    pub fn metadata(&self) -> &InternalMetadata {
        &self.metadata
    }

    /// Return mutable typed packet metadata.
    pub fn metadata_mut(&mut self) -> &mut InternalMetadata {
        &mut self.metadata
    }

    /// Return internal metadata as an underscore-keyed JSON object.
    pub fn metadata_json(&self) -> Map<String, Value> {
        self.metadata.to_json()
    }

    /// Get an internal metadata item as JSON using a case-insensitive key.
    pub fn get_internal(&self, name: &str) -> Option<Value> {
        self.metadata
            .to_json()
            .get(&name.to_ascii_lowercase())
            .cloned()
    }

    /// Serialize protocol headers with their original spelling and append the
    /// typed internal metadata fields.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut values = self
            .iter()
            .map(|(name, value)| (name.to_owned(), json!(value)))
            .collect::<Map<_, _>>();
        values.extend(self.metadata.to_json());
        values
    }

    pub(crate) fn merge(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        for (key, value) in &other.values {
            let original_name = other
                .original_names
                .get(key)
                .map_or(key.as_str(), String::as_str);
            merged.insert(original_name, value.clone());
        }
        merged.metadata = self.metadata.merge(&other.metadata);
        merged
    }
}
