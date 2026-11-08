//! Native Home Assistant fan entities and their service-to-provider commands.
//!
//! This crate models the pinned fan domain without owning a device client.
//! `FanCommand::resolve` returns ordered, typed provider effects for an adapter
//! to execute; it does not claim that a fan was changed.

mod service;

pub use service::{
    FAN_TARGET_FIELDS, FanCommand, FanCommandPlan, FanEffect, FanService, FanServiceRequest,
};

use hass_core::Context;
use hass_entity::{EntityPublication, EntitySnapshot, EntitySpec};
use serde_json::{Map, Number, Value, json};
use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};
use std::str::FromStr;

/// Home Assistant entity domain.
pub const FAN_DOMAIN: &str = "fan";

/// Home Assistant fan state strings.
pub const FAN_STATE_ON: &str = "on";
pub const FAN_STATE_OFF: &str = "off";

/// Fan state and capability attribute keys at the pinned revision.
pub const FAN_ATTR_PERCENTAGE: &str = "percentage";
pub const FAN_ATTR_PERCENTAGE_STEP: &str = "percentage_step";
pub const FAN_ATTR_OSCILLATING: &str = "oscillating";
pub const FAN_ATTR_DIRECTION: &str = "direction";
pub const FAN_ATTR_PRESET_MODE: &str = "preset_mode";
pub const FAN_ATTR_PRESET_MODES: &str = "preset_modes";

/// Direction strings exposed by the fan selector.
pub const FAN_DIRECTION_FORWARD: &str = "forward";
pub const FAN_DIRECTION_REVERSE: &str = "reverse";

/// Feature flags copied from `FanEntityFeature` at the pinned revision.
///
/// Unknown bits are retained, matching Python's `IntFlag` behavior. The fan
/// domain has no device-class enum in the pinned source.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct FanFeatures(u64);

impl FanFeatures {
    pub const SET_SPEED: Self = Self(1);
    pub const OSCILLATE: Self = Self(2);
    pub const DIRECTION: Self = Self(4);
    pub const PRESET_MODE: Self = Self(8);
    pub const TURN_OFF: Self = Self(16);
    pub const TURN_ON: Self = Self(32);

    /// Construct a feature set while retaining unknown source bits.
    #[must_use]
    pub const fn from_bits_retain(bits: u64) -> Self {
        Self(bits)
    }

    /// Return the underlying Home Assistant feature bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every bit in `features` is present.
    #[must_use]
    pub const fn contains(self, features: Self) -> bool {
        self.0 & features.0 == features.0
    }

    /// Whether at least one bit in `features` is present.
    #[must_use]
    pub const fn intersects(self, features: Self) -> bool {
        self.0 & features.0 != 0
    }

    #[must_use]
    pub const fn union(self, features: Self) -> Self {
        Self(self.0 | features.0)
    }

    /// Whether no feature bits are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for FanFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FanFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for FanFeatures {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// Typed state emitted by a fan entity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FanState {
    On,
    Off,
    Unknown,
}

impl FanState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => FAN_STATE_ON,
            Self::Off => FAN_STATE_OFF,
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FanState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The two direction values offered by Home Assistant's fan selector.
///
/// `set_direction` itself accepts any string in the pinned service validator;
/// this enum is a convenience for providers that expose the standard values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FanDirection {
    Forward,
    Reverse,
}

impl FanDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Forward => FAN_DIRECTION_FORWARD,
            Self::Reverse => FAN_DIRECTION_REVERSE,
        }
    }
}

impl FromStr for FanDirection {
    type Err = FanError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            FAN_DIRECTION_FORWARD => Ok(Self::Forward),
            FAN_DIRECTION_REVERSE => Ok(Self::Reverse),
            _ => Err(FanError::UnknownDirection(value.to_owned())),
        }
    }
}

/// SDK errors for projection, conversion, and fan service validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FanError {
    EmptyOrderedList,
    SpeedItemNotFound,
    InvalidRange,
    NonFiniteValue,
    NumericOverflow,
    ZeroSpeedCount,
    UnknownDirection(String),
    WrongDomain(String),
    UnknownService(String),
    UnknownServiceField(String),
    UnknownTargetField(String),
    EntityIdInDataAndTarget,
    MissingArgument(&'static str),
    InvalidArgument {
        field: String,
        expected: &'static str,
    },
    PercentageOutOfRange {
        field: String,
        value: i64,
    },
    UnsupportedFeature {
        required: u64,
        supported: u64,
    },
    InvalidPresetMode {
        preset_mode: String,
        available_modes: Vec<String>,
    },
}

impl fmt::Display for FanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyOrderedList => formatter.write_str("fan speed list is empty"),
            Self::SpeedItemNotFound => formatter.write_str("fan speed is not in the ordered list"),
            Self::InvalidRange => {
                formatter.write_str("fan speed range must be finite and ascending")
            }
            Self::NonFiniteValue => formatter.write_str("fan percentage value must be finite"),
            Self::NumericOverflow => formatter.write_str("fan percentage conversion overflowed"),
            Self::ZeroSpeedCount => formatter.write_str("fan speed count must not be zero"),
            Self::UnknownDirection(direction) => {
                write!(formatter, "unknown standard fan direction {direction:?}")
            }
            Self::WrongDomain(domain) => {
                write!(formatter, "expected fan service domain, got {domain:?}")
            }
            Self::UnknownService(service) => write!(formatter, "unknown fan service {service:?}"),
            Self::UnknownServiceField(field) => {
                write!(formatter, "unknown fan service field {field:?}")
            }
            Self::UnknownTargetField(field) => {
                write!(formatter, "unknown fan target field {field:?}")
            }
            Self::EntityIdInDataAndTarget => {
                formatter.write_str("entity_id is present in both service data and target")
            }
            Self::MissingArgument(field) => {
                write!(formatter, "missing fan service argument {field:?}")
            }
            Self::InvalidArgument { field, expected } => {
                write!(formatter, "fan service field {field:?} must be {expected}")
            }
            Self::PercentageOutOfRange { field, value } => {
                write!(
                    formatter,
                    "fan service field {field:?} percentage {value} is outside 0..100"
                )
            }
            Self::UnsupportedFeature {
                required,
                supported,
            } => write!(
                formatter,
                "fan service requires feature bits {required}, supported bits are {supported}"
            ),
            Self::InvalidPresetMode {
                preset_mode,
                available_modes,
            } => write!(
                formatter,
                "fan preset mode {preset_mode:?} is not one of {available_modes:?}"
            ),
        }
    }
}

impl std::error::Error for FanError {}

/// Provider-facing fan metadata and static capabilities.
///
/// State values live in [`FanSnapshot`]. Preset modes remain provider
/// capability metadata when a snapshot is unavailable.
#[derive(Clone, Debug, PartialEq)]
pub struct FanDescriptor {
    spec: EntitySpec,
    features: FanFeatures,
    speed_count: i64,
    preset_modes: Option<Vec<String>>,
}

impl FanDescriptor {
    /// Create a fan descriptor with the Home Assistant defaults.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            spec: EntitySpec::new(FAN_DOMAIN, platform, provider_id, provider_key),
            features: FanFeatures::default(),
            speed_count: 100,
            preset_modes: None,
        }
    }

    #[must_use]
    pub const fn features(&self) -> FanFeatures {
        self.features
    }

    #[must_use]
    pub const fn speed_count(&self) -> i64 {
        self.speed_count
    }

    #[must_use]
    pub fn preset_modes(&self) -> Option<&[String]> {
        self.preset_modes.as_deref()
    }

    #[must_use]
    pub fn with_features(mut self, features: FanFeatures) -> Self {
        self.features = features;
        self
    }

    #[must_use]
    pub fn with_feature(mut self, feature: FanFeatures) -> Self {
        self.features |= feature;
        self
    }

    #[must_use]
    pub fn without_feature(mut self, feature: FanFeatures) -> Self {
        self.features = FanFeatures::from_bits_retain(self.features.bits() & !feature.bits());
        self
    }

    /// Set the provider's number of non-off speed steps.
    ///
    /// The upstream entity stores this value without validating it. A zero
    /// count therefore produces [`FanError::ZeroSpeedCount`] when a percentage
    /// step or implicit step change needs to use it.
    #[must_use]
    pub const fn with_speed_count(mut self, speed_count: i64) -> Self {
        self.speed_count = speed_count;
        self
    }

    /// Set the preset-mode capability list; `None` is published as JSON null
    /// when preset modes are supported.
    #[must_use]
    pub fn with_preset_modes(mut self, preset_modes: Option<Vec<String>>) -> Self {
        self.preset_modes = preset_modes;
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
    pub fn with_suggested_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_suggested_object_id(object_id);
        self
    }

    #[must_use]
    pub fn with_entity_category(mut self, category: impl Into<String>) -> Self {
        self.spec.info.entity_category = Some(category.into());
        self
    }

    #[must_use]
    pub fn with_translation_key(mut self, translation_key: impl Into<String>) -> Self {
        self.spec.info.translation_key = Some(translation_key.into());
        self
    }

    #[must_use]
    pub fn with_entity_name(mut self, has_entity_name: bool) -> Self {
        self.spec.info.has_entity_name = has_entity_name;
        self
    }

    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Build a lifecycle-ready entity specification with live provider metadata.
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        let mut spec = self
            .spec
            .clone()
            .with_unrecorded_attributes([FAN_ATTR_PRESET_MODES]);
        spec.info.supported_features = self.features.bits();
        spec.info.capabilities = self.capability_metadata();
        spec
    }

    /// Consume the descriptor and build its lifecycle-ready entity specification.
    #[must_use]
    pub fn into_entity_spec(self) -> EntitySpec {
        self.entity_spec()
    }

    /// Convert the domain snapshot and keep preset-mode capabilities live while
    /// unavailable. User registry metadata overrides are resolved by the shared
    /// lifecycle, not copied into this publication.
    pub fn publication(&self, snapshot: &FanSnapshot) -> Result<EntityPublication, FanError> {
        let entity_snapshot = snapshot.entity_snapshot(self)?;
        let live_modes = self.supports_preset_modes().then(|| {
            self.preset_modes
                .as_ref()
                .map_or(Value::Null, |modes| json!(modes))
        });
        Ok(EntityPublication::from(entity_snapshot)
            .with_live_attribute(FAN_ATTR_PRESET_MODES, live_modes))
    }

    /// The fan speed step, matching `100 / speed_count` in the source entity.
    pub fn percentage_step(&self) -> Result<f64, FanError> {
        if self.speed_count == 0 {
            return Err(FanError::ZeroSpeedCount);
        }
        Ok(100.0 / self.speed_count as f64)
    }

    /// Validate a preset exactly against the provider's current list.
    pub fn validate_preset_mode(&self, preset_mode: &str) -> Result<(), FanError> {
        if self
            .preset_modes
            .as_ref()
            .is_some_and(|modes| modes.iter().any(|mode| mode == preset_mode))
        {
            return Ok(());
        }
        Err(FanError::InvalidPresetMode {
            preset_mode: preset_mode.to_owned(),
            available_modes: self.preset_modes.clone().unwrap_or_default(),
        })
    }

    fn supports_preset_modes(&self) -> bool {
        self.features
            .intersects(FanFeatures::SET_SPEED | FanFeatures::PRESET_MODE)
    }

    fn capability_metadata(&self) -> Option<Value> {
        if !self.supports_preset_modes() {
            return None;
        }
        let mut attributes = Map::new();
        attributes.insert(
            FAN_ATTR_PRESET_MODES.to_owned(),
            self.preset_modes
                .as_ref()
                .map_or(Value::Null, |modes| json!(modes)),
        );
        Some(Value::Object(attributes))
    }
}

/// One provider observation of a fan's state and ordinary state attributes.
///
/// A missing percentage differs from zero: `None` is published as JSON null
/// when speed is supported, while zero is the numeric off value.
#[derive(Clone, Debug, PartialEq)]
pub struct FanSnapshot {
    reported_state: Option<FanState>,
    percentage: Option<i64>,
    preset_mode: Option<String>,
    current_direction: Option<String>,
    oscillating: Option<bool>,
    available: bool,
    extra_attributes: Map<String, Value>,
    context: Context,
}

impl Default for FanSnapshot {
    fn default() -> Self {
        Self {
            reported_state: None,
            percentage: Some(0),
            preset_mode: None,
            current_direction: None,
            oscillating: None,
            available: true,
            extra_attributes: Map::new(),
            context: Context::default(),
        }
    }
}

impl FanSnapshot {
    /// Create the Home Assistant base-entity defaults: available and off at 0%.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an unavailable fan observation with default cached values.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn percentage(&self) -> Option<i64> {
        self.percentage
    }

    #[must_use]
    pub fn preset_mode(&self) -> Option<&str> {
        self.preset_mode.as_deref()
    }

    #[must_use]
    pub fn current_direction(&self) -> Option<&str> {
        self.current_direction.as_deref()
    }

    #[must_use]
    pub const fn oscillating(&self) -> Option<bool> {
        self.oscillating
    }

    #[must_use]
    pub const fn available(&self) -> bool {
        self.available
    }

    /// Use a provider's explicit power state instead of deriving it from speed
    /// and preset values. This supports providers with an independent on/off topic.
    #[must_use]
    pub fn with_reported_state(mut self, state: FanState) -> Self {
        self.reported_state = Some(state);
        self
    }

    #[must_use]
    pub fn with_percentage(mut self, percentage: Option<i64>) -> Self {
        self.percentage = percentage;
        self
    }

    #[must_use]
    pub fn with_preset_mode(mut self, preset_mode: Option<String>) -> Self {
        self.preset_mode = preset_mode;
        self
    }

    #[must_use]
    pub fn with_current_direction(mut self, direction: Option<String>) -> Self {
        self.current_direction = direction;
        self
    }

    #[must_use]
    pub fn with_oscillating(mut self, oscillating: Option<bool>) -> Self {
        self.oscillating = oscillating;
        self
    }

    #[must_use]
    pub const fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Set or remove one availability-scoped extra state attribute. `Some(null)`
    /// remains explicit JSON null; `None` removes the key.
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

    /// Project this fan observation to Home Assistant's `on`/`off` state.
    /// A reported provider state takes priority. Otherwise, a non-null preset
    /// mode counts as on even when percentage is null or 0.
    #[must_use]
    pub fn state(&self) -> FanState {
        if let Some(state) = self.reported_state {
            return state;
        }
        if self.percentage.is_some_and(|percentage| percentage > 0) || self.preset_mode.is_some() {
            FanState::On
        } else {
            FanState::Off
        }
    }

    /// Project feature-gated standard attributes plus provider extras.
    pub fn state_attributes(
        &self,
        descriptor: &FanDescriptor,
    ) -> Result<Map<String, Value>, FanError> {
        let mut attributes = self.extra_attributes.clone();
        let features = descriptor.features;

        if features.contains(FanFeatures::DIRECTION) {
            attributes.insert(
                FAN_ATTR_DIRECTION.into(),
                self.current_direction
                    .as_ref()
                    .map_or(Value::Null, |value| json!(value)),
            );
        }
        if features.contains(FanFeatures::OSCILLATE) {
            attributes.insert(
                FAN_ATTR_OSCILLATING.into(),
                self.oscillating.map_or(Value::Null, |value| json!(value)),
            );
        }

        let has_set_speed = features.contains(FanFeatures::SET_SPEED);
        if has_set_speed {
            let step = descriptor.percentage_step()?;
            let step = Number::from_f64(step).ok_or(FanError::NonFiniteValue)?;
            attributes.insert(
                FAN_ATTR_PERCENTAGE.into(),
                self.percentage.map_or(Value::Null, |value| json!(value)),
            );
            attributes.insert(FAN_ATTR_PERCENTAGE_STEP.into(), Value::Number(step));
        }
        if has_set_speed || features.contains(FanFeatures::PRESET_MODE) {
            attributes.insert(
                FAN_ATTR_PRESET_MODE.into(),
                self.preset_mode
                    .as_ref()
                    .map_or(Value::Null, |value| json!(value)),
            );
        }
        Ok(attributes)
    }

    /// Build the shared entity snapshot. Ordinary state attributes remain
    /// availability-scoped when the shared lifecycle publishes it.
    pub fn entity_snapshot(&self, descriptor: &FanDescriptor) -> Result<EntitySnapshot, FanError> {
        Ok(EntitySnapshot {
            state: Some(self.state().as_str().to_owned()),
            available: self.available,
            attributes: self.state_attributes(descriptor)?,
            context: self.context.clone(),
        })
    }
}

/// Convert an ordered speed into its one-based integer percentage.
///
/// The integer division floors, matching Home Assistant's ordered-list helper.
pub fn ordered_list_item_to_percentage<T: PartialEq>(
    ordered_list: &[T],
    item: &T,
) -> Result<i64, FanError> {
    let Some(index) = ordered_list.iter().position(|candidate| candidate == item) else {
        return Err(FanError::SpeedItemNotFound);
    };
    let len = ordered_list.len() as u128;
    let position = index as u128 + 1;
    Ok(((position * 100) / len) as i64)
}

/// Convert a percentage to the first ordered speed whose floored upper bound
/// contains it. Values above 100 select the last item, as in the source helper.
pub fn percentage_to_ordered_list_item<T>(
    ordered_list: &[T],
    percentage: f64,
) -> Result<&T, FanError> {
    if ordered_list.is_empty() {
        return Err(FanError::EmptyOrderedList);
    }
    for (offset, speed) in ordered_list.iter().enumerate() {
        let upper_bound = (((offset as u128 + 1) * 100) / ordered_list.len() as u128) as f64;
        if percentage <= upper_bound {
            return Ok(speed);
        }
    }
    Ok(ordered_list.last().expect("non-empty checked above"))
}

/// Convert one value in an inclusive numeric range to an integer percentage.
/// The source's integer scaling uses floor division and does not clamp values.
pub fn ranged_value_to_percentage(low_high_range: (f64, f64), value: f64) -> Result<i64, FanError> {
    let denominator = validate_range(low_high_range)?;
    if !value.is_finite() {
        return Err(FanError::NonFiniteValue);
    }
    let numerator = (value - (low_high_range.0 - 1.0)) * 100.0;
    let remainder = numerator % denominator;
    let mut quotient = (numerator - remainder) / denominator;
    if remainder < 0.0 {
        quotient -= 1.0;
    }
    let mut percentage = quotient.floor();
    if quotient - percentage > 0.5 {
        percentage += 1.0;
    }
    if !percentage.is_finite() {
        return Err(FanError::NumericOverflow);
    }
    if percentage < i64::MIN as f64 || percentage > i64::MAX as f64 {
        return Err(FanError::NumericOverflow);
    }
    Ok(percentage as i64)
}

/// Convert a percentage to a float in an inclusive numeric range.
/// The result is deliberately not rounded or clamped.
pub fn percentage_to_ranged_value(
    low_high_range: (f64, f64),
    percentage: f64,
) -> Result<f64, FanError> {
    let length = validate_range(low_high_range)?;
    if !percentage.is_finite() {
        return Err(FanError::NonFiniteValue);
    }
    Ok((percentage * length / 100.0) + (low_high_range.0 - 1.0))
}

/// Apply Home Assistant's fan increase/decrease arithmetic and return the
/// clamped percentage passed to `async_set_percentage`.
pub fn adjust_speed_percentage(
    current_percentage: Option<i64>,
    speed_count: i64,
    percentage_step: Option<i64>,
    modifier: i64,
) -> Result<i64, FanError> {
    let current = current_percentage.unwrap_or(0);
    let result = if let Some(step) = percentage_step {
        i128::from(current) + i128::from(step) * i128::from(modifier)
    } else {
        if speed_count == 0 {
            return Err(FanError::ZeroSpeedCount);
        }
        let range = (1.0, speed_count as f64);
        let current_index = percentage_to_ranged_value(range, current as f64)?.ceil();
        if current_index < i64::MIN as f64 || current_index > i64::MAX as f64 {
            return Err(FanError::NumericOverflow);
        }
        let next_index = i128::from(current_index as i64) + i128::from(modifier);
        let next_percentage = ranged_value_to_percentage(range, next_index as f64)?;
        i128::from(next_percentage)
    };
    Ok(result.clamp(0, 100) as i64)
}

fn validate_range(range: (f64, f64)) -> Result<f64, FanError> {
    if !range.0.is_finite() || !range.1.is_finite() {
        return Err(FanError::InvalidRange);
    }
    let length = range.1 - range.0 + 1.0;
    if range.1 < range.0 || !length.is_finite() || length <= 0.0 {
        return Err(FanError::InvalidRange);
    }
    Ok(length)
}

#[cfg(test)]
mod tests;
