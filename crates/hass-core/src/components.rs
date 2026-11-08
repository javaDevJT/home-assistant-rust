//! Process-lifetime receipts for successfully set up components.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

/// Entity platform domains recognized by Home Assistant's `BASE_PLATFORMS`.
///
/// Kept aligned with `homeassistant/generated/entity_platforms.py` at the
/// pinned source revision. For dotted names, the text after the first dot is
/// compared as a whole, matching Python's `str.partition` behavior.
const BASE_PLATFORMS: &[&str] = &[
    "ai_task",
    "air_quality",
    "alarm_control_panel",
    "assist_satellite",
    "binary_sensor",
    "button",
    "calendar",
    "camera",
    "climate",
    "conversation",
    "cover",
    "date",
    "datetime",
    "device_tracker",
    "event",
    "fan",
    "geo_location",
    "humidifier",
    "image",
    "image_processing",
    "infrared",
    "lawn_mower",
    "light",
    "lock",
    "media_player",
    "notify",
    "number",
    "radio_frequency",
    "remote",
    "scene",
    "select",
    "sensor",
    "siren",
    "stt",
    "switch",
    "text",
    "time",
    "todo",
    "tts",
    "update",
    "vacuum",
    "valve",
    "wake_word",
    "water_heater",
    "weather",
];

#[derive(Default)]
struct ComponentNames {
    /// All explicitly received component names, including dotted platforms.
    components: BTreeSet<String>,
    /// Top-level components and recognized integration platform names.
    all_components: BTreeSet<String>,
}

/// Thread-safe, cloneable view of this runtime's loaded-component receipts.
///
/// A receipt is process-lifetime state: names are only added, and snapshots
/// remain after a provider unloads or its current config entries change.
#[derive(Clone, Default)]
pub struct LoadedComponents {
    names: Arc<RwLock<ComponentNames>>,
}

impl LoadedComponents {
    /// Record a component name after its setup or module load succeeded.
    ///
    /// The caller supplies the canonical name explicitly; this method does
    /// not infer receipts from services, config entries, or other runtime
    /// state. Names follow Home Assistant's component convention, including
    /// dotted `integration.platform` names for loaded platforms.
    pub fn record_successful_component(&self, name: impl Into<String>) {
        let name = name.into();
        let mut names = self
            .names
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some((integration, platform)) = name.split_once('.') {
            if BASE_PLATFORMS.contains(&platform) {
                names.all_components.insert(integration.to_owned());
            }
        } else {
            names.all_components.insert(name.clone());
        }

        names.components.insert(name);
    }

    /// Return all received names, sorted lexicographically.
    ///
    /// This includes both top-level component names and dotted
    /// `integration.platform` names.
    pub fn components(&self) -> Vec<String> {
        self.names
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .components
            .iter()
            .cloned()
            .collect()
    }

    /// Return all top-level names, sorted lexicographically.
    ///
    /// A bare name is included directly. For a dotted name, the integration
    /// prefix is included only when the suffix is an exact Home Assistant
    /// base-platform domain.
    pub fn all_components(&self) -> Vec<String> {
        self.names
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .all_components
            .iter()
            .cloned()
            .collect()
    }
}
