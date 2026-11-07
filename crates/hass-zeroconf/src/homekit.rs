//! Pure HomeKit-specific Zeroconf classification.
//!
//! This module does not create config flows or inspect runtime state. It
//! translates one already-decoded service record into the HomeKit and ordinary
//! Zeroconf flow decisions that the caller can apply.

use crate::matcher::python_fnmatchcase;
use crate::models::ZeroconfServiceInfo;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// DNS-SD service types that enter Home Assistant's special HomeKit path.
pub const HOMEKIT_SERVICE_TYPES: [&str; 2] = ["_hap._tcp.local.", "_hap._udp.local."];

const MODEL_LOWER_KEY: &str = "md";
const MODEL_UPPER_KEY: &str = "MD";
const PAIRED_STATUS_KEY: &str = "sf";

/// The exact `source` value used when Home Assistant creates a config flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowSource {
    /// A flow created from a matched HomeKit model entry.
    HomeKit,
    /// A flow created by the ordinary Zeroconf matcher path.
    Zeroconf,
}

impl FlowSource {
    /// Returns the source string stored in Home Assistant's flow context.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HomeKit => "homekit",
            Self::Zeroconf => "zeroconf",
        }
    }
}

/// The HomeKit integration flow that should be created for a matching model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HomeKitFlowDecision {
    /// Integration domain selected by the HomeKit model registry.
    pub target_domain: String,
    /// Home Assistant flow source (`homekit`).
    pub source: FlowSource,
    /// Model value selected from the exact `md`/`MD` TXT property keys.
    pub model: String,
    /// Exact, prefix, or wildcard registry key that matched this model.
    pub matched_model_key: String,
    /// Whether this model also permits ordinary Zeroconf matching when unpaired.
    pub always_discover: bool,
}

/// Whether the caller should run ordinary Zeroconf matching after HomeKit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrdinaryFlowDecision {
    /// Continue with ordinary matching and this source context.
    Continue {
        /// Home Assistant flow source (`zeroconf`).
        source: FlowSource,
        /// Alternative integration domain set after a HomeKit match, if any.
        alternative_domain: Option<String>,
    },
    /// A HomeKit match was unpaired and its model does not always discover.
    Stop,
    /// Source `int(sf)` raised `TypeError` for a present valueless TXT key.
    ///
    /// The HomeKit flow has already been selected in [`HomeKitDecision`] when
    /// this occurs, matching the source's flow-before-pairing-check order.
    Abort(HomeKitClassificationError),
}

/// Typed result for the HomeKit-specific portion of one service update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HomeKitDecision {
    /// HomeKit flow to create, before ordinary matching, when a model matched.
    pub homekit_flow: Option<HomeKitFlowDecision>,
    /// Ordinary flow disposition and its source/alternative-domain context.
    pub ordinary_flow: OrdinaryFlowDecision,
    /// Whether `sf` parsed as integer zero. False when absent or invalid.
    pub paired: bool,
}

/// Source-compatible classification failure that occurs after HomeKit admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HomeKitClassificationError {
    /// `sf` exists as a valueless TXT property, so Python `int(None)` raises.
    NullPairedStatusFlag,
}

impl fmt::Display for HomeKitClassificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullPairedStatusFlag => {
                formatter.write_str("HomeKit TXT property 'sf' is present without a value")
            }
        }
    }
}

impl std::error::Error for HomeKitClassificationError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct HomeKitModel {
    domain: String,
    always_discover: bool,
}

/// Insertion-ordered HOMEKIT model metadata with exact and glob lookup paths.
#[derive(Clone, Debug, Default)]
pub struct HomeKitRegistry {
    exact_models: HashMap<String, HomeKitModel>,
    glob_models: Vec<(String, HomeKitModel)>,
}

impl HomeKitRegistry {
    /// Parses a generated-style model map while preserving glob source order.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let OrderedHomeKitModels(entries) = serde_json::from_str(json)?;
        let mut registry = Self::default();

        for (model, discovery) in entries {
            if model
                .chars()
                .any(|character| matches!(character, '*' | '?' | '['))
            {
                if let Some((_, existing)) = registry
                    .glob_models
                    .iter_mut()
                    .find(|(existing_model, _)| existing_model == &model)
                {
                    // Python dict assignment replaces the value without moving
                    // the original insertion slot.
                    *existing = discovery;
                } else {
                    registry.glob_models.push((model, discovery));
                }
            } else {
                registry.exact_models.insert(model, discovery);
            }
        }

        Ok(registry)
    }

    /// Returns the source-pinned generated HomeKit model registry.
    pub fn builtin() -> &'static Self {
        static BUILTIN: OnceLock<HomeKitRegistry> = OnceLock::new();
        BUILTIN.get_or_init(|| {
            Self::from_json(include_str!("homekit.json"))
                .expect("bundled HomeKit model metadata must be valid JSON")
        })
    }

    /// Classifies a service using this registry without side effects.
    pub fn classify(&self, info: &ZeroconfServiceInfo) -> HomeKitDecision {
        classify_with_registry(self, info)
    }

    fn find_model(&self, model: &str) -> Option<(String, &HomeKitModel)> {
        // Source order is significant: full exact, space-prefix exact,
        // hyphen-prefix exact, followed by wildcard entries in source order.
        let candidates = [
            model,
            model.split(' ').next().unwrap_or_default(),
            model.split('-').next().unwrap_or_default(),
        ];
        for candidate in candidates {
            if let Some(discovery) = self.exact_models.get(candidate) {
                return Some((candidate.to_owned(), discovery));
            }
        }

        self.glob_models
            .iter()
            .find(|(pattern, _)| python_fnmatchcase(model, pattern))
            .map(|(pattern, discovery)| (pattern.clone(), discovery))
    }
}

/// Classifies one service using the pinned built-in HomeKit registry.
pub fn classify_homekit(info: &ZeroconfServiceInfo) -> HomeKitDecision {
    HomeKitRegistry::builtin().classify(info)
}

fn classify_with_registry(
    registry: &HomeKitRegistry,
    info: &ZeroconfServiceInfo,
) -> HomeKitDecision {
    if !HOMEKIT_SERVICE_TYPES.contains(&info.service_type.as_str()) {
        return ordinary_only();
    }

    let model = info
        .properties
        .get(MODEL_LOWER_KEY)
        .filter(|model| !model.is_empty())
        .or_else(|| {
            info.properties
                .get(MODEL_UPPER_KEY)
                .filter(|model| !model.is_empty())
        });
    let Some(model) = model else {
        return ordinary_only();
    };
    let Some((matched_model_key, discovery)) = registry.find_model(model) else {
        return ordinary_only();
    };

    // The source admits this flow before checking `sf`. Keep it in the typed
    // decision even if the valueless-property TypeError aborts the next step.
    let homekit_flow = HomeKitFlowDecision {
        target_domain: discovery.domain.clone(),
        source: FlowSource::HomeKit,
        model: model.to_owned(),
        matched_model_key,
        always_discover: discovery.always_discover,
    };

    let paired = match info.properties.get_entry(PAIRED_STATUS_KEY) {
        None => false,
        Some(property) => match property.value.as_deref() {
            Some(value) => python_int_string_is_zero(value),
            None => {
                return HomeKitDecision {
                    homekit_flow: Some(homekit_flow),
                    ordinary_flow: OrdinaryFlowDecision::Abort(
                        HomeKitClassificationError::NullPairedStatusFlag,
                    ),
                    paired: false,
                };
            }
        },
    };

    let ordinary_flow = if paired || discovery.always_discover {
        OrdinaryFlowDecision::Continue {
            source: FlowSource::Zeroconf,
            alternative_domain: Some(discovery.domain.clone()),
        }
    } else {
        OrdinaryFlowDecision::Stop
    };

    HomeKitDecision {
        homekit_flow: Some(homekit_flow),
        ordinary_flow,
        paired,
    }
}

fn ordinary_only() -> HomeKitDecision {
    HomeKitDecision {
        homekit_flow: None,
        ordinary_flow: OrdinaryFlowDecision::Continue {
            source: FlowSource::Zeroconf,
            alternative_domain: None,
        },
        paired: false,
    }
}

/// Matches the boolean result of Python `int(value) == 0` for decimal TXT text.
/// Invalid text is a `ValueError` at the source and is treated as unpaired.
fn python_int_string_is_zero(value: &str) -> bool {
    let value = value.trim();
    let digits = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    if digits.is_empty() {
        return false;
    }

    let characters: Vec<char> = digits.chars().collect();
    let mut saw_digit = false;
    let mut all_zero = true;
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '_' {
            if index == 0
                || index + 1 == characters.len()
                || !characters[index - 1].is_ascii_digit()
                || !characters[index + 1].is_ascii_digit()
            {
                return false;
            }
        } else if character.is_ascii_digit() {
            saw_digit = true;
            all_zero &= character == '0';
        } else {
            return false;
        }
    }

    saw_digit && all_zero
}

struct OrderedHomeKitModels(Vec<(String, HomeKitModel)>);

impl<'de> Deserialize<'de> for OrderedHomeKitModels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedHomeKitModelsVisitor;

        impl<'de> Visitor<'de> for OrderedHomeKitModelsVisitor {
            type Value = OrderedHomeKitModels;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object of model names to HomeKit discoveries")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or_default());
                while let Some(entry) = map.next_entry::<String, HomeKitModel>()? {
                    entries.push(entry);
                }
                Ok(OrderedHomeKitModels(entries))
            }
        }

        deserializer.deserialize_map(OrderedHomeKitModelsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FlowSource, HomeKitClassificationError, HomeKitDecision, HomeKitRegistry,
        OrdinaryFlowDecision, classify_homekit,
    };
    use crate::models::{
        RawResolvedServiceInfo, RawTxtEntry, ScopedIpAddress, ZeroconfServiceInfo,
    };
    use std::net::{IpAddr, Ipv4Addr};

    fn info(service_type: &str, properties: &[(&str, Option<&str>)]) -> ZeroconfServiceInfo {
        let raw = RawResolvedServiceInfo {
            ip_addresses: vec![ScopedIpAddress::new(
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                0,
            )],
            port: Some(1234),
            hostname: Some("fixture.local.".to_owned()),
            service_type: service_type.to_owned(),
            name: "fixture".to_owned(),
            txt_entries: properties
                .iter()
                .map(|(key, value)| RawTxtEntry {
                    key: key.as_bytes().to_vec(),
                    value: value.map(|value| value.as_bytes().to_vec()),
                })
                .collect(),
        };
        ZeroconfServiceInfo::from_raw(raw).expect("fixture has a usable address")
    }

    fn flow(decision: &HomeKitDecision) -> &super::HomeKitFlowDecision {
        decision
            .homekit_flow
            .as_ref()
            .expect("expected HomeKit flow")
    }

    #[test]
    fn exact_and_prefix_model_lookup_preserve_source_precedence() {
        let exact = classify_homekit(&info("_hap._tcp.local.", &[("md", Some("BSB002"))]));
        assert_eq!(flow(&exact).target_domain, "hue");
        assert_eq!(flow(&exact).source, FlowSource::HomeKit);
        assert_eq!(flow(&exact).matched_model_key, "BSB002");

        let space_prefix =
            classify_homekit(&info("_hap._tcp.local.", &[("md", Some("BSB002 Bridge"))]));
        assert_eq!(flow(&space_prefix).matched_model_key, "BSB002");

        let hyphen_prefix =
            classify_homekit(&info("_hap._tcp.local.", &[("md", Some("BSB002-Bridge"))]));
        assert_eq!(flow(&hyphen_prefix).matched_model_key, "BSB002");

        let uppercase_fallback = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("")), ("MD", Some("BSB002"))],
        ));
        assert_eq!(flow(&uppercase_fallback).target_domain, "hue");

        let lowercase_precedence = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("unknown")), ("MD", Some("BSB002"))],
        ));
        assert!(lowercase_precedence.homekit_flow.is_none());
    }

    #[test]
    fn wildcard_lookup_is_case_sensitive_and_keeps_registry_order() {
        let wildcard =
            classify_homekit(&info("_hap._udp.local.", &[("md", Some("ecobee Premium"))]));
        assert_eq!(flow(&wildcard).target_domain, "ecobee");
        assert_eq!(flow(&wildcard).matched_model_key, "ecobee*");

        let wrong_case =
            classify_homekit(&info("_hap._tcp.local.", &[("md", Some("Ecobee Premium"))]));
        assert!(wrong_case.homekit_flow.is_none());

        let registry = HomeKitRegistry::from_json(
            r#"{"foo*":{"domain":"first","always_discover":false},"f*":{"domain":"second","always_discover":false}}"#,
        )
        .expect("valid ordered model registry");
        let ordered = registry.classify(&info("_hap._tcp.local.", &[("md", Some("foobar"))]));
        assert_eq!(flow(&ordered).target_domain, "first");

        let unrelated_type =
            classify_homekit(&info("_homekit._tcp.local.", &[("md", Some("BSB002"))]));
        assert!(unrelated_type.homekit_flow.is_none());
    }

    #[test]
    fn paired_and_always_discover_control_ordinary_flow_context() {
        let paired = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("BSB002")), ("sf", Some("0"))],
        ));
        assert!(paired.paired);
        assert_eq!(
            paired.ordinary_flow,
            OrdinaryFlowDecision::Continue {
                source: FlowSource::Zeroconf,
                alternative_domain: Some("hue".to_owned()),
            }
        );

        let unpaired = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("BSB002")), ("sf", Some("1"))],
        ));
        assert_eq!(unpaired.ordinary_flow, OrdinaryFlowDecision::Stop);

        let invalid_status = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("BSB002")), ("sf", Some("invalid"))],
        ));
        assert_eq!(invalid_status.ordinary_flow, OrdinaryFlowDecision::Stop);

        let always_discover = classify_homekit(&info("_hap._tcp.local.", &[("md", Some("AC02"))]));
        assert!(flow(&always_discover).always_discover);
        assert_eq!(
            always_discover.ordinary_flow,
            OrdinaryFlowDecision::Continue {
                source: FlowSource::Zeroconf,
                alternative_domain: Some("tado".to_owned()),
            }
        );
    }

    #[test]
    fn null_paired_status_preserves_homekit_admission() {
        let decision = classify_homekit(&info(
            "_hap._tcp.local.",
            &[("md", Some("BSB002")), ("sf", None)],
        ));

        assert_eq!(flow(&decision).target_domain, "hue");
        assert_eq!(
            decision.ordinary_flow,
            OrdinaryFlowDecision::Abort(HomeKitClassificationError::NullPairedStatusFlag)
        );
    }
}
