//! Typed Home Assistant climate descriptions, state projection, and commands.
//!
//! The public contract follows Core commit
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. This crate has no provider I/O:
//! descriptors produce shared entity specifications, snapshots produce
//! [`EntityPublication`] values, and parsed commands resolve to ordered provider
//! effects for an adapter to execute.

mod service;

pub use service::{
    CLIMATE_TARGET_FIELDS, ClimateCommand, ClimateCommandPlan, ClimateEffect, ClimateService,
    ClimateServiceRequest,
};

use hass_core::Context;
use hass_entity::{EntityPublication, EntitySnapshot, EntitySpec};
use hass_units::{ConversionError, UnitFamily, convert};
use serde_json::{Map, Value, json};
use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};
use std::str::FromStr;

/// Home Assistant entity domain for native climate entities.
pub const CLIMATE_DOMAIN: &str = "climate";

/// Default temperature bounds in Celsius from the pinned climate domain.
pub const DEFAULT_MIN_TEMPERATURE_CELSIUS: f64 = 7.0;
pub const DEFAULT_MAX_TEMPERATURE_CELSIUS: f64 = 35.0;

/// Default target-humidity bounds from the pinned climate domain.
pub const DEFAULT_MIN_HUMIDITY: f64 = 30.0;
pub const DEFAULT_MAX_HUMIDITY: f64 = 99.0;

pub const PRESET_NONE: &str = "none";
pub const PRESET_ECO: &str = "eco";
pub const PRESET_AWAY: &str = "away";
pub const PRESET_BOOST: &str = "boost";
pub const PRESET_COMFORT: &str = "comfort";
pub const PRESET_HOME: &str = "home";
pub const PRESET_SLEEP: &str = "sleep";
pub const PRESET_ACTIVITY: &str = "activity";
pub const FAN_ON: &str = "on";
pub const FAN_OFF: &str = "off";
pub const FAN_AUTO: &str = "auto";
pub const FAN_LOW: &str = "low";
pub const FAN_MEDIUM: &str = "medium";
pub const FAN_HIGH: &str = "high";
pub const FAN_TOP: &str = "top";
pub const FAN_MIDDLE: &str = "middle";
pub const FAN_FOCUS: &str = "focus";
pub const FAN_DIFFUSE: &str = "diffuse";
pub const SWING_ON: &str = "on";
pub const SWING_OFF: &str = "off";
pub const SWING_BOTH: &str = "both";
pub const SWING_VERTICAL: &str = "vertical";
pub const SWING_HORIZONTAL: &str = "horizontal";
pub const SWING_HORIZONTAL_ON: &str = "on";
pub const SWING_HORIZONTAL_OFF: &str = "off";

pub const HVAC_MODES: &[HvacMode] = &HvacMode::ALL;
pub const CURRENT_HVAC_ACTIONS: &[HvacAction] = &HvacAction::ALL;

/// Climate state and capability attribute names at the pinned revision.
pub mod attr {
    pub const CURRENT_HUMIDITY: &str = "current_humidity";
    pub const CURRENT_TEMPERATURE: &str = "current_temperature";
    pub const FAN_MODE: &str = "fan_mode";
    pub const FAN_MODES: &str = "fan_modes";
    pub const HVAC_ACTION: &str = "hvac_action";
    pub const HVAC_MODE: &str = "hvac_mode";
    pub const HVAC_MODES: &str = "hvac_modes";
    pub const MAX_HUMIDITY: &str = "max_humidity";
    pub const MAX_TEMP: &str = "max_temp";
    pub const MIN_HUMIDITY: &str = "min_humidity";
    pub const MIN_TEMP: &str = "min_temp";
    pub const PRESET_MODE: &str = "preset_mode";
    pub const PRESET_MODES: &str = "preset_modes";
    pub const SUPPORTED_FEATURES: &str = "supported_features";
    pub const SWING_HORIZONTAL_MODE: &str = "swing_horizontal_mode";
    pub const SWING_HORIZONTAL_MODES: &str = "swing_horizontal_modes";
    pub const SWING_MODE: &str = "swing_mode";
    pub const SWING_MODES: &str = "swing_modes";
    pub const TARGET_HUMIDITY: &str = "humidity";
    pub const TARGET_HUMIDITY_STEP: &str = "target_humidity_step";
    pub const TARGET_TEMPERATURE: &str = "temperature";
    pub const TARGET_TEMP_HIGH: &str = "target_temp_high";
    pub const TARGET_TEMP_LOW: &str = "target_temp_low";
    pub const TARGET_TEMP_STEP: &str = "target_temp_step";
    pub const TEMPERATURE_UNIT: &str = "temperature_unit";
}

/// HVAC operation modes supported by Home Assistant Core.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HvacMode {
    Off,
    Heat,
    Cool,
    HeatCool,
    Auto,
    Dry,
    FanOnly,
}

impl HvacMode {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Heat,
        Self::Cool,
        Self::HeatCool,
        Self::Auto,
        Self::Dry,
        Self::FanOnly,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Heat => "heat",
            Self::Cool => "cool",
            Self::HeatCool => "heat_cool",
            Self::Auto => "auto",
            Self::Dry => "dry",
            Self::FanOnly => "fan_only",
        }
    }
}

impl fmt::Display for HvacMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HvacMode {
    type Err = ClimateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == value)
            .ok_or_else(|| ClimateError::InvalidHvacMode(value.to_owned()))
    }
}

/// Current HVAC action strings defined by Home Assistant Core.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HvacAction {
    Cooling,
    Defrosting,
    Drying,
    Fan,
    Heating,
    Idle,
    Off,
    Preheating,
}

impl HvacAction {
    pub const ALL: [Self; 8] = [
        Self::Cooling,
        Self::Defrosting,
        Self::Drying,
        Self::Fan,
        Self::Heating,
        Self::Idle,
        Self::Off,
        Self::Preheating,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cooling => "cooling",
            Self::Defrosting => "defrosting",
            Self::Drying => "drying",
            Self::Fan => "fan",
            Self::Heating => "heating",
            Self::Idle => "idle",
            Self::Off => "off",
            Self::Preheating => "preheating",
        }
    }
}

impl FromStr for HvacAction {
    type Err = ClimateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
            .ok_or_else(|| ClimateError::InvalidHvacAction(value.to_owned()))
    }
}

impl fmt::Display for HvacAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A single Home Assistant climate feature bit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClimateFeature {
    TargetTemperature,
    TargetTemperatureRange,
    TargetHumidity,
    FanMode,
    PresetMode,
    SwingMode,
    TurnOff,
    TurnOn,
    SwingHorizontalMode,
}

impl ClimateFeature {
    #[must_use]
    pub const fn bits(self) -> u64 {
        match self {
            Self::TargetTemperature => 1,
            Self::TargetTemperatureRange => 2,
            Self::TargetHumidity => 4,
            Self::FanMode => 8,
            Self::PresetMode => 16,
            Self::SwingMode => 32,
            Self::TurnOff => 128,
            Self::TurnOn => 256,
            Self::SwingHorizontalMode => 512,
        }
    }
}

/// Source-compatible bit mask for `ClimateEntityFeature`.
///
/// Unknown bits are retained, matching Python's `IntFlag` behavior. Bit 64 is
/// intentionally unused because it is absent from the pinned source enum.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ClimateFeatures(u64);

impl ClimateFeatures {
    pub const NONE: Self = Self(0);
    pub const TARGET_TEMPERATURE: Self = Self(1);
    pub const TARGET_TEMPERATURE_RANGE: Self = Self(2);
    pub const TARGET_HUMIDITY: Self = Self(4);
    pub const FAN_MODE: Self = Self(8);
    pub const PRESET_MODE: Self = Self(16);
    pub const SWING_MODE: Self = Self(32);
    pub const TURN_OFF: Self = Self(128);
    pub const TURN_ON: Self = Self(256);
    pub const SWING_HORIZONTAL_MODE: Self = Self(512);

    /// Construct a feature mask while retaining bits unknown to this revision.
    #[must_use]
    pub const fn from_bits_retain(bits: u64) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, features: Self) -> bool {
        self.0 & features.0 == features.0
    }

    #[must_use]
    pub const fn contains_feature(self, feature: ClimateFeature) -> bool {
        self.0 & feature.bits() == feature.bits()
    }

    #[must_use]
    pub const fn intersects(self, features: Self) -> bool {
        self.0 & features.0 != 0
    }

    #[must_use]
    pub const fn union(self, features: Self) -> Self {
        Self(self.0 | features.0)
    }

    #[must_use]
    pub const fn with(self, feature: ClimateFeature) -> Self {
        Self(self.0 | feature.bits())
    }

    #[must_use]
    pub const fn without(self, feature: ClimateFeature) -> Self {
        Self(self.0 & !feature.bits())
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl From<ClimateFeature> for ClimateFeatures {
    fn from(feature: ClimateFeature) -> Self {
        Self(feature.bits())
    }
}

impl BitOr for ClimateFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ClimateFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for ClimateFeatures {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// Temperature unit accepted by climate provider state and service values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClimateTemperatureUnit {
    Celsius,
    Fahrenheit,
    Kelvin,
}

impl ClimateTemperatureUnit {
    pub const ALL: [Self; 3] = [Self::Celsius, Self::Fahrenheit, Self::Kelvin];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Kelvin => "K",
        }
    }
}

impl fmt::Display for ClimateTemperatureUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ClimateTemperatureUnit {
    type Err = ClimateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|unit| unit.as_str() == value)
            .ok_or_else(|| ClimateError::InvalidTemperatureUnit(value.to_owned()))
    }
}

/// Core-owned global temperature presentation settings supplied by the host.
///
/// The provider's required native unit remains separate from this configured
/// display unit. The unit conversion itself uses `hass-units`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClimateUnitContext {
    /// Core's configured temperature unit, also used for state presentation.
    pub temperature_unit: ClimateTemperatureUnit,
    /// Optional precision override, matching the host's climate settings.
    pub precision: Option<f64>,
}

impl ClimateUnitContext {
    #[must_use]
    pub const fn new(temperature_unit: ClimateTemperatureUnit) -> Self {
        Self {
            temperature_unit,
            precision: None,
        }
    }

    #[must_use]
    pub const fn with_precision(mut self, precision: f64) -> Self {
        self.precision = Some(precision);
        self
    }
}

/// Errors from climate descriptor/state validation and service planning.
#[derive(Clone, Debug, PartialEq)]
pub enum ClimateError {
    InvalidTemperatureUnit(String),
    InvalidHvacMode(String),
    InvalidHvacAction(String),
    InvalidDescriptor(&'static str),
    InvalidNumber {
        field: String,
    },
    UnitConversion(String),
    WrongDomain(String),
    UnknownService(String),
    UnknownServiceField(String),
    UnknownTargetField(String),
    DuplicateTargetField(String),
    EntityIdInDataAndTarget,
    MissingArgument(&'static str),
    InvalidArgument {
        field: String,
        expected: &'static str,
    },
    InvalidTemperatureRange,
    TemperatureOutOfRange {
        field: String,
        value: f64,
        min: f64,
        max: f64,
    },
    HumidityOutOfRange {
        value: i64,
        min: f64,
        max: f64,
    },
    UnsupportedFeature {
        required: u64,
        supported: u64,
    },
    InvalidMode {
        family: &'static str,
        mode: String,
        available: Vec<String>,
    },
}

impl fmt::Display for ClimateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTemperatureUnit(unit) => {
                write!(formatter, "unsupported climate temperature unit {unit:?}")
            }
            Self::InvalidHvacMode(mode) => write!(formatter, "unknown HVAC mode {mode:?}"),
            Self::InvalidHvacAction(action) => write!(formatter, "unknown HVAC action {action:?}"),
            Self::InvalidDescriptor(reason) => {
                write!(formatter, "invalid climate descriptor: {reason}")
            }
            Self::InvalidNumber { field } => {
                write!(formatter, "climate field {field:?} must be a finite number")
            }
            Self::UnitConversion(reason) => {
                write!(formatter, "climate temperature conversion failed: {reason}")
            }
            Self::WrongDomain(domain) => {
                write!(formatter, "expected climate domain, got {domain:?}")
            }
            Self::UnknownService(service) => {
                write!(formatter, "unknown climate service {service:?}")
            }
            Self::UnknownServiceField(field) => {
                write!(formatter, "unknown climate service field {field:?}")
            }
            Self::UnknownTargetField(field) => {
                write!(formatter, "unknown climate target field {field:?}")
            }
            Self::DuplicateTargetField(field) => write!(
                formatter,
                "climate selector {field:?} is in both data and target"
            ),
            Self::EntityIdInDataAndTarget => {
                formatter.write_str("entity_id is present in both climate service data and target")
            }
            Self::MissingArgument(field) => {
                write!(formatter, "missing climate service argument {field:?}")
            }
            Self::InvalidArgument { field, expected } => write!(
                formatter,
                "climate service field {field:?} must be {expected}"
            ),
            Self::InvalidTemperatureRange => {
                formatter.write_str("target_temp_low must not exceed target_temp_high")
            }
            Self::TemperatureOutOfRange {
                field,
                value,
                min,
                max,
            } => write!(
                formatter,
                "climate temperature {field:?} value {value} is outside {min}..={max}"
            ),
            Self::HumidityOutOfRange { value, min, max } => write!(
                formatter,
                "climate humidity {value} is outside {min}..={max}"
            ),
            Self::UnsupportedFeature {
                required,
                supported,
            } => write!(
                formatter,
                "climate service requires feature bits {required}, supported bits are {supported}"
            ),
            Self::InvalidMode {
                family,
                mode,
                available,
            } => write!(
                formatter,
                "climate {family} mode {mode:?} is not in {available:?}"
            ),
        }
    }
}

impl std::error::Error for ClimateError {}

impl From<ConversionError> for ClimateError {
    fn from(error: ConversionError) -> Self {
        Self::UnitConversion(error.to_string())
    }
}

/// Stable provider description for one native climate entity.
///
/// Climate has no device-class enum in the pinned source. Provider capability
/// lists for fan, preset and swing modes remain arbitrary strings, as in Core.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimateDescriptor {
    spec: EntitySpec,
    features: ClimateFeatures,
    hvac_modes: Option<Vec<HvacMode>>,
    fan_modes: Option<Vec<String>>,
    preset_modes: Option<Vec<String>>,
    swing_modes: Option<Vec<String>>,
    swing_horizontal_modes: Option<Vec<String>>,
    native_temperature_unit: Option<ClimateTemperatureUnit>,
    precision: Option<f64>,
    min_temp: Option<f64>,
    max_temp: Option<f64>,
    min_humidity: f64,
    max_humidity: f64,
    target_temperature_step: Option<f64>,
    target_humidity_step: Option<i64>,
}

impl ClimateDescriptor {
    /// Create a climate descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        let spec = EntitySpec::new(CLIMATE_DOMAIN, platform, provider_id, provider_key)
            .with_unrecorded_attributes([
                attr::HVAC_MODES,
                attr::FAN_MODES,
                attr::SWING_MODES,
                attr::MIN_TEMP,
                attr::MAX_TEMP,
                attr::MIN_HUMIDITY,
                attr::MAX_HUMIDITY,
                attr::TARGET_HUMIDITY_STEP,
                attr::TARGET_TEMP_STEP,
                attr::PRESET_MODES,
            ]);
        Self {
            spec,
            features: ClimateFeatures::NONE,
            hvac_modes: None,
            fan_modes: None,
            preset_modes: None,
            swing_modes: None,
            swing_horizontal_modes: None,
            native_temperature_unit: None,
            precision: None,
            min_temp: None,
            max_temp: None,
            min_humidity: DEFAULT_MIN_HUMIDITY,
            max_humidity: DEFAULT_MAX_HUMIDITY,
            target_temperature_step: None,
            target_humidity_step: None,
        }
    }

    #[must_use]
    pub const fn features(&self) -> ClimateFeatures {
        self.features
    }

    /// Return the advertised HVAC modes; `None` differs from an empty list.
    #[must_use]
    pub fn hvac_modes(&self) -> Option<&[HvacMode]> {
        self.hvac_modes.as_deref()
    }

    #[must_use]
    pub fn fan_modes(&self) -> Option<&[String]> {
        self.fan_modes.as_deref()
    }

    #[must_use]
    pub fn preset_modes(&self) -> Option<&[String]> {
        self.preset_modes.as_deref()
    }

    #[must_use]
    pub fn swing_modes(&self) -> Option<&[String]> {
        self.swing_modes.as_deref()
    }

    #[must_use]
    pub fn swing_horizontal_modes(&self) -> Option<&[String]> {
        self.swing_horizontal_modes.as_deref()
    }

    #[must_use]
    pub const fn native_temperature_unit(&self) -> Option<ClimateTemperatureUnit> {
        self.native_temperature_unit
    }

    #[must_use]
    pub fn with_supported_features(mut self, features: ClimateFeatures) -> Self {
        self.features = features;
        self
    }

    #[must_use]
    pub fn with_feature(mut self, feature: ClimateFeature) -> Self {
        self.features = self.features.with(feature);
        self
    }

    #[must_use]
    pub fn without_feature(mut self, feature: ClimateFeature) -> Self {
        self.features = self.features.without(feature);
        self
    }

    /// Set the provider's currently supported HVAC modes in provider order.
    #[must_use]
    pub fn with_hvac_modes(mut self, modes: impl IntoIterator<Item = HvacMode>) -> Self {
        self.hvac_modes = Some(modes.into_iter().collect());
        self
    }

    /// Publish no HVAC mode list, matching the source property's nullable default.
    #[must_use]
    pub fn without_hvac_modes(mut self) -> Self {
        self.hvac_modes = None;
        self
    }

    /// Set provider-defined fan-mode strings. `None` is a supported null list.
    #[must_use]
    pub fn with_fan_modes(mut self, modes: Option<Vec<String>>) -> Self {
        self.fan_modes = modes;
        self
    }

    /// Set provider-defined preset strings. `None` is a supported null list.
    #[must_use]
    pub fn with_preset_modes(mut self, modes: Option<Vec<String>>) -> Self {
        self.preset_modes = modes;
        self
    }

    /// Set provider-defined vertical swing strings. `None` is a supported null list.
    #[must_use]
    pub fn with_swing_modes(mut self, modes: Option<Vec<String>>) -> Self {
        self.swing_modes = modes;
        self
    }

    /// Set provider-defined horizontal swing strings. `None` is a supported null list.
    #[must_use]
    pub fn with_swing_horizontal_modes(mut self, modes: Option<Vec<String>>) -> Self {
        self.swing_horizontal_modes = modes;
        self
    }

    #[must_use]
    pub fn with_native_temperature_unit(mut self, unit: ClimateTemperatureUnit) -> Self {
        self.native_temperature_unit = Some(unit);
        self
    }

    #[must_use]
    pub fn with_precision(mut self, precision: f64) -> Self {
        self.precision = Some(precision);
        self
    }

    #[must_use]
    pub fn with_temperature_limits(mut self, min: f64, max: f64) -> Self {
        self.min_temp = Some(min);
        self.max_temp = Some(max);
        self
    }

    #[must_use]
    pub fn with_min_temperature(mut self, min: f64) -> Self {
        self.min_temp = Some(min);
        self
    }

    #[must_use]
    pub fn with_max_temperature(mut self, max: f64) -> Self {
        self.max_temp = Some(max);
        self
    }

    #[must_use]
    pub fn with_humidity_limits(mut self, min: f64, max: f64) -> Self {
        self.min_humidity = min;
        self.max_humidity = max;
        self
    }

    #[must_use]
    pub fn with_target_temperature_step(mut self, step: Option<f64>) -> Self {
        self.target_temperature_step = step;
        self
    }

    #[must_use]
    pub fn with_target_humidity_step(mut self, step: Option<i64>) -> Self {
        self.target_humidity_step = step;
        self
    }

    #[must_use]
    pub fn with_unique_id(mut self, unique_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_unique_id(unique_id);
        self
    }

    #[must_use]
    pub fn with_config_entry_id(mut self, config_entry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_entry_id(config_entry_id);
        self
    }

    #[must_use]
    pub fn with_config_subentry_id(mut self, config_subentry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_subentry_id(config_subentry_id);
        self
    }

    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_device_id(device_id);
        self
    }

    #[must_use]
    pub fn with_area_id(mut self, area_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_area_id(area_id);
        self
    }

    #[must_use]
    pub fn with_suggested_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_suggested_object_id(object_id);
        self
    }

    #[must_use]
    pub fn with_prior_entity_id(mut self, entity_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_prior_entity_id(entity_id);
        self
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.spec.info.original_name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.spec.info.original_icon = Some(icon.into());
        self
    }

    #[must_use]
    pub fn with_entity_category(mut self, category: impl Into<String>) -> Self {
        self.spec.info.entity_category = Some(category.into());
        self
    }

    #[must_use]
    pub fn with_has_entity_name(mut self, has_entity_name: bool) -> Self {
        self.spec.info.has_entity_name = has_entity_name;
        self
    }

    #[must_use]
    pub fn with_translation_key(mut self, translation_key: impl Into<String>) -> Self {
        self.spec.info.translation_key = Some(translation_key.into());
        self
    }

    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Validate ranges/precision and return a lifecycle-ready entity spec.
    pub fn try_entity_spec(&self) -> Result<EntitySpec, ClimateError> {
        self.validate()?;
        Ok(self.entity_spec())
    }

    /// Build the entity spec, retaining the source's provider-owned feature
    /// bits and registry metadata. Call [`Self::try_entity_spec`] before
    /// registration when descriptor values are provider-controlled.
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        let mut spec = self.spec.clone();
        spec.info.supported_features = self.features.bits();
        spec.info.capabilities = Some(Value::Object(self.registry_capabilities()));
        spec
    }

    /// Build capability metadata in Core's configured display unit. The
    /// provider must establish its native temperature unit before registration.
    pub fn entity_spec_with_units(
        &self,
        units: ClimateUnitContext,
    ) -> Result<EntitySpec, ClimateError> {
        self.validate()?;
        let mut spec = self.spec.clone();
        spec.info.supported_features = self.features.bits();
        let mut capabilities = Map::new();
        for (name, value) in self.live_capabilities(units)? {
            if name != attr::SUPPORTED_FEATURES
                && let Some(value) = value
            {
                capabilities.insert(name, value);
            }
        }
        spec.info.capabilities = Some(Value::Object(capabilities));
        Ok(spec)
    }

    /// Project one provider snapshot into shared runtime state plus live
    /// capabilities. Capability removals use `EntityPublication::Remove`;
    /// ordinary state attributes stay availability-scoped.
    pub fn publication(
        &self,
        snapshot: &ClimateSnapshot,
        units: ClimateUnitContext,
    ) -> Result<EntityPublication, ClimateError> {
        self.validate()?;
        units.validate()?;
        let native_unit = self
            .native_temperature_unit
            .ok_or(ClimateError::InvalidDescriptor(
                "native_temperature_unit is required",
            ))?;
        let precision = self.effective_precision(units)?;

        let mut attributes = snapshot.extra_attributes.clone();
        if !self.features.is_empty() {
            attributes.insert(attr::SUPPORTED_FEATURES.into(), json!(self.features.bits()));
        }
        insert_number(
            &mut attributes,
            attr::CURRENT_TEMPERATURE,
            display_temperature(
                snapshot.current_temperature,
                native_unit,
                units.temperature_unit,
                precision,
            )?,
            true,
        )?;
        attributes.insert(
            attr::TEMPERATURE_UNIT.into(),
            Value::String(units.temperature_unit.as_str().to_owned()),
        );

        if self.features.contains(ClimateFeatures::TARGET_TEMPERATURE) {
            insert_number(
                &mut attributes,
                attr::TARGET_TEMPERATURE,
                display_temperature(
                    snapshot.target_temperature,
                    native_unit,
                    units.temperature_unit,
                    precision,
                )?,
                true,
            )?;
        }
        if self
            .features
            .contains(ClimateFeatures::TARGET_TEMPERATURE_RANGE)
        {
            insert_number(
                &mut attributes,
                attr::TARGET_TEMP_HIGH,
                display_temperature(
                    snapshot.target_temperature_high,
                    native_unit,
                    units.temperature_unit,
                    precision,
                )?,
                true,
            )?;
            insert_number(
                &mut attributes,
                attr::TARGET_TEMP_LOW,
                display_temperature(
                    snapshot.target_temperature_low,
                    native_unit,
                    units.temperature_unit,
                    precision,
                )?,
                true,
            )?;
        }

        if let Some(value) = snapshot.current_humidity {
            insert_number(&mut attributes, attr::CURRENT_HUMIDITY, Some(value), false)?;
        }
        if self.features.contains(ClimateFeatures::TARGET_HUMIDITY) {
            insert_number(
                &mut attributes,
                attr::TARGET_HUMIDITY,
                snapshot.target_humidity,
                true,
            )?;
        }
        if self.features.contains(ClimateFeatures::FAN_MODE) {
            insert_optional_string(
                &mut attributes,
                attr::FAN_MODE,
                snapshot.fan_mode.as_deref(),
            );
        }
        if let Some(action) = snapshot.hvac_action {
            attributes.insert(
                attr::HVAC_ACTION.into(),
                Value::String(action.as_str().to_owned()),
            );
        }
        if self.features.contains(ClimateFeatures::PRESET_MODE) {
            insert_optional_string(
                &mut attributes,
                attr::PRESET_MODE,
                snapshot.preset_mode.as_deref(),
            );
        }
        if self.features.contains(ClimateFeatures::SWING_MODE) {
            insert_optional_string(
                &mut attributes,
                attr::SWING_MODE,
                snapshot.swing_mode.as_deref(),
            );
        }
        if self
            .features
            .contains(ClimateFeatures::SWING_HORIZONTAL_MODE)
        {
            insert_optional_string(
                &mut attributes,
                attr::SWING_HORIZONTAL_MODE,
                snapshot.swing_horizontal_mode.as_deref(),
            );
        }

        let entity_snapshot = EntitySnapshot {
            state: snapshot.hvac_mode.map(HvacMode::as_str).map(str::to_owned),
            available: snapshot.available,
            attributes,
            context: snapshot.context.clone(),
        };
        let mut publication = EntityPublication::from(entity_snapshot);
        for (name, value) in self.live_capabilities(units)? {
            publication = publication.with_live_attribute(name, value);
        }
        Ok(publication)
    }

    /// Validate provider-supplied ranges and precision before registration or
    /// publication.
    pub fn validate(&self) -> Result<(), ClimateError> {
        if self.native_temperature_unit.is_none() {
            return Err(ClimateError::InvalidDescriptor(
                "native_temperature_unit is required",
            ));
        }
        for (field, value) in [
            ("min_temp", self.min_temp),
            ("max_temp", self.max_temp),
            ("min_humidity", Some(self.min_humidity)),
            ("max_humidity", Some(self.max_humidity)),
            ("target_temp_step", self.target_temperature_step),
        ] {
            if value.is_some_and(|value| !value.is_finite()) {
                return Err(ClimateError::InvalidNumber {
                    field: field.to_owned(),
                });
            }
        }
        if self
            .min_temp
            .zip(self.max_temp)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ClimateError::InvalidDescriptor("min_temp exceeds max_temp"));
        }
        if self.min_humidity > self.max_humidity {
            return Err(ClimateError::InvalidDescriptor(
                "min_humidity exceeds max_humidity",
            ));
        }
        if self
            .precision
            .is_some_and(|precision| !precision.is_finite() || precision <= 0.0)
        {
            return Err(ClimateError::InvalidDescriptor(
                "precision must be finite and positive",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_mode(
        &self,
        family: &'static str,
        mode: &str,
    ) -> Result<(), ClimateError> {
        let available = match family {
            "hvac" => self
                .hvac_modes
                .as_ref()
                .into_iter()
                .flatten()
                .map(|mode| mode.as_str().to_owned())
                .collect(),
            "fan" => self.fan_modes.clone().unwrap_or_default(),
            "preset" => self.preset_modes.clone().unwrap_or_default(),
            "swing" => self.swing_modes.clone().unwrap_or_default(),
            "horizontal_swing" => self.swing_horizontal_modes.clone().unwrap_or_default(),
            _ => Vec::new(),
        };
        if available.iter().any(|value| value == mode) {
            return Ok(());
        }
        Err(ClimateError::InvalidMode {
            family,
            mode: mode.to_owned(),
            available,
        })
    }

    pub(crate) fn temperature_limits(
        &self,
        units: ClimateUnitContext,
    ) -> Result<(f64, f64, ClimateTemperatureUnit, f64), ClimateError> {
        units.validate()?;
        let native = self
            .native_temperature_unit
            .ok_or(ClimateError::InvalidDescriptor(
                "native_temperature_unit is required",
            ))?;
        let default_min = convert(
            UnitFamily::Temperature,
            DEFAULT_MIN_TEMPERATURE_CELSIUS,
            Some(ClimateTemperatureUnit::Celsius.as_str()),
            Some(native.as_str()),
        )?;
        let default_max = convert(
            UnitFamily::Temperature,
            DEFAULT_MAX_TEMPERATURE_CELSIUS,
            Some(ClimateTemperatureUnit::Celsius.as_str()),
            Some(native.as_str()),
        )?;
        let min = self.min_temp.unwrap_or(default_min);
        let max = self.max_temp.unwrap_or(default_max);
        if min > max {
            return Err(ClimateError::InvalidDescriptor("min_temp exceeds max_temp"));
        }
        Ok((min, max, native, self.effective_precision(units)?))
    }

    fn effective_precision(&self, units: ClimateUnitContext) -> Result<f64, ClimateError> {
        let precision = self.precision.or(units.precision).unwrap_or_else(|| {
            if units.temperature_unit == ClimateTemperatureUnit::Celsius {
                0.1
            } else {
                1.0
            }
        });
        if !precision.is_finite() || precision <= 0.0 {
            return Err(ClimateError::InvalidDescriptor(
                "precision must be finite and positive",
            ));
        }
        Ok(precision)
    }

    fn live_capabilities(
        &self,
        units: ClimateUnitContext,
    ) -> Result<Vec<(String, Option<Value>)>, ClimateError> {
        let (min_temp, max_temp, native, precision) = self.temperature_limits(units)?;
        let min_display =
            display_temperature(Some(min_temp), native, units.temperature_unit, precision)?
                .expect("a present temperature remains present after conversion");
        let max_display =
            display_temperature(Some(max_temp), native, units.temperature_unit, precision)?
                .expect("a present temperature remains present after conversion");
        let mut output = vec![
            (
                attr::HVAC_MODES,
                Some(self.hvac_modes.as_ref().map_or(Value::Null, |modes| {
                    json!(modes.iter().map(|mode| mode.as_str()).collect::<Vec<_>>())
                })),
            ),
            (attr::MIN_TEMP, Some(json!(min_display))),
            (attr::MAX_TEMP, Some(json!(max_display))),
            (
                attr::SUPPORTED_FEATURES,
                (self.features.bits() != 0).then(|| json!(self.features.bits())),
            ),
        ];
        output.push((
            attr::TARGET_TEMP_STEP,
            self.target_temperature_step
                .filter(|step| *step != 0.0)
                .map(|step| json!(step)),
        ));
        self.push_capability(
            &mut output,
            ClimateFeatures::TARGET_HUMIDITY,
            attr::MIN_HUMIDITY,
            Some(json!(self.min_humidity)),
        );
        self.push_capability(
            &mut output,
            ClimateFeatures::TARGET_HUMIDITY,
            attr::MAX_HUMIDITY,
            Some(json!(self.max_humidity)),
        );
        self.push_capability(
            &mut output,
            ClimateFeatures::TARGET_HUMIDITY,
            attr::TARGET_HUMIDITY_STEP,
            self.target_humidity_step.map(|step| json!(step)),
        );
        self.push_list_capability(
            &mut output,
            ClimateFeatures::FAN_MODE,
            attr::FAN_MODES,
            &self.fan_modes,
        );
        self.push_list_capability(
            &mut output,
            ClimateFeatures::PRESET_MODE,
            attr::PRESET_MODES,
            &self.preset_modes,
        );
        self.push_list_capability(
            &mut output,
            ClimateFeatures::SWING_MODE,
            attr::SWING_MODES,
            &self.swing_modes,
        );
        self.push_list_capability(
            &mut output,
            ClimateFeatures::SWING_HORIZONTAL_MODE,
            attr::SWING_HORIZONTAL_MODES,
            &self.swing_horizontal_modes,
        );
        Ok(output
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect())
    }

    fn push_capability(
        &self,
        output: &mut Vec<(&'static str, Option<Value>)>,
        feature: ClimateFeatures,
        name: &'static str,
        value: Option<Value>,
    ) {
        output.push((
            name,
            self.features.contains(feature).then_some(value).flatten(),
        ));
    }

    fn push_list_capability(
        &self,
        output: &mut Vec<(&'static str, Option<Value>)>,
        feature: ClimateFeatures,
        name: &'static str,
        values: &Option<Vec<String>>,
    ) {
        output.push((
            name,
            self.features
                .contains(feature)
                .then(|| values.as_ref().map_or(Value::Null, |values| json!(values))),
        ));
    }

    fn registry_capabilities(&self) -> Map<String, Value> {
        let mut output = Map::new();
        output.insert(
            attr::HVAC_MODES.into(),
            self.hvac_modes.as_ref().map_or(Value::Null, |modes| {
                json!(modes.iter().map(|mode| mode.as_str()).collect::<Vec<_>>())
            }),
        );
        if let Some(native) = self.native_temperature_unit {
            let min = self.min_temp.or_else(|| {
                convert(
                    UnitFamily::Temperature,
                    DEFAULT_MIN_TEMPERATURE_CELSIUS,
                    Some(ClimateTemperatureUnit::Celsius.as_str()),
                    Some(native.as_str()),
                )
                .ok()
            });
            let max = self.max_temp.or_else(|| {
                convert(
                    UnitFamily::Temperature,
                    DEFAULT_MAX_TEMPERATURE_CELSIUS,
                    Some(ClimateTemperatureUnit::Celsius.as_str()),
                    Some(native.as_str()),
                )
                .ok()
            });
            if let Some(min) = min {
                output.insert(attr::MIN_TEMP.into(), json!(min));
            }
            if let Some(max) = max {
                output.insert(attr::MAX_TEMP.into(), json!(max));
            }
        } else {
            if let Some(min) = self.min_temp {
                output.insert(attr::MIN_TEMP.into(), json!(min));
            }
            if let Some(max) = self.max_temp {
                output.insert(attr::MAX_TEMP.into(), json!(max));
            }
        }
        if self.target_temperature_step.is_some_and(|step| step != 0.0) {
            output.insert(
                attr::TARGET_TEMP_STEP.into(),
                json!(self.target_temperature_step),
            );
        }
        if self.features.contains(ClimateFeatures::TARGET_HUMIDITY) {
            output.insert(attr::MIN_HUMIDITY.into(), json!(self.min_humidity));
            output.insert(attr::MAX_HUMIDITY.into(), json!(self.max_humidity));
            if let Some(step) = self.target_humidity_step {
                output.insert(attr::TARGET_HUMIDITY_STEP.into(), json!(step));
            }
        }
        if self.features.contains(ClimateFeatures::FAN_MODE) {
            output.insert(attr::FAN_MODES.into(), json!(self.fan_modes));
        }
        if self.features.contains(ClimateFeatures::PRESET_MODE) {
            output.insert(attr::PRESET_MODES.into(), json!(self.preset_modes));
        }
        if self.features.contains(ClimateFeatures::SWING_MODE) {
            output.insert(attr::SWING_MODES.into(), json!(self.swing_modes));
        }
        if self
            .features
            .contains(ClimateFeatures::SWING_HORIZONTAL_MODE)
        {
            output.insert(
                attr::SWING_HORIZONTAL_MODES.into(),
                json!(self.swing_horizontal_modes),
            );
        }
        output
    }
}

/// Current provider-reported climate state and ordinary attributes.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimateSnapshot {
    /// Provider availability. The shared lifecycle publishes `unavailable`.
    pub available: bool,
    /// Current operation mode; `None` projects to Home Assistant `unknown`.
    pub hvac_mode: Option<HvacMode>,
    /// Current action. `None` omits the `hvac_action` state attribute.
    pub hvac_action: Option<HvacAction>,
    /// Current and target temperatures in the descriptor's native unit.
    pub current_temperature: Option<f64>,
    pub target_temperature: Option<f64>,
    pub target_temperature_high: Option<f64>,
    pub target_temperature_low: Option<f64>,
    /// Current and target humidity percentages.
    pub current_humidity: Option<f64>,
    pub target_humidity: Option<f64>,
    /// Provider-defined mode values. Core validates these against descriptor lists.
    pub fan_mode: Option<String>,
    pub preset_mode: Option<String>,
    pub swing_mode: Option<String>,
    pub swing_horizontal_mode: Option<String>,
    /// Additional ordinary state attributes, omitted by the lifecycle offline.
    pub extra_attributes: Map<String, Value>,
    /// Causal context copied to the shared runtime state and event.
    pub context: Context,
}

impl ClimateSnapshot {
    /// Create an available snapshot with the reported HVAC mode.
    #[must_use]
    pub fn new(hvac_mode: Option<HvacMode>) -> Self {
        Self {
            available: true,
            hvac_mode,
            ..Self::default()
        }
    }

    /// Create an available state whose mode is unknown.
    #[must_use]
    pub fn unknown() -> Self {
        Self::new(None)
    }

    /// Create an unavailable provider snapshot.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_hvac_action(mut self, action: Option<HvacAction>) -> Self {
        self.hvac_action = action;
        self
    }

    #[must_use]
    pub fn with_current_temperature(mut self, temperature: Option<f64>) -> Self {
        self.current_temperature = temperature;
        self
    }

    #[must_use]
    pub fn with_target_temperature(mut self, temperature: Option<f64>) -> Self {
        self.target_temperature = temperature;
        self
    }

    #[must_use]
    pub fn with_target_temperature_range(mut self, low: Option<f64>, high: Option<f64>) -> Self {
        self.target_temperature_low = low;
        self.target_temperature_high = high;
        self
    }

    #[must_use]
    pub fn with_current_humidity(mut self, humidity: Option<f64>) -> Self {
        self.current_humidity = humidity;
        self
    }

    #[must_use]
    pub fn with_target_humidity(mut self, humidity: Option<f64>) -> Self {
        self.target_humidity = humidity;
        self
    }

    #[must_use]
    pub fn with_fan_mode(mut self, mode: Option<impl Into<String>>) -> Self {
        self.fan_mode = mode.map(Into::into);
        self
    }

    #[must_use]
    pub fn with_preset_mode(mut self, mode: Option<impl Into<String>>) -> Self {
        self.preset_mode = mode.map(Into::into);
        self
    }

    #[must_use]
    pub fn with_swing_mode(mut self, mode: Option<impl Into<String>>) -> Self {
        self.swing_mode = mode.map(Into::into);
        self
    }

    #[must_use]
    pub fn with_swing_horizontal_mode(mut self, mode: Option<impl Into<String>>) -> Self {
        self.swing_horizontal_mode = mode.map(Into::into);
        self
    }

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

    #[must_use]
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    /// Project provider state through the typed descriptor.
    pub fn project(
        &self,
        descriptor: &ClimateDescriptor,
        units: ClimateUnitContext,
    ) -> Result<EntityPublication, ClimateError> {
        descriptor.publication(self, units)
    }
}

impl Default for ClimateSnapshot {
    fn default() -> Self {
        Self {
            available: true,
            hvac_mode: None,
            hvac_action: None,
            current_temperature: None,
            target_temperature: None,
            target_temperature_high: None,
            target_temperature_low: None,
            current_humidity: None,
            target_humidity: None,
            fan_mode: None,
            preset_mode: None,
            swing_mode: None,
            swing_horizontal_mode: None,
            extra_attributes: Map::new(),
            context: Context::default(),
        }
    }
}

impl Default for ClimateUnitContext {
    fn default() -> Self {
        Self::new(ClimateTemperatureUnit::Celsius)
    }
}

impl ClimateUnitContext {
    fn validate(self) -> Result<(), ClimateError> {
        if self
            .precision
            .is_some_and(|precision| !precision.is_finite() || precision <= 0.0)
        {
            return Err(ClimateError::InvalidDescriptor(
                "unit-context precision must be finite and positive",
            ));
        }
        Ok(())
    }
}

fn display_temperature(
    temperature: Option<f64>,
    native: ClimateTemperatureUnit,
    display: ClimateTemperatureUnit,
    precision: f64,
) -> Result<Option<f64>, ClimateError> {
    let Some(temperature) = temperature else {
        return Ok(None);
    };
    if !temperature.is_finite() {
        return Err(ClimateError::InvalidNumber {
            field: "temperature".to_owned(),
        });
    }
    let converted = convert(
        UnitFamily::Temperature,
        temperature,
        Some(native.as_str()),
        Some(display.as_str()),
    )?;
    if !converted.is_finite() {
        return Err(ClimateError::InvalidNumber {
            field: "temperature".to_owned(),
        });
    }
    Ok(Some(round_temperature(converted, precision)))
}

pub(crate) fn round_temperature(value: f64, precision: f64) -> f64 {
    if precision == 0.5 {
        (value * 2.0).round_ties_even() / 2.0
    } else if precision == 0.1 {
        (value * 10.0).round_ties_even() / 10.0
    } else {
        value.round_ties_even()
    }
}

fn insert_number(
    attributes: &mut Map<String, Value>,
    name: &str,
    value: Option<f64>,
    include_null: bool,
) -> Result<(), ClimateError> {
    let Some(value) = value else {
        if include_null {
            attributes.insert(name.into(), Value::Null);
        }
        return Ok(());
    };
    let number =
        serde_json::Number::from_f64(value).ok_or_else(|| ClimateError::InvalidNumber {
            field: name.to_owned(),
        })?;
    attributes.insert(name.into(), Value::Number(number));
    Ok(())
}

fn insert_optional_string(attributes: &mut Map<String, Value>, name: &str, value: Option<&str>) {
    attributes.insert(
        name.into(),
        value.map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
}
