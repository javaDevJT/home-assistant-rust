//! Typed binary-sensor descriptions and snapshots for native providers.
//!
//! The projection follows Home Assistant Core revision
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`: `Some(true)` is `on`,
//! `Some(false)` is `off`, and `None` is `unknown`. Availability is a separate
//! flag and the shared [`hass_entity::EntityLifecycle`] publishes
//! `unavailable` when it is false.

use hass_core::Context;
use hass_entity::{EntitySnapshot, EntitySpec};
use serde_json::{Map, Value};
use std::fmt;
use std::str::FromStr;

/// Home Assistant's binary-sensor device classes at the pinned Core revision.
///
/// A class describes what `on` and `off` mean; it does not change those stored
/// state strings or dispatch device conditions and triggers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BinarySensorDeviceClass {
    /// `on` means low; `off` means normal.
    Battery,
    /// `on` means charging; `off` means not charging.
    BatteryCharging,
    /// `on` means carbon monoxide detected; `off` means clear.
    CarbonMonoxide,
    /// `on` means cold; `off` means normal.
    Cold,
    /// `on` means connected; `off` means disconnected.
    Connectivity,
    /// `on` means open; `off` means closed.
    Door,
    /// `on` means open; `off` means closed.
    GarageDoor,
    /// `on` means gas detected; `off` means clear.
    Gas,
    /// `on` means glass break detected; `off` means clear.
    GlassBreak,
    /// `on` means hot; `off` means normal.
    Heat,
    /// `on` means light detected; `off` means no light.
    Light,
    /// `on` means open (unlocked); `off` means closed (locked).
    Lock,
    /// `on` means wet; `off` means dry.
    Moisture,
    /// `on` means motion detected; `off` means clear.
    Motion,
    /// `on` means moving; `off` means stopped.
    Moving,
    /// `on` means occupied; `off` means clear.
    Occupancy,
    /// `on` means open; `off` means closed.
    Opening,
    /// `on` means plugged in; `off` means unplugged.
    Plug,
    /// `on` means power detected; `off` means no power.
    Power,
    /// `on` means home; `off` means away.
    Presence,
    /// `on` means a problem detected; `off` means OK.
    Problem,
    /// `on` means running; `off` means not running.
    Running,
    /// `on` means unsafe; `off` means safe.
    Safety,
    /// `on` means smoke detected; `off` means clear.
    Smoke,
    /// `on` means sound detected; `off` means no sound.
    Sound,
    /// `on` means tampering detected; `off` means clear.
    Tamper,
    /// `on` means an update is available; `off` means up to date.
    Update,
    /// `on` means vibration detected; `off` means no vibration.
    Vibration,
    /// `on` means open; `off` means closed.
    Window,
}

impl BinarySensorDeviceClass {
    /// All device classes, in source declaration order at the pinned revision.
    pub const ALL: &'static [Self] = &[
        Self::Battery,
        Self::BatteryCharging,
        Self::CarbonMonoxide,
        Self::Cold,
        Self::Connectivity,
        Self::Door,
        Self::GarageDoor,
        Self::Gas,
        Self::GlassBreak,
        Self::Heat,
        Self::Light,
        Self::Lock,
        Self::Moisture,
        Self::Motion,
        Self::Moving,
        Self::Occupancy,
        Self::Opening,
        Self::Plug,
        Self::Power,
        Self::Presence,
        Self::Problem,
        Self::Running,
        Self::Safety,
        Self::Smoke,
        Self::Sound,
        Self::Tamper,
        Self::Update,
        Self::Vibration,
        Self::Window,
    ];

    /// Return the canonical Home Assistant device-class string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Battery => "battery",
            Self::BatteryCharging => "battery_charging",
            Self::CarbonMonoxide => "carbon_monoxide",
            Self::Cold => "cold",
            Self::Connectivity => "connectivity",
            Self::Door => "door",
            Self::GarageDoor => "garage_door",
            Self::Gas => "gas",
            Self::GlassBreak => "glass_break",
            Self::Heat => "heat",
            Self::Light => "light",
            Self::Lock => "lock",
            Self::Moisture => "moisture",
            Self::Motion => "motion",
            Self::Moving => "moving",
            Self::Occupancy => "occupancy",
            Self::Opening => "opening",
            Self::Plug => "plug",
            Self::Power => "power",
            Self::Presence => "presence",
            Self::Problem => "problem",
            Self::Running => "running",
            Self::Safety => "safety",
            Self::Smoke => "smoke",
            Self::Sound => "sound",
            Self::Tamper => "tamper",
            Self::Update => "update",
            Self::Vibration => "vibration",
            Self::Window => "window",
        }
    }

    /// Return Core's static description of the meanings of `on` and `off`.
    #[must_use]
    pub const fn meaning(self) -> BinarySensorClassMeaning {
        match self {
            Self::Battery => BinarySensorClassMeaning::new("low", "normal"),
            Self::BatteryCharging => BinarySensorClassMeaning::new("charging", "not charging"),
            Self::CarbonMonoxide => BinarySensorClassMeaning::new(
                "carbon monoxide detected",
                "no carbon monoxide (clear)",
            ),
            Self::Cold => BinarySensorClassMeaning::new("cold", "normal"),
            Self::Connectivity => BinarySensorClassMeaning::new("connected", "disconnected"),
            Self::Door | Self::GarageDoor | Self::Opening | Self::Window => {
                BinarySensorClassMeaning::new("open", "closed")
            }
            Self::Gas => BinarySensorClassMeaning::new("gas detected", "no gas (clear)"),
            Self::GlassBreak => {
                BinarySensorClassMeaning::new("glass break detected", "no glass break (clear)")
            }
            Self::Heat => BinarySensorClassMeaning::new("hot", "normal"),
            Self::Light => BinarySensorClassMeaning::new("light detected", "no light"),
            Self::Lock => BinarySensorClassMeaning::new("open (unlocked)", "closed (locked)"),
            Self::Moisture => BinarySensorClassMeaning::new("wet", "dry"),
            Self::Motion => BinarySensorClassMeaning::new("motion detected", "no motion (clear)"),
            Self::Moving => BinarySensorClassMeaning::new("moving", "not moving (stopped)"),
            Self::Occupancy => BinarySensorClassMeaning::new("occupied", "not occupied (clear)"),
            Self::Plug => BinarySensorClassMeaning::new("plugged in", "unplugged"),
            Self::Power => BinarySensorClassMeaning::new("power detected", "no power"),
            Self::Presence => BinarySensorClassMeaning::new("home", "away"),
            Self::Problem => BinarySensorClassMeaning::new("problem detected", "no problem (OK)"),
            Self::Running => BinarySensorClassMeaning::new("running", "not running"),
            Self::Safety => BinarySensorClassMeaning::new("unsafe", "safe"),
            Self::Smoke => BinarySensorClassMeaning::new("smoke detected", "no smoke (clear)"),
            Self::Sound => BinarySensorClassMeaning::new("sound detected", "no sound"),
            Self::Tamper => {
                BinarySensorClassMeaning::new("tampering detected", "no tampering (clear)")
            }
            Self::Update => BinarySensorClassMeaning::new("update available", "up-to-date"),
            Self::Vibration => BinarySensorClassMeaning::new("vibration detected", "no vibration"),
        }
    }
}

impl fmt::Display for BinarySensorDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for BinarySensorDeviceClass {
    type Err = UnknownBinarySensorDeviceClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|class| class.as_str() == normalized)
            .ok_or_else(|| UnknownBinarySensorDeviceClass(value.to_owned()))
    }
}

impl TryFrom<&str> for BinarySensorDeviceClass {
    type Error = UnknownBinarySensorDeviceClass;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Parse failure for an unsupported binary-sensor device class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownBinarySensorDeviceClass(String);

impl fmt::Display for UnknownBinarySensorDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown binary-sensor device class {:?}", self.0)
    }
}

impl std::error::Error for UnknownBinarySensorDeviceClass {}

/// Static meaning of the canonical `on` and `off` states for a device class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinarySensorClassMeaning {
    /// Meaning of state `on`.
    pub on: &'static str,
    /// Meaning of state `off`.
    pub off: &'static str,
}

impl BinarySensorClassMeaning {
    const fn new(on: &'static str, off: &'static str) -> Self {
        Self { on, off }
    }
}

/// Provider description for one `binary_sensor` registry entity.
///
/// Registry identity, device links, and provider metadata are projected to the
/// shared [`EntitySpec`]. Registry-owned overrides remain the lifecycle's
/// responsibility and take priority over the provider's original metadata.
#[derive(Clone, Debug)]
pub struct BinarySensorDescriptor {
    spec: EntitySpec,
    device_class: Option<BinarySensorDeviceClass>,
}

impl BinarySensorDescriptor {
    /// Create a binary-sensor descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            spec: EntitySpec::new("binary_sensor", platform, provider_id, provider_key),
            device_class: None,
        }
    }

    /// Return the currently described device class, if one was set.
    #[must_use]
    pub const fn device_class(&self) -> Option<BinarySensorDeviceClass> {
        self.device_class
    }

    /// Set the typed binary-sensor device class.
    #[must_use]
    pub fn with_device_class(mut self, device_class: BinarySensorDeviceClass) -> Self {
        self.spec.info.original_device_class = Some(device_class.as_str().to_owned());
        self.device_class = Some(device_class);
        self
    }

    /// Clear the provider's device-class metadata.
    #[must_use]
    pub fn without_device_class(mut self) -> Self {
        self.spec.info.original_device_class = None;
        self.device_class = None;
        self
    }

    /// Set the registry unique ID.
    #[must_use]
    pub fn with_unique_id(mut self, unique_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_unique_id(unique_id);
        self
    }

    /// Associate the descriptor with a config entry.
    #[must_use]
    pub fn with_config_entry_id(mut self, config_entry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_entry_id(config_entry_id);
        self
    }

    /// Associate the descriptor with a config subentry.
    #[must_use]
    pub fn with_config_subentry_id(mut self, config_subentry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_subentry_id(config_subentry_id);
        self
    }

    /// Associate the descriptor with a device registry entry.
    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_device_id(device_id);
        self
    }

    /// Associate the descriptor with an area registry entry.
    #[must_use]
    pub fn with_area_id(mut self, area_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_area_id(area_id);
        self
    }

    /// Set the suggested object ID for newly registered entities.
    #[must_use]
    pub fn with_suggested_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_suggested_object_id(object_id);
        self
    }

    /// Set a provider-owned entity ID receipt used during migration.
    #[must_use]
    pub fn with_prior_entity_id(mut self, entity_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_prior_entity_id(entity_id);
        self
    }

    /// Set the provider's original name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.spec.info.original_name = Some(name.into());
        self
    }

    /// Set the provider's original icon.
    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.spec.info.original_icon = Some(icon.into());
        self
    }

    /// Set the entity category (normally `diagnostic` for a binary sensor).
    #[must_use]
    pub fn with_entity_category(mut self, category: impl Into<String>) -> Self {
        self.spec.info.entity_category = Some(category.into());
        self
    }

    /// Set whether the entity name should be combined with the device name.
    #[must_use]
    pub fn with_has_entity_name(mut self, has_entity_name: bool) -> Self {
        self.spec.info.has_entity_name = has_entity_name;
        self
    }

    /// Set a translation key on the provider entity metadata.
    #[must_use]
    pub fn with_translation_key(mut self, translation_key: impl Into<String>) -> Self {
        self.spec.info.translation_key = Some(translation_key.into());
        self
    }

    /// Set provider capability attributes.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Value) -> Self {
        self.spec.info.capabilities = Some(capabilities);
        self
    }

    /// Set provider-supported features.
    #[must_use]
    pub fn with_supported_features(mut self, supported_features: u64) -> Self {
        self.spec.info.supported_features = supported_features;
        self
    }

    /// Set whether newly created registry entries start enabled.
    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Build a lifecycle-ready `EntitySpec` without consuming the descriptor.
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        self.spec.clone()
    }

    /// Consume this descriptor and build its lifecycle-ready `EntitySpec`.
    #[must_use]
    pub fn into_entity_spec(self) -> EntitySpec {
        self.spec
    }
}

/// Current provider value and dynamic attributes for one binary sensor.
///
/// `is_on` and `available` are independent: a missing value is `unknown` while
/// available, while an unavailable sensor projects to `unavailable` through
/// the shared entity lifecycle.
#[derive(Clone, Debug)]
pub struct BinarySensorSnapshot {
    is_on: Option<bool>,
    available: bool,
    extra_attributes: Map<String, Value>,
    context: Context,
}

impl BinarySensorSnapshot {
    /// Create an available snapshot with a typed binary value.
    #[must_use]
    pub fn new(is_on: Option<bool>) -> Self {
        Self {
            is_on,
            available: true,
            extra_attributes: Map::new(),
            context: Context::default(),
        }
    }

    /// Create an unavailable snapshot, independently of the last known value.
    #[must_use]
    pub fn unavailable(is_on: Option<bool>) -> Self {
        Self::new(is_on).with_available(false)
    }

    /// Return the source's current binary value.
    #[must_use]
    pub const fn is_on(&self) -> Option<bool> {
        self.is_on
    }

    /// Return whether the provider currently considers the entity available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }

    /// Set availability independently of the value.
    #[must_use]
    pub fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Replace the causal context retained by the entity lifecycle.
    #[must_use]
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    /// Add or omit one extra attribute.
    ///
    /// `None` removes the key; an explicit JSON `null` is retained. Standard metadata such as
    /// `device_class`, `icon`, and `friendly_name` is resolved by the shared
    /// lifecycle: registry overrides win, present dynamic values follow, and
    /// provider metadata supplies the fallback.
    #[must_use]
    pub fn with_extra_attribute(mut self, name: impl Into<String>, value: Option<Value>) -> Self {
        let name = name.into();
        if let Some(value) = value {
            self.extra_attributes.insert(name, value);
        } else {
            self.extra_attributes.remove(&name);
        }
        self
    }

    /// Merge extra attributes, preserving explicit JSON `null` values.
    #[must_use]
    pub fn with_extra_attributes(mut self, attributes: Map<String, Value>) -> Self {
        self.extra_attributes.extend(attributes);
        self
    }

    /// Return the projected canonical state string, or `None` for `unknown`.
    #[must_use]
    pub const fn state(&self) -> Option<&'static str> {
        match self.is_on {
            Some(true) => Some("on"),
            Some(false) => Some("off"),
            None => None,
        }
    }

    /// Convert this value to a shared lifecycle snapshot.
    #[must_use]
    pub fn entity_snapshot(&self) -> EntitySnapshot {
        EntitySnapshot {
            state: self.state().map(str::to_owned),
            available: self.available,
            attributes: self.extra_attributes.clone(),
            context: self.context.clone(),
        }
    }

    /// Consume this value and convert it to a shared lifecycle snapshot.
    #[must_use]
    pub fn into_entity_snapshot(self) -> EntitySnapshot {
        EntitySnapshot {
            state: self.state().map(str::to_owned),
            available: self.available,
            attributes: self.extra_attributes,
            context: self.context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{MemoryStore, Runtime};
    use hass_entity::{EntityLifecycle, EntityStatus};
    use hass_registry::{EntityUpdate, Registries};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if condition() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle change did not arrive");
    }

    #[test]
    fn device_classes_match_the_pinned_source_values_and_meanings() {
        let expected = [
            "battery",
            "battery_charging",
            "carbon_monoxide",
            "cold",
            "connectivity",
            "door",
            "garage_door",
            "gas",
            "glass_break",
            "heat",
            "light",
            "lock",
            "moisture",
            "motion",
            "moving",
            "occupancy",
            "opening",
            "plug",
            "power",
            "presence",
            "problem",
            "running",
            "safety",
            "smoke",
            "sound",
            "tamper",
            "update",
            "vibration",
            "window",
        ];
        assert_eq!(BinarySensorDeviceClass::ALL.len(), expected.len());
        for (class, expected_value) in BinarySensorDeviceClass::ALL.iter().zip(expected) {
            assert_eq!(class.as_str(), expected_value);
            assert_eq!(
                expected_value.parse::<BinarySensorDeviceClass>(),
                Ok(*class)
            );
        }
        assert_eq!("BATTERY".parse(), Ok(BinarySensorDeviceClass::Battery));
        assert!("unknown".parse::<BinarySensorDeviceClass>().is_err());
        assert_eq!(
            BinarySensorDeviceClass::Battery.meaning(),
            BinarySensorClassMeaning {
                on: "low",
                off: "normal"
            }
        );
        assert_eq!(
            BinarySensorDeviceClass::Door.meaning(),
            BinarySensorClassMeaning {
                on: "open",
                off: "closed"
            }
        );
    }

    #[tokio::test]
    async fn lifecycle_publishes_binary_values_context_metadata_and_availability() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let descriptor = BinarySensorDescriptor::new("fixture", "fixture:entry-a", "battery")
            .with_unique_id("battery-1")
            .with_device_class(BinarySensorDeviceClass::Battery)
            .with_name("Battery health")
            .with_icon("mdi:battery")
            .with_suggested_object_id("battery_health");
        let handle = lifecycle
            .register_provider_batch("fixture:entry-a", vec![descriptor.entity_spec()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();

        let context = Context::default();
        handle
            .publish(
                BinarySensorSnapshot::new(Some(true))
                    .with_context(context.clone())
                    .with_extra_attribute("sample", Some(json!(1)))
                    .with_extra_attribute("device_class", Some(json!("motion")))
                    .with_extra_attribute("icon", Some(json!("mdi:alert")))
                    .with_extra_attribute("friendly_name", Some(json!("wrong name")))
                    .with_extra_attribute("omitted", None)
                    .with_extra_attribute("explicit_null", Some(Value::Null))
                    .with_extra_attribute("cleared", Some(json!(true)))
                    .with_extra_attribute("cleared", None)
                    .into_entity_snapshot(),
            )
            .unwrap()
            .unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "on");
        assert_eq!(state.context, context);
        assert_eq!(state.attributes.get("sample"), Some(&json!(1)));
        assert_eq!(state.attributes.get("device_class"), Some(&json!("motion")));
        assert_eq!(state.attributes.get("icon"), Some(&json!("mdi:alert")));
        assert_eq!(
            state.attributes.get("friendly_name"),
            Some(&json!("wrong name"))
        );
        assert!(!state.attributes.contains_key("omitted"));
        assert_eq!(state.attributes.get("explicit_null"), Some(&Value::Null));
        assert!(!state.attributes.contains_key("cleared"));

        handle
            .publish(BinarySensorSnapshot::new(Some(false)).into_entity_snapshot())
            .unwrap()
            .unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "off");
        assert_eq!(
            state.attributes.get("device_class"),
            Some(&json!("battery"))
        );
        assert_eq!(state.attributes.get("icon"), Some(&json!("mdi:battery")));
        assert_eq!(
            state.attributes.get("friendly_name"),
            Some(&json!("Battery health"))
        );

        handle
            .publish(BinarySensorSnapshot::new(None).into_entity_snapshot())
            .unwrap()
            .unwrap();
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "unknown");

        handle
            .publish(
                BinarySensorSnapshot::unavailable(Some(true))
                    .with_extra_attribute("sample", Some(json!(2)))
                    .into_entity_snapshot(),
            )
            .unwrap()
            .unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "unavailable");
        assert!(!state.attributes.contains_key("sample"));

        handle.release().unwrap();
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "unavailable");
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_tracks_registry_rename_disable_and_release() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let descriptor = BinarySensorDescriptor::new("fixture", "fixture:entry-b", "door")
            .with_unique_id("door-1")
            .with_device_class(BinarySensorDeviceClass::Door)
            .with_name("Front door")
            .with_enabled_by_default(true);
        let handle = lifecycle
            .register_provider_batch("fixture:entry-b", vec![descriptor.into_entity_spec()])
            .unwrap()
            .remove(0);
        let original_id = handle.current_entity_id().unwrap();
        handle
            .publish(
                BinarySensorSnapshot::new(Some(true))
                    .with_extra_attribute("provider_only", Some(json!(true)))
                    .into_entity_snapshot(),
            )
            .unwrap()
            .unwrap();

        let renamed_id = "binary_sensor.front_door_renamed";
        registries
            .update_entity(
                &original_id,
                EntityUpdate {
                    new_entity_id: Some(renamed_id.into()),
                    name: Some(Some("User door label".into())),
                    device_class: Some(Some("problem".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(&original_id).is_none()
                && runtime.get_state(renamed_id).is_some()
                && handle.current_entity_id().as_deref() == Some(renamed_id)
        })
        .await;
        let renamed = runtime.get_state(renamed_id).unwrap();
        assert_eq!(renamed.state, "on");
        assert_eq!(
            renamed.attributes.get("friendly_name"),
            Some(&json!("User door label"))
        );
        assert_eq!(
            renamed.attributes.get("device_class"),
            Some(&json!("problem"))
        );

        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            matches!(handle.status(), EntityStatus::Disabled { .. })
                && runtime.get_state(renamed_id).is_none()
        })
        .await;
        assert!(handle.current_entity_id().is_none());
        assert!(
            handle
                .publish(BinarySensorSnapshot::new(Some(false)).into_entity_snapshot())
                .unwrap()
                .is_none()
        );

        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| handle.current_entity_id().as_deref() == Some(renamed_id)).await;
        handle
            .publish(BinarySensorSnapshot::new(Some(false)).into_entity_snapshot())
            .unwrap()
            .unwrap();
        handle.release().unwrap();
        let released = runtime.get_state(renamed_id).unwrap();
        assert_eq!(released.state, "unavailable");
        assert_eq!(released.attributes.get("restored"), Some(&json!(true)));
        assert!(!released.attributes.contains_key("provider_only"));
        assert!(matches!(handle.status(), EntityStatus::Released { .. }));
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn releasing_generated_binary_sensor_removes_its_provider_state() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch(
                "fixture:generated",
                vec![
                    BinarySensorDescriptor::new("fixture", "fixture:generated", "transient")
                        .with_suggested_object_id("transient_sensor")
                        .into_entity_spec(),
                ],
            )
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle
            .publish(BinarySensorSnapshot::new(Some(true)).into_entity_snapshot())
            .unwrap()
            .unwrap();
        assert!(runtime.get_state(&entity_id).is_some());

        handle.release().unwrap();

        assert!(runtime.get_state(&entity_id).is_none());
        lifecycle.shutdown().await.unwrap();
    }
}
