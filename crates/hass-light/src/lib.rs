//! Typed descriptions, state projection, and service normalization for native
//! Home Assistant `light` entities.
//!
//! Behavior is pinned to Home Assistant Core commit
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. This crate covers the light
//! color-mode contract, capability metadata, state attributes, common color
//! conversions, and the standard `turn_on`, `turn_off`, and `toggle` service
//! vocabulary. Profile lookup and the full CSS color-name table remain
//! integration-owned and are not synthesized here.

mod color;
mod service;

pub use color::{
    hs_to_rgb, kelvin_to_rgb, kelvin_to_rgbww, rgb_to_hs, rgb_to_rgbw, rgb_to_rgbww, rgb_to_xy,
    rgbw_to_rgb, rgbww_to_rgb, xy_to_kelvin, xy_to_rgb,
};
pub use service::{
    LIGHT_SERVICE_ARGUMENTS, LIGHT_TARGET_FIELDS, LIGHT_TURN_OFF_ARGUMENTS,
    LIGHT_TURN_ON_ARGUMENTS, LightAction, LightCommand, LightServiceError, LightServiceRequest,
    NormalizedLightCommand,
};

use hass_core::Context;
use hass_entity::{EntityPublication, EntitySnapshot, EntitySpec};
use serde_json::{Map, Value, json};
use std::fmt;
use std::str::FromStr;

/// Home Assistant entity domain for native lights.
pub const LIGHT_DOMAIN: &str = "light";
/// Default warm-end color temperature in Kelvin used by Home Assistant Core.
pub const LIGHT_DEFAULT_MIN_KELVIN: u32 = 2_000;
/// Default cool-end color temperature in Kelvin used by Home Assistant Core.
pub const LIGHT_DEFAULT_MAX_KELVIN: u32 = 6_535;
/// The effect string Home Assistant uses to mean that no effect is active.
pub const LIGHT_EFFECT_OFF: &str = "off";

/// State and capability attribute names emitted by Home Assistant's light
/// entity.
pub mod attr {
    pub const SUPPORTED_COLOR_MODES: &str = "supported_color_modes";
    pub const MIN_COLOR_TEMP_KELVIN: &str = "min_color_temp_kelvin";
    pub const MAX_COLOR_TEMP_KELVIN: &str = "max_color_temp_kelvin";
    pub const EFFECT_LIST: &str = "effect_list";
    pub const EFFECT: &str = "effect";
    pub const COLOR_MODE: &str = "color_mode";
    pub const BRIGHTNESS: &str = "brightness";
    pub const COLOR_TEMP_KELVIN: &str = "color_temp_kelvin";
    pub const HS_COLOR: &str = "hs_color";
    pub const RGB_COLOR: &str = "rgb_color";
    pub const XY_COLOR: &str = "xy_color";
    pub const RGBW_COLOR: &str = "rgbw_color";
    pub const RGBWW_COLOR: &str = "rgbww_color";
    pub const SUPPORTED_FEATURES: &str = "supported_features";
}

/// Color modes accepted by Home Assistant Core at the pinned revision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightColorMode {
    /// An unspecified mode. Valid in a state only where Core permits unknown.
    Unknown,
    /// On/off only; this must be the sole supported mode.
    OnOff,
    /// Brightness only; this must be the sole supported mode.
    Brightness,
    /// Color temperature in Kelvin.
    ColorTemp,
    /// Hue and saturation.
    Hs,
    /// CIE xy chromaticity.
    Xy,
    /// Red, green, and blue channels.
    Rgb,
    /// Red, green, blue, and white channels.
    Rgbw,
    /// Red, green, blue, cold-white, and warm-white channels.
    Rgbww,
    /// White brightness; this can only accompany a color-capable mode.
    White,
}

impl LightColorMode {
    /// All enum values in the pinned Home Assistant declaration order.
    pub const ALL: [Self; 10] = [
        Self::Unknown,
        Self::OnOff,
        Self::Brightness,
        Self::ColorTemp,
        Self::Hs,
        Self::Xy,
        Self::Rgb,
        Self::Rgbw,
        Self::Rgbww,
        Self::White,
    ];

    /// Valid values for `supported_color_modes`, excluding `unknown`.
    pub const VALID: [Self; 9] = [
        Self::OnOff,
        Self::Brightness,
        Self::ColorTemp,
        Self::Hs,
        Self::Xy,
        Self::Rgb,
        Self::Rgbw,
        Self::Rgbww,
        Self::White,
    ];

    /// Return the exact Home Assistant string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::OnOff => "onoff",
            Self::Brightness => "brightness",
            Self::ColorTemp => "color_temp",
            Self::Hs => "hs",
            Self::Xy => "xy",
            Self::Rgb => "rgb",
            Self::Rgbw => "rgbw",
            Self::Rgbww => "rgbww",
            Self::White => "white",
        }
    }

    /// Whether this mode supports a brightness attribute under Core's mode
    /// rules. `onoff` is the sole exception among valid supported modes.
    #[must_use]
    pub const fn supports_brightness(self) -> bool {
        !matches!(self, Self::Unknown | Self::OnOff)
    }

    /// Whether this is one of Core's five chromatic color modes.
    #[must_use]
    pub const fn is_color(self) -> bool {
        matches!(
            self,
            Self::Hs | Self::Xy | Self::Rgb | Self::Rgbw | Self::Rgbww
        )
    }
}

impl fmt::Display for LightColorMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LightColorMode {
    type Err = UnknownLightColorMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == value)
            .ok_or_else(|| UnknownLightColorMode(value.to_owned()))
    }
}

impl TryFrom<&str> for LightColorMode {
    type Error = UnknownLightColorMode;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Parse failure for an unrecognized light color mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownLightColorMode(String);

impl fmt::Display for UnknownLightColorMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown light color mode {:?}", self.0)
    }
}

impl std::error::Error for UnknownLightColorMode {}

/// A standard feature advertised by a native light.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightFeature {
    /// Supports dynamic effects.
    Effect,
    /// Supports short or long flash requests.
    Flash,
    /// Supports transition durations.
    Transition,
}

impl LightFeature {
    /// Return the pinned `LightEntityFeature` bit value.
    #[must_use]
    pub const fn bits(self) -> u64 {
        match self {
            Self::Effect => 4,
            Self::Flash => 8,
            Self::Transition => 32,
        }
    }
}

/// Home Assistant light feature bits. Unknown bits are retained so newer
/// Core features survive a round trip through this SDK.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightFeatures(u64);

impl LightFeatures {
    /// No advertised light features.
    pub const NONE: Self = Self(0);
    /// Supports effects.
    pub const EFFECT: Self = Self(LightFeature::Effect.bits());
    /// Supports flashing.
    pub const FLASH: Self = Self(LightFeature::Flash.bits());
    /// Supports transitions.
    pub const TRANSITION: Self = Self(LightFeature::Transition.bits());

    /// Retain every supplied feature bit.
    #[must_use]
    pub const fn from_bits_retain(bits: u64) -> Self {
        Self(bits)
    }

    /// Return the raw Home Assistant feature mask.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Test whether a standard feature is enabled.
    #[must_use]
    pub const fn contains(self, feature: LightFeature) -> bool {
        self.0 & feature.bits() == feature.bits()
    }

    /// Add one standard feature.
    #[must_use]
    pub const fn with(self, feature: LightFeature) -> Self {
        Self(self.0 | feature.bits())
    }
}

impl std::ops::BitOr for LightFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for LightFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Stable provider description for one native light.
#[derive(Clone, Debug)]
pub struct LightDescriptor {
    spec: EntitySpec,
    supported_color_modes: Option<Vec<LightColorMode>>,
    supported_features: LightFeatures,
    min_color_temp_kelvin: u32,
    max_color_temp_kelvin: u32,
    effect_list: Option<Vec<String>>,
}

impl LightDescriptor {
    /// Create a light descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            spec: EntitySpec::new(LIGHT_DOMAIN, platform, provider_id, provider_key),
            supported_color_modes: None,
            supported_features: LightFeatures::NONE,
            min_color_temp_kelvin: LIGHT_DEFAULT_MIN_KELVIN,
            max_color_temp_kelvin: LIGHT_DEFAULT_MAX_KELVIN,
            effect_list: None,
        }
    }

    /// Set supported modes. Validation and stable sorting occur when the
    /// descriptor is validated or projected.
    #[must_use]
    pub fn with_supported_color_modes(
        mut self,
        modes: impl IntoIterator<Item = LightColorMode>,
    ) -> Self {
        self.supported_color_modes = Some(modes.into_iter().collect());
        self
    }

    /// Set the pinned light feature mask.
    #[must_use]
    pub fn with_supported_features(mut self, features: LightFeatures) -> Self {
        self.supported_features = features;
        self
    }

    /// Add one pinned light feature bit.
    #[must_use]
    pub fn with_feature(mut self, feature: LightFeature) -> Self {
        self.supported_features = self.supported_features.with(feature);
        self
    }

    /// Set the warm-end color-temperature bound in Kelvin.
    #[must_use]
    pub fn with_min_color_temp_kelvin(mut self, kelvin: u32) -> Self {
        self.min_color_temp_kelvin = kelvin;
        self
    }

    /// Set the cool-end color-temperature bound in Kelvin.
    #[must_use]
    pub fn with_max_color_temp_kelvin(mut self, kelvin: u32) -> Self {
        self.max_color_temp_kelvin = kelvin;
        self
    }

    /// Set the provider's supported effect names. The list is advertised only
    /// when the `effect` feature bit is enabled, matching Core.
    #[must_use]
    pub fn with_effect_list(
        mut self,
        effects: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.effect_list = Some(effects.into_iter().map(Into::into).collect());
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

    /// Set the suggested object ID for a newly registered entity.
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

    /// Set the provider's original entity name.
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

    /// Set the entity category supplied by the provider.
    #[must_use]
    pub fn with_entity_category(mut self, category: impl Into<String>) -> Self {
        self.spec.info.entity_category = Some(category.into());
        self
    }

    /// Set whether the entity name is combined with its device name.
    #[must_use]
    pub fn with_has_entity_name(mut self, has_entity_name: bool) -> Self {
        self.spec.info.has_entity_name = has_entity_name;
        self
    }

    /// Set the provider translation key.
    #[must_use]
    pub fn with_translation_key(mut self, translation_key: impl Into<String>) -> Self {
        self.spec.info.translation_key = Some(translation_key.into());
        self
    }

    /// Set whether a newly created registry entry starts enabled.
    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Current supported color modes in stable Home Assistant string order.
    #[must_use]
    pub fn supported_color_modes(&self) -> Option<Vec<LightColorMode>> {
        self.supported_color_modes
            .as_deref()
            .map(normalize_color_modes)
    }

    /// Current supported feature bits.
    #[must_use]
    pub const fn supported_features(&self) -> LightFeatures {
        self.supported_features
    }

    /// Return the currently described color-temperature lower bound.
    #[must_use]
    pub const fn min_color_temp_kelvin(&self) -> u32 {
        self.min_color_temp_kelvin
    }

    /// Return the currently described color-temperature upper bound.
    #[must_use]
    pub const fn max_color_temp_kelvin(&self) -> u32 {
        self.max_color_temp_kelvin
    }

    /// Validate that supported color modes satisfy Home Assistant's pinned
    /// invariants.
    pub fn validate(&self) -> Result<(), LightError> {
        let Some(modes) = self.supported_color_modes.as_deref() else {
            return Err(LightError::MissingSupportedColorModes);
        };
        validate_color_modes(modes).map(|_| ())
    }

    /// Return the light capability attributes advertised by the entity.
    pub fn capability_attributes(&self) -> Result<Map<String, Value>, LightError> {
        self.validate()?;
        let modes = self
            .supported_color_modes()
            .expect("validated supported color modes are present");
        let mut attributes = Map::new();
        if modes.contains(&LightColorMode::ColorTemp) {
            attributes.insert(
                attr::MIN_COLOR_TEMP_KELVIN.into(),
                json!(self.min_color_temp_kelvin),
            );
            attributes.insert(
                attr::MAX_COLOR_TEMP_KELVIN.into(),
                json!(self.max_color_temp_kelvin),
            );
        }
        if self.supported_features.contains(LightFeature::Effect) {
            attributes.insert(attr::EFFECT_LIST.into(), json!(self.effect_list));
        }
        attributes.insert(
            attr::SUPPORTED_COLOR_MODES.into(),
            json!(modes.iter().map(|mode| mode.as_str()).collect::<Vec<_>>()),
        );
        Ok(attributes)
    }

    /// Build a lifecycle-ready `EntitySpec` without consuming the descriptor.
    /// Call [`Self::validate`] before registration.
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        let mut spec = self.spec.clone();
        spec.info.supported_features = self.supported_features.bits();
        spec.info.capabilities = Some(Value::Object(self.capability_attributes_unchecked()));
        spec
    }

    /// Validate and build a lifecycle-ready `EntitySpec`.
    pub fn try_entity_spec(&self) -> Result<EntitySpec, LightError> {
        self.validate()?;
        Ok(self.entity_spec())
    }

    /// Consume this descriptor and build its lifecycle-ready `EntitySpec`.
    /// Call [`Self::validate`] before registration.
    #[must_use]
    pub fn into_entity_spec(self) -> EntitySpec {
        self.entity_spec()
    }

    fn capability_attributes_unchecked(&self) -> Map<String, Value> {
        let mut attributes = Map::new();
        if let Some(modes) = self.supported_color_modes() {
            if modes.contains(&LightColorMode::ColorTemp) {
                attributes.insert(
                    attr::MIN_COLOR_TEMP_KELVIN.into(),
                    json!(self.min_color_temp_kelvin),
                );
                attributes.insert(
                    attr::MAX_COLOR_TEMP_KELVIN.into(),
                    json!(self.max_color_temp_kelvin),
                );
            }
            attributes.insert(
                attr::SUPPORTED_COLOR_MODES.into(),
                json!(modes.iter().map(|mode| mode.as_str()).collect::<Vec<_>>()),
            );
        }
        if self.supported_features.contains(LightFeature::Effect) {
            attributes.insert(attr::EFFECT_LIST.into(), json!(self.effect_list));
        }
        attributes
    }
}

fn normalize_color_modes(modes: &[LightColorMode]) -> Vec<LightColorMode> {
    let mut modes = modes.to_vec();
    modes.sort_by_key(|mode| mode.as_str());
    modes.dedup();
    modes
}

fn validate_color_modes(modes: &[LightColorMode]) -> Result<Vec<LightColorMode>, LightError> {
    let modes = normalize_color_modes(modes);
    if modes.is_empty() {
        return Err(LightError::InvalidSupportedColorModes("empty mode set"));
    }
    if modes.contains(&LightColorMode::Unknown) {
        return Err(LightError::InvalidSupportedColorModes(
            "unknown cannot be advertised",
        ));
    }
    if modes.len() > 1
        && (modes.contains(&LightColorMode::OnOff) || modes.contains(&LightColorMode::Brightness))
    {
        return Err(LightError::InvalidSupportedColorModes(
            "onoff and brightness must each be the only supported mode",
        ));
    }
    if modes.contains(&LightColorMode::White) && !modes.iter().any(|mode| mode.is_color()) {
        return Err(LightError::InvalidSupportedColorModes(
            "white requires a color mode",
        ));
    }
    Ok(modes)
}

/// Current provider-reported light state and dynamic attributes.
#[derive(Clone, Debug)]
pub struct LightSnapshot {
    is_on: Option<bool>,
    available: bool,
    brightness: Option<u8>,
    color_mode: Option<LightColorMode>,
    color_temp_kelvin: Option<u32>,
    hs_color: Option<[f64; 2]>,
    xy_color: Option<[f64; 2]>,
    rgb_color: Option<[u8; 3]>,
    rgbw_color: Option<[u8; 4]>,
    rgbww_color: Option<[u8; 5]>,
    effect: Option<String>,
    extra_attributes: Map<String, Value>,
    context: Context,
}

impl LightSnapshot {
    /// Create an available snapshot with a typed on/off state.
    #[must_use]
    pub fn new(is_on: Option<bool>) -> Self {
        Self {
            is_on,
            available: true,
            brightness: None,
            color_mode: None,
            color_temp_kelvin: None,
            hs_color: None,
            xy_color: None,
            rgb_color: None,
            rgbw_color: None,
            rgbww_color: None,
            effect: None,
            extra_attributes: Map::new(),
            context: Context::default(),
        }
    }

    /// Create an unavailable snapshot while retaining the last reported value.
    #[must_use]
    pub fn unavailable(is_on: Option<bool>) -> Self {
        Self {
            available: false,
            ..Self::new(is_on)
        }
    }

    /// Set whether the provider currently considers this light available.
    #[must_use]
    pub fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Set the reported brightness on the Home Assistant 0..255 scale.
    #[must_use]
    pub fn with_brightness(mut self, brightness: Option<u8>) -> Self {
        self.brightness = brightness;
        self
    }

    /// Set the currently active color mode.
    #[must_use]
    pub fn with_color_mode(mut self, color_mode: Option<LightColorMode>) -> Self {
        self.color_mode = color_mode;
        self
    }

    /// Set the reported color temperature in Kelvin.
    #[must_use]
    pub fn with_color_temp_kelvin(mut self, kelvin: Option<u32>) -> Self {
        self.color_temp_kelvin = kelvin;
        self
    }

    /// Set the reported hue and saturation values.
    #[must_use]
    pub fn with_hs_color(mut self, hs_color: Option<[f64; 2]>) -> Self {
        self.hs_color = hs_color;
        self
    }

    /// Set the reported CIE xy coordinates.
    #[must_use]
    pub fn with_xy_color(mut self, xy_color: Option<[f64; 2]>) -> Self {
        self.xy_color = xy_color;
        self
    }

    /// Set the reported RGB color.
    #[must_use]
    pub fn with_rgb_color(mut self, rgb_color: Option<[u8; 3]>) -> Self {
        self.rgb_color = rgb_color;
        self
    }

    /// Set the reported RGBW color.
    #[must_use]
    pub fn with_rgbw_color(mut self, rgbw_color: Option<[u8; 4]>) -> Self {
        self.rgbw_color = rgbw_color;
        self
    }

    /// Set the reported RGBWW color.
    #[must_use]
    pub fn with_rgbww_color(mut self, rgbww_color: Option<[u8; 5]>) -> Self {
        self.rgbww_color = rgbww_color;
        self
    }

    /// Set the current effect name.
    #[must_use]
    pub fn with_effect(mut self, effect: Option<impl Into<String>>) -> Self {
        self.effect = effect.map(Into::into);
        self
    }

    /// Add or omit one extra state attribute. `Some(Value::Null)` retains an
    /// explicit JSON null; `None` removes the key.
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

    /// Merge provider extra attributes, preserving explicit JSON nulls.
    #[must_use]
    pub fn with_extra_attributes(mut self, attributes: Map<String, Value>) -> Self {
        self.extra_attributes.extend(attributes);
        self
    }

    /// Replace the causal context retained by the entity lifecycle.
    #[must_use]
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
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

    /// Return the reported Home Assistant brightness.
    #[must_use]
    pub const fn brightness(&self) -> Option<u8> {
        self.brightness
    }

    /// Return the active color mode.
    #[must_use]
    pub const fn color_mode(&self) -> Option<LightColorMode> {
        self.color_mode
    }

    /// Return the projected canonical state string, or `None` for unknown.
    #[must_use]
    pub const fn state(&self) -> Option<&'static str> {
        match self.is_on {
            Some(true) => Some("on"),
            Some(false) => Some("off"),
            None => None,
        }
    }

    /// Project this reading and its descriptor into lifecycle state and
    /// publication values.
    pub fn project(&self, descriptor: &LightDescriptor) -> Result<LightProjection, LightError> {
        descriptor.validate()?;
        let modes = descriptor
            .supported_color_modes()
            .expect("validated supported color modes are present");
        let on = self.available && self.is_on == Some(true);
        let color_mode = if on { self.color_mode } else { None };
        if on && color_mode.is_none() {
            return Err(LightError::MissingColorMode);
        }

        let effect = if descriptor.supported_features.contains(LightFeature::Effect) && on {
            self.effect.as_deref()
        } else {
            None
        };
        if let Some(mode) = color_mode {
            validate_reported_color_mode(mode, &modes, effect)?;
        }

        let mut attributes = self.extra_attributes.clone();
        let mut capability_attributes = descriptor.capability_attributes()?;
        attributes.append(&mut capability_attributes);
        attributes.insert(
            attr::SUPPORTED_FEATURES.into(),
            json!(descriptor.supported_features.bits()),
        );
        if descriptor.supported_features.contains(LightFeature::Effect) {
            attributes.insert(
                attr::EFFECT.into(),
                effect.map_or(Value::Null, |v| json!(v)),
            );
        }
        attributes.insert(
            attr::COLOR_MODE.into(),
            color_mode.map_or(Value::Null, |mode| json!(mode.as_str())),
        );

        if modes.iter().any(|mode| mode.supports_brightness()) {
            let brightness = color_mode
                .filter(|mode| mode.supports_brightness())
                .and(self.brightness);
            attributes.insert(
                attr::BRIGHTNESS.into(),
                brightness.map_or(Value::Null, |value| json!(value)),
            );
        }
        if modes.contains(&LightColorMode::ColorTemp) {
            let color_temp = color_mode
                .filter(|mode| *mode == LightColorMode::ColorTemp)
                .and(self.color_temp_kelvin);
            attributes.insert(
                attr::COLOR_TEMP_KELVIN.into(),
                color_temp.map_or(Value::Null, |value| json!(value)),
            );
        }

        if modes.iter().any(|mode| mode.is_color()) || modes.contains(&LightColorMode::ColorTemp) {
            attributes.insert(attr::HS_COLOR.into(), Value::Null);
            attributes.insert(attr::RGB_COLOR.into(), Value::Null);
            attributes.insert(attr::XY_COLOR.into(), Value::Null);
            if modes.contains(&LightColorMode::Rgbw) {
                attributes.insert(attr::RGBW_COLOR.into(), Value::Null);
            }
            if modes.contains(&LightColorMode::Rgbww) {
                attributes.insert(attr::RGBWW_COLOR.into(), Value::Null);
            }
            if let Some(mode) = color_mode {
                self.project_color(mode, descriptor, &mut attributes)?;
            }
        }

        let snapshot = EntitySnapshot {
            state: self.state().map(str::to_owned),
            available: self.available,
            attributes,
            context: self.context.clone(),
        };
        let mut publication = EntityPublication::from(snapshot.clone());
        for name in [
            attr::SUPPORTED_COLOR_MODES,
            attr::SUPPORTED_FEATURES,
            attr::MIN_COLOR_TEMP_KELVIN,
            attr::MAX_COLOR_TEMP_KELVIN,
            attr::EFFECT_LIST,
        ] {
            publication =
                publication.with_live_attribute(name, snapshot.attributes.get(name).cloned());
        }
        Ok(LightProjection {
            snapshot,
            publication,
        })
    }

    fn project_color(
        &self,
        mode: LightColorMode,
        descriptor: &LightDescriptor,
        attributes: &mut Map<String, Value>,
    ) -> Result<(), LightError> {
        match mode {
            LightColorMode::Hs => {
                if let Some(hs) = self.hs_color {
                    validate_hs(hs)?;
                    let rounded = [round_to(hs[0], 3), round_to(hs[1], 3)];
                    let rgb = color::hs_to_rgb(hs[0], hs[1]);
                    attributes.insert(attr::HS_COLOR.into(), json!(rounded));
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(attr::XY_COLOR.into(), json!(color::rgb_to_xy(rgb)));
                }
            }
            LightColorMode::Xy => {
                if let Some(xy) = self.xy_color {
                    validate_xy(xy)?;
                    let rgb = color::xy_to_rgb(xy);
                    attributes.insert(attr::HS_COLOR.into(), json!(color::rgb_to_hs(rgb)));
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(
                        attr::XY_COLOR.into(),
                        json!([round_to(xy[0], 6), round_to(xy[1], 6)]),
                    );
                }
            }
            LightColorMode::Rgb => {
                if let Some(rgb) = self.rgb_color {
                    attributes.insert(attr::HS_COLOR.into(), json!(color::rgb_to_hs(rgb)));
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(attr::XY_COLOR.into(), json!(color::rgb_to_xy(rgb)));
                }
            }
            LightColorMode::Rgbw => {
                if let Some(rgbw) = self.rgbw_color {
                    let rgb = color::rgbw_to_rgb(rgbw);
                    attributes.insert(attr::HS_COLOR.into(), json!(color::rgb_to_hs(rgb)));
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(attr::RGBW_COLOR.into(), json!(rgbw));
                    attributes.insert(attr::XY_COLOR.into(), json!(color::rgb_to_xy(rgb)));
                }
            }
            LightColorMode::Rgbww => {
                if let Some(rgbww) = self.rgbww_color {
                    let rgb = color::rgbww_to_rgb(
                        rgbww,
                        descriptor.min_color_temp_kelvin,
                        descriptor.max_color_temp_kelvin,
                    )?;
                    attributes.insert(attr::HS_COLOR.into(), json!(color::rgb_to_hs(rgb)));
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(attr::RGBWW_COLOR.into(), json!(rgbww));
                    attributes.insert(attr::XY_COLOR.into(), json!(color::rgb_to_xy(rgb)));
                }
            }
            LightColorMode::ColorTemp => {
                if let Some(kelvin) = self.color_temp_kelvin {
                    let hs = color::kelvin_to_hs(kelvin);
                    let rgb = color::hs_to_rgb(hs[0], hs[1]);
                    attributes.insert(
                        attr::HS_COLOR.into(),
                        json!([round_to(hs[0], 3), round_to(hs[1], 3)]),
                    );
                    attributes.insert(attr::RGB_COLOR.into(), json!(rgb));
                    attributes.insert(attr::XY_COLOR.into(), json!(color::rgb_to_xy(rgb)));
                }
            }
            LightColorMode::Unknown
            | LightColorMode::OnOff
            | LightColorMode::Brightness
            | LightColorMode::White => {}
        }
        Ok(())
    }
}

/// State plus availability-independent capability metadata ready for the
/// shared entity lifecycle.
#[derive(Clone, Debug)]
pub struct LightProjection {
    /// Ordinary state attributes, omitted while unavailable by the lifecycle.
    pub snapshot: EntitySnapshot,
    /// Publication that retains light capability attributes while unavailable.
    pub publication: EntityPublication,
}

/// Error returned for invalid native light descriptions or state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LightError {
    /// A light must advertise one or more supported color modes.
    MissingSupportedColorModes,
    /// A supported mode set violates a pinned Home Assistant invariant.
    InvalidSupportedColorModes(&'static str),
    /// An on light must report its active mode.
    MissingColorMode,
    /// The active mode is not supported by this descriptor.
    UnsupportedColorMode(LightColorMode),
    /// An effect may use onoff or brightness modes in addition to supported
    /// modes, as described by the pinned Core implementation.
    InvalidColorValue(&'static str),
    /// A color conversion requires a positive and ordered Kelvin range.
    InvalidTemperatureRange,
    /// A color conversion encountered a singular or non-finite value.
    InvalidColorConversion,
}

impl fmt::Display for LightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSupportedColorModes => {
                formatter.write_str("light descriptor has no supported color modes")
            }
            Self::InvalidSupportedColorModes(reason) => {
                write!(formatter, "invalid supported color modes: {reason}")
            }
            Self::MissingColorMode => formatter.write_str("an on light must report a color mode"),
            Self::UnsupportedColorMode(mode) => {
                write!(formatter, "light reported unsupported color mode {mode}")
            }
            Self::InvalidColorValue(field) => {
                write!(formatter, "invalid {field} value")
            }
            Self::InvalidTemperatureRange => {
                formatter.write_str("color temperature range must be positive and ordered")
            }
            Self::InvalidColorConversion => {
                formatter.write_str("light color conversion received a non-finite value")
            }
        }
    }
}

impl std::error::Error for LightError {}

fn validate_reported_color_mode(
    mode: LightColorMode,
    supported_modes: &[LightColorMode],
    effect: Option<&str>,
) -> Result<(), LightError> {
    if effect.is_none_or(|value| value.is_empty() || value == LIGHT_EFFECT_OFF) {
        if supported_modes.contains(&mode) {
            return Ok(());
        }
        return Err(LightError::UnsupportedColorMode(mode));
    }

    let effect_mode_allowed = supported_modes.contains(&mode)
        || mode == LightColorMode::OnOff
        || (mode == LightColorMode::Brightness
            && supported_modes
                .iter()
                .any(|supported| supported.supports_brightness()));
    if effect_mode_allowed {
        Ok(())
    } else {
        Err(LightError::UnsupportedColorMode(mode))
    }
}

fn validate_hs(hs: [f64; 2]) -> Result<(), LightError> {
    if hs.iter().any(|value| !value.is_finite())
        || !(0.0..=360.0).contains(&hs[0])
        || !(0.0..=100.0).contains(&hs[1])
    {
        return Err(LightError::InvalidColorValue(attr::HS_COLOR));
    }
    Ok(())
}

fn validate_xy(xy: [f64; 2]) -> Result<(), LightError> {
    if xy.iter().any(|value| !value.is_finite()) {
        return Err(LightError::InvalidColorValue(attr::XY_COLOR));
    }
    Ok(())
}

fn round_to(value: f64, places: u32) -> f64 {
    let scale = 10_f64.powi(places as i32);
    round_ties_even(value * scale) / scale
}

pub(crate) fn round_ties_even(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    let fraction = value - floor;
    if fraction < 0.5 {
        floor
    } else if fraction > 0.5 {
        floor + 1.0
    } else if floor % 2.0 == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

#[cfg(test)]
mod tests;
