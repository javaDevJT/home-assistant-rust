//! The adapter's indexed exact-subset matchers and callback header filters.

use crate::Headers;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub type MatchDict = BTreeMap<String, String>;
pub type IntegrationMatchersConfig = BTreeMap<String, Vec<MatchDict>>;
/// Matcher metadata generated from the pinned Core source, independent of
/// whether a native driver for each matching integration is available yet.
pub fn pinned_integration_matchers() -> IntegrationMatchersConfig {
    serde_json::from_str(include_str!("matchers.json"))
        .expect("the bundled pinned SSDP matcher registry is valid")
}
const PRIMARY_KEYS: [&str; 5] = ["manufacturer", "st", "deviceType", "nt", "manufacturerURL"];

#[derive(Clone, Debug, Default)]
pub struct IntegrationMatchers {
    by_key: BTreeMap<String, BTreeMap<String, Vec<(String, MatchDict)>>>,
}

impl IntegrationMatchers {
    pub fn new(config: IntegrationMatchersConfig) -> Self {
        let mut by_key: BTreeMap<String, BTreeMap<String, Vec<(String, MatchDict)>>> =
            BTreeMap::new();
        for key in PRIMARY_KEYS {
            let index = by_key.entry(key.into()).or_default();
            for (domain, matchers) in &config {
                for matcher in matchers {
                    if let Some(value) = matcher.get(key).filter(|value| !value.is_empty()) {
                        index
                            .entry(value.clone())
                            .or_default()
                            .push((domain.clone(), matcher.clone()));
                    }
                }
            }
        }
        Self { by_key }
    }

    pub fn matching_domains(
        &self,
        headers: &Headers,
        description: &Map<String, Value>,
    ) -> BTreeSet<String> {
        let mut info: BTreeMap<String, Value> = headers
            .to_json()
            .into_iter()
            .map(|(key, value)| (key.to_lowercase(), value))
            .collect();
        for (key, value) in description {
            info.insert(key.to_lowercase(), value.clone());
        }
        let mut domains = BTreeSet::new();
        for (key, candidates) in &self.by_key {
            let Some(value) = info.get(&key.to_lowercase()).and_then(Value::as_str) else {
                continue;
            };
            let Some(matchers) = candidates.get(value) else {
                continue;
            };
            for (domain, matcher) in matchers {
                if matcher.iter().all(|(key, expected)| {
                    let key = key.to_lowercase();
                    if key == "_timestamp"
                        && headers.metadata().timestamp.is_some()
                        && !description
                            .keys()
                            .any(|key| key.eq_ignore_ascii_case("_timestamp"))
                    {
                        return false;
                    }
                    info.get(&key).and_then(Value::as_str) == Some(expected)
                }) {
                    domains.insert(domain.clone());
                }
            }
        }
        domains
    }
}

pub fn headers_match(headers: &Headers, matcher: &MatchDict) -> bool {
    let mut info: BTreeMap<String, Value> = headers
        .to_json()
        .into_iter()
        .map(|(key, value)| (key.to_lowercase(), value))
        .collect();
    // A source datetime is not equal to a string, even though JSON encodes it
    // as one. Presence matching still includes that internal field.
    if headers.metadata().timestamp.is_some() {
        info.insert("_timestamp".into(), Value::Null);
    }
    matcher.iter().all(|(key, expected)| {
        let key = key.to_lowercase();
        if expected == "*" {
            info.contains_key(&key)
        } else {
            info.get(&key).and_then(Value::as_str) == Some(expected)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bundled_registry_retains_the_pinned_domains_and_matcher_count() {
        let config = pinned_integration_matchers();
        assert_eq!(config.len(), 44);
        assert_eq!(config.values().map(Vec::len).sum::<usize>(), 94);
        assert_eq!(
            config["sonos"][0]["st"],
            "urn:schemas-upnp-org:device:ZonePlayer:1"
        );
        assert_eq!(config["victron_gx"][0]["X_MqttOnLan"], "1");
    }

    fn dict(entries: &[(&str, &str)]) -> MatchDict {
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn callback_wildcard_means_presence_and_values_are_exact() {
        let mut headers = Headers::new();
        headers.insert("SERVER", "Vendor");
        headers.metadata_mut().timestamp = Some(chrono::Utc::now());
        assert!(headers_match(
            &headers,
            &dict(&[("server", "Vendor"), ("_timestamp", "*")])
        ));
        assert!(!headers_match(&headers, &dict(&[("server", "vendor")])));
        assert!(!headers_match(&headers, &dict(&[("missing", "*")])));
        assert!(!headers_match(
            &headers,
            &dict(&[(
                "_timestamp",
                &headers.metadata().timestamp.unwrap().to_rfc3339()
            )])
        ));
    }

    #[test]
    fn integration_matchers_need_a_primary_and_exact_case_insensitive_key_subset() {
        let config = BTreeMap::from([
            (
                "match".into(),
                vec![dict(&[("st", "type"), ("MODELNAME", "Fixture")])],
            ),
            ("no_primary".into(), vec![dict(&[("modelName", "Fixture")])]),
            ("literal_star".into(), vec![dict(&[("st", "*")])]),
        ]);
        let matchers = IntegrationMatchers::new(config);
        let mut headers = Headers::new();
        headers.insert("ST", "type");
        assert_eq!(
            matchers.matching_domains(
                &headers,
                &Map::from_iter([("modelName".into(), json!("Fixture"))])
            ),
            BTreeSet::from(["match".into()])
        );
        assert!(
            matchers
                .matching_domains(
                    &headers,
                    &Map::from_iter([("modelName".into(), json!("fixture"))])
                )
                .is_empty()
        );
    }
}
