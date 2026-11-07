//! Home Assistant's public discovery-info shape, independent of transport.

use std::collections::BTreeSet;

use hass_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{DeviceSnapshot, Headers, SsdpSource};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SsdpServiceInfo {
    pub ssdp_usn: String,
    pub ssdp_st: String,
    pub upnp: Map<String, Value>,
    pub ssdp_location: Option<String>,
    pub ssdp_nt: Option<String>,
    pub ssdp_udn: Option<String>,
    pub ssdp_ext: Option<String>,
    pub ssdp_server: Option<String>,
    pub ssdp_headers: Map<String, Value>,
    pub ssdp_all_locations: BTreeSet<String>,
    pub x_homeassistant_matching_domains: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SsdpChange {
    Alive,
    Byebye,
    Update,
}

impl SsdpChange {
    pub fn from_source(source: SsdpSource) -> Option<Self> {
        match source {
            SsdpSource::SearchAlive
            | SsdpSource::SearchChanged
            | SsdpSource::AdvertisementAlive => Some(Self::Alive),
            SsdpSource::AdvertisementByebye => Some(Self::Byebye),
            SsdpSource::AdvertisementUpdate => Some(Self::Update),
            // Generic sources are used for rediscovery without callbacks.
            SsdpSource::Search | SsdpSource::Advertisement => None,
        }
    }
}

impl SsdpServiceInfo {
    pub fn from_headers_and_description(
        device: &DeviceSnapshot,
        headers: &Headers,
        description: &Map<String, Value>,
    ) -> Result<Self> {
        let usn = headers
            .get("usn")
            .ok_or_else(|| Error::InvalidInput("SSDP discovery has no USN".into()))?;
        let st = headers
            .get("st")
            .filter(|value| !value.is_empty())
            .or_else(|| headers.get("nt"))
            .ok_or_else(|| Error::InvalidInput("SSDP discovery has no ST or NT".into()))?;
        let mut upnp = description.clone();
        // The HA fallback is deliberately case-sensitive, unlike the protocol
        // decoder's UDN extraction. An existing UDN, including null, wins.
        if !upnp.contains_key("UDN") && usn.starts_with("uuid:") {
            upnp.insert("UDN".into(), json!(usn.split("::").next().unwrap_or(usn)));
        }
        Ok(Self {
            ssdp_usn: usn.into(),
            ssdp_st: st.into(),
            upnp,
            ssdp_location: headers.get("location").map(str::to_owned),
            ssdp_nt: headers.get("nt").map(str::to_owned),
            ssdp_udn: headers.metadata().udn.clone(),
            ssdp_ext: headers.get("ext").map(str::to_owned),
            ssdp_server: headers.get("server").map(str::to_owned),
            ssdp_headers: headers.to_json(),
            ssdp_all_locations: device.locations.keys().cloned().collect(),
            x_homeassistant_matching_domains: BTreeSet::new(),
        })
    }

    pub fn websocket_event(&self, change: SsdpChange) -> Result<Value> {
        if change == SsdpChange::Byebye {
            return Ok(
                json!({"remove":[{"ssdp_st":self.ssdp_st,"ssdp_location":self.ssdp_location}]}),
            );
        }
        let mut item = serde_json::to_value(self)
            .map_err(|error| Error::Service(format!("cannot serialize SSDP discovery: {error}")))?;
        item.as_object_mut()
            .expect("a discovery-info struct serializes to an object")
            .insert(
                "name".into(),
                self.upnp
                    .get("friendlyName")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        Ok(json!({"add":[item]}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn device() -> DeviceSnapshot {
        DeviceSnapshot {
            udn: "uuid:fixture".into(),
            valid_to: Utc::now(),
            locations: BTreeMap::from([("http://127.0.0.1/device.xml".into(), Utc::now())]),
            last_seen: None,
            search_headers: indexmap::IndexMap::new(),
            advertisement_headers: indexmap::IndexMap::new(),
        }
    }

    #[test]
    fn conversion_preserves_udn_fallback_case_null_and_empty_headers() {
        let mut headers = Headers::new();
        headers.insert("USN", "uuid:Mixed::type");
        headers.insert("ST", "");
        headers.insert("NT", "type");
        headers.insert("EXT", "");
        let info = SsdpServiceInfo::from_headers_and_description(&device(), &headers, &Map::new())
            .unwrap();
        assert_eq!(info.upnp["UDN"], "uuid:Mixed");
        assert_eq!(info.ssdp_st, "type");
        assert_eq!(info.ssdp_ext.as_deref(), Some(""));
        assert!(info.ssdp_udn.is_none());
        let info = SsdpServiceInfo::from_headers_and_description(
            &device(),
            &headers,
            &Map::from_iter([("UDN".into(), Value::Null)]),
        )
        .unwrap();
        assert!(info.upnp["UDN"].is_null());
        headers.insert("USN", "UUID:Mixed::type");
        let info = SsdpServiceInfo::from_headers_and_description(&device(), &headers, &Map::new())
            .unwrap();
        assert!(!info.upnp.contains_key("UDN"));
    }

    #[test]
    fn websocket_add_and_remove_use_source_shapes() {
        let mut headers = Headers::new();
        headers.insert("usn", "uuid:fixture::type");
        headers.insert("st", "type");
        headers.insert("location", "http://127.0.0.1/device.xml");
        let info = SsdpServiceInfo::from_headers_and_description(
            &device(),
            &headers,
            &Map::from_iter([("friendlyName".into(), json!("Fixture"))]),
        )
        .unwrap();
        let added = info.websocket_event(SsdpChange::Update).unwrap();
        assert_eq!(added["add"][0]["name"], "Fixture");
        assert_eq!(
            added["add"][0]["x_homeassistant_matching_domains"],
            json!([])
        );
        assert_eq!(
            info.websocket_event(SsdpChange::Byebye).unwrap(),
            json!({"remove":[{"ssdp_st":"type","ssdp_location":"http://127.0.0.1/device.xml"}]})
        );
    }
}
