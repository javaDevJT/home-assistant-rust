//! Typed cover descriptions, state projections, and standard commands.
//!
//! The contract follows Home Assistant Core commit
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Providers describe stable
//! registry identity with [`CoverDescriptor`], project reported state through
//! [`CoverSnapshot`], and can use [`CoverCommand`] to validate and resolve the
//! standard cover service vocabulary. The shared [`hass_entity::EntityLifecycle`]
//! remains responsible for registry identity, user overrides, rename/disable
//! handling, and state publication.

use hass_core::{Context, ServiceCall};
use hass_entity::{EntityPublication, EntitySnapshot, EntitySpec};
use serde_json::{Map, Value, json};
use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};
use std::str::FromStr;

/// Home Assistant entity domain for native covers.
pub const COVER_DOMAIN: &str = "cover";

/// Reported cover position attribute name.
pub const ATTR_CURRENT_POSITION: &str = "current_position";
/// Reported cover tilt position attribute name.
pub const ATTR_CURRENT_TILT_POSITION: &str = "current_tilt_position";
/// Reported closed/open attribute name.
pub const ATTR_IS_CLOSED: &str = "is_closed";
/// Requested cover position service field name.
pub const ATTR_POSITION: &str = "position";
/// Requested cover speed service field name.
pub const ATTR_SPEED: &str = "speed";
/// Reported and requested cover tilt position field name.
pub const ATTR_TILT_POSITION: &str = "tilt_position";
/// Supported cover speed capability attribute name.
pub const ATTR_SUPPORTED_SPEEDS: &str = "supported_speeds";
/// Current supported feature mask attribute name.
pub const ATTR_SUPPORTED_FEATURES: &str = "supported_features";

/// Standard selector keys understood by the shared service-target resolver.
///
/// Selector values are left to `hass-registry` for validation and resolution.
pub const COVER_TARGET_FIELDS: &[&str] =
    &["entity_id", "device_id", "area_id", "floor_id", "label_id"];

/// A device class supported by Home Assistant's cover component.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CoverDeviceClass {
    /// Retractable awning.
    Awning,
    /// Blind.
    Blind,
    /// Curtain.
    Curtain,
    /// Damper.
    Damper,
    /// Door.
    Door,
    /// Garage door.
    Garage,
    /// Gate.
    Gate,
    /// Shade.
    Shade,
    /// Shutter.
    Shutter,
    /// Window.
    Window,
}

impl CoverDeviceClass {
    /// All device classes in pinned upstream enum order.
    pub const ALL: [Self; 10] = [
        Self::Awning,
        Self::Blind,
        Self::Curtain,
        Self::Damper,
        Self::Door,
        Self::Garage,
        Self::Gate,
        Self::Shade,
        Self::Shutter,
        Self::Window,
    ];

    /// Return the Home Assistant device-class string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Awning => "awning",
            Self::Blind => "blind",
            Self::Curtain => "curtain",
            Self::Damper => "damper",
            Self::Door => "door",
            Self::Garage => "garage",
            Self::Gate => "gate",
            Self::Shade => "shade",
            Self::Shutter => "shutter",
            Self::Window => "window",
        }
    }
}

impl fmt::Display for CoverDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CoverDeviceClass {
    type Err = UnknownCoverDeviceClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|class| class.as_str() == normalized)
            .ok_or_else(|| UnknownCoverDeviceClass(value.to_owned()))
    }
}

impl TryFrom<&str> for CoverDeviceClass {
    type Error = UnknownCoverDeviceClass;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Parse failure for an unsupported cover device class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownCoverDeviceClass(String);

impl fmt::Display for UnknownCoverDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown cover device class {:?}", self.0)
    }
}

impl std::error::Error for UnknownCoverDeviceClass {}

/// Home Assistant cover feature bits.
///
/// The bit positions match the pinned `CoverEntityFeature` `IntFlag`. Unknown
/// bits are retained by [`Self::from_bits_retain`] so newer providers can pass
/// through feature values without losing information.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CoverFeatures(u64);

impl CoverFeatures {
    /// No cover features.
    pub const EMPTY: Self = Self(0);
    /// Open the cover.
    pub const OPEN: Self = Self(1);
    /// Close the cover.
    pub const CLOSE: Self = Self(2);
    /// Set the cover position.
    pub const SET_POSITION: Self = Self(4);
    /// Stop cover movement.
    pub const STOP: Self = Self(8);
    /// Open the cover tilt.
    pub const OPEN_TILT: Self = Self(16);
    /// Close the cover tilt.
    pub const CLOSE_TILT: Self = Self(32);
    /// Stop cover tilt movement.
    pub const STOP_TILT: Self = Self(64);
    /// Set the cover tilt position.
    pub const SET_TILT_POSITION: Self = Self(128);
    /// Select among supported cover speeds.
    pub const SPEED: Self = Self(256);

    /// Construct a feature set while retaining every supplied bit.
    #[must_use]
    pub const fn from_bits_retain(bits: u64) -> Self {
        Self(bits)
    }

    /// Return the raw Home Assistant feature bitmask.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Return whether every feature in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Return whether this feature set has no bits set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for CoverFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CoverFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for CoverFeatures {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// A validated 0–100 cover or tilt service target.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CoverPercentage(u8);

impl CoverPercentage {
    /// Construct a percentage if `value` is within the inclusive 0–100 range.
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 100 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Return the percentage as an integer.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<i64> for CoverPercentage {
    type Error = i64;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u8::try_from(value).ok().and_then(Self::new).ok_or(value)
    }
}

impl fmt::Display for CoverPercentage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A reported cover state. An absent value maps to Home Assistant `unknown`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CoverState {
    /// Fully closed.
    Closed,
    /// Moving toward closed.
    Closing,
    /// Fully open.
    Open,
    /// Moving toward open.
    Opening,
}

impl CoverState {
    /// Return the canonical Home Assistant state string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Closing => "closing",
            Self::Open => "open",
            Self::Opening => "opening",
        }
    }
}

impl fmt::Display for CoverState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Standard speed labels defined by Home Assistant's cover component.
///
/// Providers may still advertise custom speed strings through
/// [`CoverSnapshot::with_supported_speeds`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CoverSpeed {
    /// Move quickly.
    Fast,
    /// Move at an intermediate speed.
    Medium,
    /// Move slowly.
    Slow,
}

impl CoverSpeed {
    /// Standard speeds in pinned upstream enum order.
    pub const ALL: [Self; 3] = [Self::Fast, Self::Medium, Self::Slow];

    /// Return the Home Assistant speed string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Medium => "medium",
            Self::Slow => "slow",
        }
    }
}

impl fmt::Display for CoverSpeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CoverSpeed {
    type Err = UnknownCoverSpeed;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|speed| speed.as_str() == value)
            .ok_or_else(|| UnknownCoverSpeed(value.to_owned()))
    }
}

/// Parse failure for a nonstandard Home Assistant cover speed label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownCoverSpeed(String);

impl fmt::Display for UnknownCoverSpeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown cover speed {:?}", self.0)
    }
}

impl std::error::Error for UnknownCoverSpeed {}

/// Standard Home Assistant cover services.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CoverService {
    /// Close the cover.
    Close,
    /// Close the cover tilt.
    CloseTilt,
    /// Open the cover.
    Open,
    /// Open the cover tilt.
    OpenTilt,
    /// Set the cover position.
    SetPosition,
    /// Set the cover tilt position.
    SetTiltPosition,
    /// Stop cover movement.
    Stop,
    /// Stop cover tilt movement.
    StopTilt,
    /// Toggle the cover.
    Toggle,
    /// Toggle the cover tilt.
    ToggleTilt,
}

impl CoverService {
    /// All standard cover services in pinned upstream enum order.
    pub const ALL: [Self; 10] = [
        Self::Close,
        Self::CloseTilt,
        Self::Open,
        Self::OpenTilt,
        Self::SetPosition,
        Self::SetTiltPosition,
        Self::Stop,
        Self::StopTilt,
        Self::Toggle,
        Self::ToggleTilt,
    ];

    /// Return the Home Assistant service name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Close => "close_cover",
            Self::CloseTilt => "close_cover_tilt",
            Self::Open => "open_cover",
            Self::OpenTilt => "open_cover_tilt",
            Self::SetPosition => "set_cover_position",
            Self::SetTiltPosition => "set_cover_tilt_position",
            Self::Stop => "stop_cover",
            Self::StopTilt => "stop_cover_tilt",
            Self::Toggle => "toggle",
            Self::ToggleTilt => "toggle_cover_tilt",
        }
    }
}

impl fmt::Display for CoverService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CoverService {
    type Err = CoverCommandError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|service| service.as_str() == value)
            .ok_or_else(|| CoverCommandError::UnknownService(value.to_owned()))
    }
}

/// Provider description for one `cover` registry entity.
///
/// Provider-owned original metadata is kept on the shared `EntitySpec`.
/// Registry overrides remain owned by `EntityLifecycle`.
#[derive(Clone, Debug)]
pub struct CoverDescriptor {
    spec: EntitySpec,
    device_class: Option<CoverDeviceClass>,
    supported_features: Option<CoverFeatures>,
}

impl CoverDescriptor {
    /// Create a cover descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        let mut spec = EntitySpec::new(COVER_DOMAIN, platform, provider_id, provider_key)
            .with_unrecorded_attributes([ATTR_SUPPORTED_SPEEDS]);
        spec.info.supported_features =
            (CoverFeatures::OPEN | CoverFeatures::CLOSE | CoverFeatures::STOP).bits();
        Self {
            spec,
            device_class: None,
            supported_features: None,
        }
    }

    /// Return the currently described device class, if one was set.
    #[must_use]
    pub const fn device_class(&self) -> Option<CoverDeviceClass> {
        self.device_class
    }

    /// Set the typed cover device class.
    #[must_use]
    pub fn with_device_class(mut self, device_class: CoverDeviceClass) -> Self {
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

    /// Set provider capability attributes.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Value) -> Self {
        self.spec.info.capabilities = Some(capabilities);
        self
    }

    /// Set provider-supported feature bits, overriding inferred features.
    #[must_use]
    pub fn with_supported_features(mut self, supported_features: CoverFeatures) -> Self {
        self.spec.info.supported_features = supported_features.bits();
        self.supported_features = Some(supported_features);
        self
    }

    /// Clear the explicit feature override and infer features from each snapshot.
    #[must_use]
    pub fn without_supported_features(mut self) -> Self {
        self.spec.info.supported_features =
            (CoverFeatures::OPEN | CoverFeatures::CLOSE | CoverFeatures::STOP).bits();
        self.supported_features = None;
        self
    }

    /// Set whether newly created registry entries start enabled.
    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Build a lifecycle-ready `EntitySpec` without consuming the descriptor.
    ///
    /// When feature bits are inferred from state, prefer
    /// [`Self::entity_spec_for`] so the initial registry entry starts with the
    /// same feature mask as its first publication.
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        self.spec.clone()
    }

    /// Build a lifecycle-ready `EntitySpec` using the initial snapshot's feature bits.
    #[must_use]
    pub fn entity_spec_for(&self, snapshot: &CoverSnapshot) -> EntitySpec {
        let mut spec = self.spec.clone();
        spec.info.supported_features = self.supported_features(snapshot).bits();
        spec
    }

    /// Consume the descriptor and build its lifecycle-ready `EntitySpec`.
    #[must_use]
    pub fn into_entity_spec(self) -> EntitySpec {
        self.spec
    }

    /// Return explicit or source-inferred feature bits for one snapshot.
    #[must_use]
    pub fn supported_features(&self, snapshot: &CoverSnapshot) -> CoverFeatures {
        self.supported_features
            .unwrap_or_else(|| snapshot.inferred_features())
    }
}

/// Current provider value and dynamic attributes for one cover.
///
/// `is_closed`, positions, and motion flags retain their source `None` values.
/// An absent `is_closed` produces an `unknown` state and an explicit JSON null
/// `is_closed` attribute. Position attributes are omitted when their values are
/// absent. An unavailable publication becomes `unavailable` through the
/// shared lifecycle, which suppresses state attributes but retains live
/// capability metadata.
#[derive(Clone, Debug)]
pub struct CoverSnapshot {
    is_closed: Option<bool>,
    is_opening: Option<bool>,
    is_closing: Option<bool>,
    current_position: Option<i64>,
    current_tilt_position: Option<i64>,
    supported_speeds: Option<Vec<String>>,
    available: bool,
    extra_attributes: Map<String, Value>,
    context: Context,
}

impl CoverSnapshot {
    /// Create an available snapshot with a reported closed/open value.
    #[must_use]
    pub fn new(is_closed: Option<bool>) -> Self {
        Self {
            is_closed,
            is_opening: None,
            is_closing: None,
            current_position: None,
            current_tilt_position: None,
            supported_speeds: None,
            available: true,
            extra_attributes: Map::new(),
            context: Context::default(),
        }
    }

    /// Create an unavailable snapshot with no last reported state.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::new(None).with_available(false)
    }

    /// Set whether the provider currently considers this cover available.
    #[must_use]
    pub fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Set the reported `is_closed` value.
    #[must_use]
    pub fn with_is_closed(mut self, is_closed: Option<bool>) -> Self {
        self.is_closed = is_closed;
        self
    }

    /// Set whether the cover is moving open.
    #[must_use]
    pub fn with_is_opening(mut self, is_opening: Option<bool>) -> Self {
        self.is_opening = is_opening;
        self
    }

    /// Set whether the cover is moving closed.
    #[must_use]
    pub fn with_is_closing(mut self, is_closing: Option<bool>) -> Self {
        self.is_closing = is_closing;
        self
    }

    /// Set the current cover position as reported by the provider.
    #[must_use]
    pub fn with_current_position(mut self, current_position: Option<i64>) -> Self {
        self.current_position = current_position;
        self
    }

    /// Set the current cover tilt position as reported by the provider.
    #[must_use]
    pub fn with_current_tilt_position(mut self, current_tilt_position: Option<i64>) -> Self {
        self.current_tilt_position = current_tilt_position;
        self
    }

    /// Set the supported cover speeds.
    ///
    /// An absent or empty list means speed selection is not supported.
    #[must_use]
    pub fn with_supported_speeds(mut self, supported_speeds: Option<Vec<String>>) -> Self {
        self.supported_speeds = supported_speeds;
        self
    }

    /// Add or omit one extra attribute; explicit JSON null values are preserved.
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

    /// Merge extra attributes, preserving explicit JSON null values.
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

    /// Return the reported closed/open value.
    #[must_use]
    pub const fn is_closed(&self) -> Option<bool> {
        self.is_closed
    }

    /// Return the reported opening flag.
    #[must_use]
    pub const fn is_opening(&self) -> Option<bool> {
        self.is_opening
    }

    /// Return the reported closing flag.
    #[must_use]
    pub const fn is_closing(&self) -> Option<bool> {
        self.is_closing
    }

    /// Return the reported current position.
    #[must_use]
    pub const fn current_position(&self) -> Option<i64> {
        self.current_position
    }

    /// Return the reported current tilt position.
    #[must_use]
    pub const fn current_tilt_position(&self) -> Option<i64> {
        self.current_tilt_position
    }

    /// Return supported speeds when the provider supplies them.
    #[must_use]
    pub fn supported_speeds(&self) -> Option<&[String]> {
        self.supported_speeds.as_deref()
    }

    /// Return whether the provider currently considers this cover available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }

    /// Return the projected cover state, or `None` for `unknown`.
    ///
    /// Opening takes precedence over closing. Position alone does not determine
    /// open or closed state in the pinned entity implementation.
    #[must_use]
    pub fn state(&self) -> Option<CoverState> {
        if self.is_opening == Some(true) {
            return Some(CoverState::Opening);
        }
        if self.is_closing == Some(true) {
            return Some(CoverState::Closing);
        }
        match self.is_closed {
            Some(true) => Some(CoverState::Closed),
            Some(false) => Some(CoverState::Open),
            None => None,
        }
    }

    /// Infer default features from current positions and supported speeds.
    #[must_use]
    pub fn inferred_features(&self) -> CoverFeatures {
        let mut features = CoverFeatures::OPEN | CoverFeatures::CLOSE | CoverFeatures::STOP;
        if self
            .supported_speeds
            .as_ref()
            .is_some_and(|speeds| !speeds.is_empty())
        {
            features |= CoverFeatures::SPEED;
        }
        if self.current_position.is_some() {
            features |= CoverFeatures::SET_POSITION;
        }
        if self.current_tilt_position.is_some() {
            features |= CoverFeatures::OPEN_TILT
                | CoverFeatures::CLOSE_TILT
                | CoverFeatures::STOP_TILT
                | CoverFeatures::SET_TILT_POSITION;
        }
        features
    }

    /// Convert this value to a shared lifecycle snapshot.
    #[must_use]
    pub fn entity_snapshot(&self, descriptor: &CoverDescriptor) -> EntitySnapshot {
        let mut attributes = Map::new();
        attributes.insert(
            ATTR_IS_CLOSED.into(),
            self.is_closed.map_or(Value::Null, Value::Bool),
        );
        if let Some(position) = self.current_position {
            attributes.insert(ATTR_CURRENT_POSITION.into(), json!(position));
        }
        if let Some(position) = self.current_tilt_position {
            attributes.insert(ATTR_CURRENT_TILT_POSITION.into(), json!(position));
        }
        if let Some(speeds) = self
            .supported_speeds
            .as_ref()
            .filter(|speeds| !speeds.is_empty())
        {
            attributes.insert(ATTR_SUPPORTED_SPEEDS.into(), json!(speeds));
        }
        attributes.extend(self.extra_attributes.clone());
        attributes.insert(
            ATTR_SUPPORTED_FEATURES.into(),
            json!(descriptor.supported_features(self).bits()),
        );

        EntitySnapshot {
            state: self.state().map(|state| state.as_str().to_owned()),
            available: self.available,
            attributes,
            context: self.context.clone(),
        }
    }

    /// Build a publication that retains capability attributes while unavailable.
    #[must_use]
    pub fn publication(&self, descriptor: &CoverDescriptor) -> EntityPublication {
        let features = descriptor.supported_features(self);
        let supported_speeds = self
            .supported_speeds
            .as_ref()
            .filter(|speeds| !speeds.is_empty())
            .map(|speeds| json!(speeds));
        EntityPublication::from(self.entity_snapshot(descriptor))
            .with_live_attribute(ATTR_SUPPORTED_FEATURES, Some(json!(features.bits())))
            .with_live_attribute(ATTR_SUPPORTED_SPEEDS, supported_speeds)
    }
}

/// One standard Home Assistant cover service with validated arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoverCommand {
    /// Open the cover, optionally requesting a supported speed.
    Open { speed: Option<String> },
    /// Close the cover, optionally requesting a supported speed.
    Close { speed: Option<String> },
    /// Set the cover's 0–100 position, optionally requesting a supported speed.
    SetPosition {
        position: CoverPercentage,
        speed: Option<String>,
    },
    /// Stop cover movement.
    Stop,
    /// Resolve a direction or stop from current state, as HA cover toggle does.
    Toggle,
    /// Open the cover tilt.
    OpenTilt,
    /// Close the cover tilt.
    CloseTilt,
    /// Set the cover tilt's 0–100 position.
    SetTiltPosition { position: CoverPercentage },
    /// Stop cover tilt movement.
    StopTilt,
    /// Toggle tilt; an absent or nonzero tilt position requests closing.
    ToggleTilt,
}

impl CoverCommand {
    /// Parse a standard `cover` service call and its data/target fields.
    ///
    /// The service schemas match the pinned `probatio.Coerce(int)` plus
    /// `Percentage()` position rules. Selector values remain the responsibility
    /// of the shared target resolver.
    pub fn parse_service_call(call: &ServiceCall) -> Result<Self, CoverCommandError> {
        if call.domain != COVER_DOMAIN {
            return Err(CoverCommandError::WrongDomain(call.domain.clone()));
        }
        if call.data.contains_key("entity_id") && call.target.contains_key("entity_id") {
            return Err(CoverCommandError::EntityIdInDataAndTarget);
        }
        if let Some(field) = call.data.keys().find(|field| {
            !COVER_TARGET_FIELDS.contains(&field.as_str())
                && !service_field_allowed(call.service.as_str(), field)
        }) {
            return Err(CoverCommandError::UnknownServiceField(field.clone()));
        }
        if let Some(field) = call
            .target
            .keys()
            .find(|field| !COVER_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(CoverCommandError::UnknownTargetField(field.clone()));
        }

        let service = call.service.parse::<CoverService>()?;
        let speed = optional_string(&call.data, ATTR_SPEED)?;
        match service {
            CoverService::Open => Ok(Self::Open { speed }),
            CoverService::Close => Ok(Self::Close { speed }),
            CoverService::SetPosition => Ok(Self::SetPosition {
                position: required_percentage(&call.data, ATTR_POSITION)?,
                speed,
            }),
            CoverService::Stop => Ok(Self::Stop),
            CoverService::Toggle => Ok(Self::Toggle),
            CoverService::OpenTilt => Ok(Self::OpenTilt),
            CoverService::CloseTilt => Ok(Self::CloseTilt),
            CoverService::SetTiltPosition => Ok(Self::SetTiltPosition {
                position: required_percentage(&call.data, ATTR_TILT_POSITION)?,
            }),
            CoverService::StopTilt => Ok(Self::StopTilt),
            CoverService::ToggleTilt => Ok(Self::ToggleTilt),
        }
    }

    /// Return the standard service for this command.
    #[must_use]
    pub const fn service(&self) -> CoverService {
        match self {
            Self::Open { .. } => CoverService::Open,
            Self::Close { .. } => CoverService::Close,
            Self::SetPosition { .. } => CoverService::SetPosition,
            Self::Stop => CoverService::Stop,
            Self::Toggle => CoverService::Toggle,
            Self::OpenTilt => CoverService::OpenTilt,
            Self::CloseTilt => CoverService::CloseTilt,
            Self::SetTiltPosition { .. } => CoverService::SetTiltPosition,
            Self::StopTilt => CoverService::StopTilt,
            Self::ToggleTilt => CoverService::ToggleTilt,
        }
    }

    /// Return the feature mask required by the pinned service registration.
    #[must_use]
    pub fn required_features(&self) -> CoverFeatures {
        match self {
            Self::Open { .. } => CoverFeatures::OPEN,
            Self::Close { .. } => CoverFeatures::CLOSE,
            Self::SetPosition { .. } => CoverFeatures::SET_POSITION,
            Self::Stop => CoverFeatures::STOP,
            Self::Toggle => CoverFeatures::OPEN | CoverFeatures::CLOSE,
            Self::OpenTilt => CoverFeatures::OPEN_TILT,
            Self::CloseTilt => CoverFeatures::CLOSE_TILT,
            Self::SetTiltPosition { .. } => CoverFeatures::SET_TILT_POSITION,
            Self::StopTilt => CoverFeatures::STOP_TILT,
            Self::ToggleTilt => CoverFeatures::OPEN_TILT | CoverFeatures::CLOSE_TILT,
        }
    }

    /// Resolve a validated command to a concrete provider action.
    ///
    /// Open/close/set-position speeds that are not in the current nonempty
    /// supported-speed list are omitted, matching the source wrapper. Toggle
    /// stops only when the cover reports motion and advertises `STOP`.
    pub fn resolve(
        &self,
        descriptor: &CoverDescriptor,
        snapshot: &CoverSnapshot,
        toggle_state: &mut CoverToggleState,
    ) -> Result<CoverAction, CoverCommandError> {
        let supported = descriptor.supported_features(snapshot);
        let required = self.required_features();
        if !supported.contains(required) {
            return Err(CoverCommandError::UnsupportedFeature {
                required: required.bits(),
                supported: supported.bits(),
            });
        }

        toggle_state.observe(snapshot);
        let action = match self {
            Self::Open { speed } => CoverAction::Open {
                speed: supported_speed(speed.as_deref(), snapshot),
            },
            Self::Close { speed } => CoverAction::Close {
                speed: supported_speed(speed.as_deref(), snapshot),
            },
            Self::SetPosition { position, speed } => CoverAction::SetPosition {
                position: *position,
                speed: supported_speed(speed.as_deref(), snapshot),
            },
            Self::Stop => CoverAction::Stop,
            Self::Toggle => toggle_state.resolve(snapshot, supported),
            Self::OpenTilt => CoverAction::OpenTilt,
            Self::CloseTilt => CoverAction::CloseTilt,
            Self::SetTiltPosition { position } => CoverAction::SetTiltPosition {
                position: *position,
            },
            Self::StopTilt => CoverAction::StopTilt,
            Self::ToggleTilt => {
                if snapshot.current_tilt_position == Some(0) {
                    CoverAction::OpenTilt
                } else {
                    CoverAction::CloseTilt
                }
            }
        };
        Ok(action)
    }
}

/// Concrete action to pass to a cover provider after command resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoverAction {
    /// Open the cover, optionally with a retained supported speed.
    Open { speed: Option<String> },
    /// Close the cover, optionally with a retained supported speed.
    Close { speed: Option<String> },
    /// Set the cover position.
    SetPosition {
        position: CoverPercentage,
        speed: Option<String>,
    },
    /// Stop cover movement.
    Stop,
    /// Open the cover tilt.
    OpenTilt,
    /// Close the cover tilt.
    CloseTilt,
    /// Set the cover tilt position.
    SetTiltPosition { position: CoverPercentage },
    /// Stop cover tilt movement.
    StopTilt,
}

/// Stateful memory for Home Assistant's cover-toggle direction fallback.
///
/// Home Assistant starts by assuming a partially open cover should close, then
/// remembers its latest reported opening/closing direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverToggleState {
    last_toggle_direction_open: bool,
}

impl Default for CoverToggleState {
    fn default() -> Self {
        Self {
            last_toggle_direction_open: true,
        }
    }
}

impl CoverToggleState {
    /// Create toggle memory with the source's initial direction assumption.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_toggle_direction_open: true,
        }
    }

    /// Observe state flags and remember the most recently reported direction.
    pub fn observe(&mut self, snapshot: &CoverSnapshot) {
        if snapshot.is_opening == Some(true) {
            self.last_toggle_direction_open = true;
        } else if snapshot.is_closing == Some(true) {
            self.last_toggle_direction_open = false;
        }
    }

    /// Return the last remembered movement direction.
    #[must_use]
    pub const fn last_toggle_direction_open(self) -> bool {
        self.last_toggle_direction_open
    }

    fn resolve(&self, snapshot: &CoverSnapshot, features: CoverFeatures) -> CoverAction {
        let is_opening = snapshot.is_opening == Some(true);
        let is_closing = snapshot.is_closing == Some(true);
        if features.contains(CoverFeatures::STOP) && (is_closing || is_opening) {
            return CoverAction::Stop;
        }
        if snapshot.is_closed == Some(true) || is_closing {
            return CoverAction::Open { speed: None };
        }
        if snapshot.current_position == Some(100) || is_opening {
            return CoverAction::Close { speed: None };
        }
        if self.last_toggle_direction_open {
            CoverAction::Close { speed: None }
        } else {
            CoverAction::Open { speed: None }
        }
    }
}

/// A rejected cover service call or feature-gated command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoverCommandError {
    /// The call is for a domain other than `cover`.
    WrongDomain(String),
    /// The service name is not one of the standard cover services.
    UnknownService(String),
    /// The service data contains an unsupported non-target field.
    UnknownServiceField(String),
    /// The target contains a field unknown to the shared target resolver.
    UnknownTargetField(String),
    /// Entity IDs were supplied in both the target and legacy data map.
    EntityIdInDataAndTarget,
    /// A required service value was absent.
    MissingArgument(String),
    /// A service value could not be coerced to an integer.
    InvalidInteger { argument: String, value: Value },
    /// A percentage service value was outside the inclusive 0–100 range.
    PercentageOutOfRange { argument: String, value: i64 },
    /// A string service value was not a JSON string.
    InvalidString { argument: String, value: Value },
    /// The entity does not advertise every feature required by this service.
    UnsupportedFeature { required: u64, supported: u64 },
}

impl fmt::Display for CoverCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDomain(domain) => write!(formatter, "expected cover domain, got {domain:?}"),
            Self::UnknownService(service) => write!(formatter, "unknown cover service {service:?}"),
            Self::UnknownServiceField(field) => {
                write!(formatter, "unsupported cover service field {field:?}")
            }
            Self::UnknownTargetField(field) => {
                write!(formatter, "unsupported cover target field {field:?}")
            }
            Self::EntityIdInDataAndTarget => {
                formatter.write_str("entity_id must be supplied in target or data, not both")
            }
            Self::MissingArgument(argument) => {
                write!(formatter, "missing required cover argument {argument:?}")
            }
            Self::InvalidInteger { argument, value } => {
                write!(
                    formatter,
                    "cover argument {argument:?} is not integer-coercible: {value}"
                )
            }
            Self::PercentageOutOfRange { argument, value } => write!(
                formatter,
                "cover argument {argument:?} percentage {value} is outside 0..=100"
            ),
            Self::InvalidString { argument, value } => {
                write!(
                    formatter,
                    "cover argument {argument:?} is not a string: {value}"
                )
            }
            Self::UnsupportedFeature {
                required,
                supported,
            } => write!(
                formatter,
                "cover service requires feature bits {required}, supported bits are {supported}"
            ),
        }
    }
}

impl std::error::Error for CoverCommandError {}

fn service_field_allowed(service: &str, field: &str) -> bool {
    match service {
        "open_cover" | "close_cover" => field == ATTR_SPEED,
        "set_cover_position" => matches!(field, ATTR_POSITION | ATTR_SPEED),
        "set_cover_tilt_position" => field == ATTR_TILT_POSITION,
        _ => false,
    }
}

fn optional_string(
    data: &Map<String, Value>,
    argument: &str,
) -> Result<Option<String>, CoverCommandError> {
    let Some(value) = data.get(argument) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| CoverCommandError::InvalidString {
            argument: argument.to_owned(),
            value: value.clone(),
        })
}

fn required_percentage(
    data: &Map<String, Value>,
    argument: &str,
) -> Result<CoverPercentage, CoverCommandError> {
    let value = data
        .get(argument)
        .ok_or_else(|| CoverCommandError::MissingArgument(argument.to_owned()))?;
    let integer = coerce_integer(value).ok_or_else(|| CoverCommandError::InvalidInteger {
        argument: argument.to_owned(),
        value: value.clone(),
    })?;
    CoverPercentage::try_from(integer).map_err(|_| CoverCommandError::PercentageOutOfRange {
        argument: argument.to_owned(),
        value: integer,
    })
}

fn coerce_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| {
                number
                    .as_u64()
                    .and_then(|number| i64::try_from(number).ok())
            })
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .map(|number| number.trunc() as i64)
            }),
        Value::String(value) => value.trim().parse::<i64>().ok(),
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn supported_speed(speed: Option<&str>, snapshot: &CoverSnapshot) -> Option<String> {
    let speed = speed?;
    snapshot
        .supported_speeds
        .as_ref()
        .filter(|speeds| speeds.iter().any(|supported| supported == speed))
        .map(|_| speed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::Runtime;
    use hass_registry::Registries;
    use hass_storage::SqliteStore;
    use serde_json::Map;
    use std::sync::Arc;

    fn service_call(
        service: &str,
        data: Map<String, Value>,
        target: Map<String, Value>,
    ) -> ServiceCall {
        ServiceCall {
            domain: COVER_DOMAIN.into(),
            service: service.into(),
            data,
            target,
            context: Context::default(),
        }
    }

    #[test]
    fn cover_device_classes_match_the_pinned_enum_and_parse_case_insensitively() {
        assert_eq!(
            CoverDeviceClass::ALL.map(CoverDeviceClass::as_str),
            [
                "awning", "blind", "curtain", "damper", "door", "garage", "gate", "shade",
                "shutter", "window"
            ]
        );
        assert_eq!("WINDOW".parse(), Ok(CoverDeviceClass::Window));
        assert_eq!(
            "screen"
                .parse::<CoverDeviceClass>()
                .unwrap_err()
                .to_string(),
            "unknown cover device class \"screen\""
        );
    }

    #[test]
    fn cover_speed_labels_match_the_pinned_enum() {
        assert_eq!(
            CoverSpeed::ALL.map(CoverSpeed::as_str),
            ["fast", "medium", "slow"]
        );
        assert_eq!("slow".parse(), Ok(CoverSpeed::Slow));
        assert!("quiet".parse::<CoverSpeed>().is_err());
    }

    #[test]
    fn descriptor_preserves_entity_identity_class_and_explicit_features() {
        let descriptor = CoverDescriptor::new("mqtt", "cover-provider", "blind/kitchen")
            .with_unique_id("kitchen-blind")
            .with_config_entry_id("config-1")
            .with_device_id("device-2")
            .with_area_id("area-3")
            .with_suggested_object_id("kitchen_blind")
            .with_name("Kitchen blind")
            .with_device_class(CoverDeviceClass::Blind)
            .with_supported_features(CoverFeatures::OPEN | CoverFeatures::CLOSE);
        let snapshot = CoverSnapshot::new(Some(true));
        let spec = descriptor.entity_spec_for(&snapshot);

        assert_eq!(spec.domain, COVER_DOMAIN);
        assert_eq!(spec.platform, "mqtt");
        assert_eq!(spec.provider_id, "cover-provider");
        assert_eq!(spec.provider_key, "blind/kitchen");
        assert_eq!(spec.unique_id.as_deref(), Some("kitchen-blind"));
        assert_eq!(spec.config_entry_id.as_deref(), Some("config-1"));
        assert_eq!(spec.device_id.as_deref(), Some("device-2"));
        assert_eq!(spec.area_id.as_deref(), Some("area-3"));
        assert_eq!(spec.suggested_object_id.as_deref(), Some("kitchen_blind"));
        assert_eq!(spec.info.original_name.as_deref(), Some("Kitchen blind"));
        assert_eq!(spec.info.original_device_class.as_deref(), Some("blind"));
        assert_eq!(spec.info.supported_features, 3);
        assert_eq!(descriptor.device_class(), Some(CoverDeviceClass::Blind));
        assert!(
            spec.recording_policy
                .unrecorded_attributes()
                .contains("supported_speeds")
        );
    }

    #[test]
    fn snapshot_projects_source_states_nulls_positions_tilt_and_inferred_features() {
        let moving_open = CoverSnapshot::new(Some(true))
            .with_is_opening(Some(true))
            .with_is_closing(Some(true))
            .with_current_position(Some(47))
            .with_current_tilt_position(Some(15))
            .with_supported_speeds(Some(vec!["slow".into(), "fast".into()]));
        assert_eq!(moving_open.state(), Some(CoverState::Opening));
        assert_eq!(
            moving_open.inferred_features().bits(),
            1 | 2 | 4 | 8 | 16 | 32 | 64 | 128 | 256
        );
        let descriptor = CoverDescriptor::new("mqtt", "cover-provider", "blind/kitchen");
        let entity = moving_open.entity_snapshot(&descriptor);
        assert_eq!(entity.state.as_deref(), Some("opening"));
        assert_eq!(entity.attributes["is_closed"], json!(true));
        assert_eq!(entity.attributes["current_position"], json!(47));
        assert_eq!(entity.attributes["current_tilt_position"], json!(15));
        assert_eq!(
            entity.attributes["supported_speeds"],
            json!(["slow", "fast"])
        );

        let unknown = CoverSnapshot::new(None);
        let entity = unknown.entity_snapshot(&descriptor);
        assert_eq!(entity.state, None);
        assert_eq!(entity.attributes["is_closed"], Value::Null);
        assert!(!entity.attributes.contains_key("current_position"));
        assert!(!entity.attributes.contains_key("current_tilt_position"));
        assert_eq!(unknown.inferred_features().bits(), 1 | 2 | 8);

        assert_eq!(
            CoverSnapshot::new(Some(false)).state(),
            Some(CoverState::Open)
        );
        assert_eq!(
            CoverSnapshot::new(Some(false))
                .with_is_closing(Some(true))
                .state(),
            Some(CoverState::Closing)
        );
    }

    #[test]
    fn service_calls_validate_schema_fields_percentages_and_feature_masks() {
        let mut data = Map::new();
        data.insert("position".into(), json!("84"));
        data.insert("speed".into(), json!("slow"));
        data.insert("entity_id".into(), json!("cover.kitchen"));
        let command =
            CoverCommand::parse_service_call(&service_call("set_cover_position", data, Map::new()))
                .unwrap();
        assert_eq!(
            command,
            CoverCommand::SetPosition {
                position: CoverPercentage::new(84).unwrap(),
                speed: Some("slow".into())
            }
        );
        assert_eq!(command.required_features(), CoverFeatures::SET_POSITION);

        for value in [json!(-1), json!(101), json!("not a number"), Value::Null] {
            let mut data = Map::new();
            data.insert("position".into(), value);
            assert!(matches!(
                CoverCommand::parse_service_call(&service_call(
                    "set_cover_position",
                    data,
                    Map::new()
                )),
                Err(CoverCommandError::InvalidInteger { .. })
                    | Err(CoverCommandError::PercentageOutOfRange { .. })
            ));
        }

        let mut data = Map::new();
        data.insert("position".into(), json!(51));
        data.insert("transition".into(), json!(1));
        assert_eq!(
            CoverCommand::parse_service_call(&service_call("set_cover_position", data, Map::new())),
            Err(CoverCommandError::UnknownServiceField("transition".into()))
        );

        let mut data = Map::new();
        data.insert("entity_id".into(), json!("cover.one"));
        let mut target = Map::new();
        target.insert("entity_id".into(), json!("cover.two"));
        assert_eq!(
            CoverCommand::parse_service_call(&service_call("toggle", data, target)),
            Err(CoverCommandError::EntityIdInDataAndTarget)
        );

        let toggle = CoverCommand::Toggle;
        assert_eq!(
            toggle.required_features(),
            CoverFeatures::OPEN | CoverFeatures::CLOSE
        );
        let descriptor = CoverDescriptor::new("mqtt", "cover-provider", "one")
            .with_supported_features(CoverFeatures::OPEN);
        assert!(matches!(
            toggle.resolve(
                &descriptor,
                &CoverSnapshot::new(None),
                &mut CoverToggleState::new()
            ),
            Err(CoverCommandError::UnsupportedFeature { .. })
        ));
    }

    #[test]
    fn command_speed_filter_toggle_fallbacks_and_tilt_toggle_match_source() {
        let descriptor = CoverDescriptor::new("mqtt", "cover-provider", "one");
        let snapshot = CoverSnapshot::new(Some(false))
            .with_current_position(Some(47))
            .with_supported_speeds(Some(vec!["slow".into()]));
        let mut toggle_state = CoverToggleState::new();
        assert_eq!(
            CoverCommand::Open {
                speed: Some("fast".into())
            }
            .resolve(&descriptor, &snapshot, &mut toggle_state)
            .unwrap(),
            CoverAction::Open { speed: None }
        );
        assert_eq!(
            CoverCommand::Open {
                speed: Some("slow".into())
            }
            .resolve(&descriptor, &snapshot, &mut toggle_state)
            .unwrap(),
            CoverAction::Open {
                speed: Some("slow".into())
            }
        );
        assert_eq!(
            CoverCommand::Toggle
                .resolve(&descriptor, &snapshot, &mut toggle_state)
                .unwrap(),
            CoverAction::Close { speed: None }
        );

        let moving_open = CoverSnapshot::new(None).with_is_opening(Some(true));
        assert_eq!(
            CoverCommand::Toggle
                .resolve(&descriptor, &moving_open, &mut toggle_state)
                .unwrap(),
            CoverAction::Stop
        );
        let no_stop = CoverDescriptor::new("mqtt", "cover-provider", "one")
            .with_supported_features(CoverFeatures::OPEN | CoverFeatures::CLOSE);
        assert_eq!(
            CoverCommand::Toggle
                .resolve(&no_stop, &moving_open, &mut toggle_state)
                .unwrap(),
            CoverAction::Close { speed: None }
        );

        let moving_closed = CoverSnapshot::new(None).with_is_closing(Some(true));
        toggle_state.observe(&moving_closed);
        let stopped_unknown = CoverSnapshot::new(None);
        assert_eq!(
            CoverCommand::Toggle
                .resolve(&descriptor, &stopped_unknown, &mut toggle_state)
                .unwrap(),
            CoverAction::Open { speed: None }
        );
        assert_eq!(
            CoverCommand::ToggleTilt
                .resolve(
                    &CoverDescriptor::new("mqtt", "cover-provider", "one").with_supported_features(
                        CoverFeatures::OPEN_TILT | CoverFeatures::CLOSE_TILT,
                    ),
                    &CoverSnapshot::new(None).with_current_tilt_position(Some(0)),
                    &mut toggle_state
                )
                .unwrap(),
            CoverAction::OpenTilt
        );
        assert_eq!(
            CoverCommand::ToggleTilt
                .resolve(
                    &CoverDescriptor::new("mqtt", "cover-provider", "one").with_supported_features(
                        CoverFeatures::OPEN_TILT | CoverFeatures::CLOSE_TILT,
                    ),
                    &CoverSnapshot::new(None),
                    &mut toggle_state
                )
                .unwrap(),
            CoverAction::CloseTilt
        );
    }

    #[tokio::test]
    async fn lifecycle_publishes_cover_state_and_retains_live_capabilities_offline() {
        let runtime = Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let descriptor = CoverDescriptor::new("mqtt", "cover-provider", "shade/office")
            .with_unique_id("office-shade")
            .with_device_class(CoverDeviceClass::Shade);
        let available = CoverSnapshot::new(Some(true))
            .with_current_position(Some(0))
            .with_supported_speeds(Some(vec!["slow".into()]));
        let handle = lifecycle
            .register_batch(vec![descriptor.entity_spec_for(&available)])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();

        handle
            .publish_publication(available.publication(&descriptor))
            .unwrap()
            .unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "closed");
        assert_eq!(state.attributes["is_closed"], json!(true));
        assert_eq!(state.attributes["current_position"], json!(0));
        assert_eq!(state.attributes["device_class"], json!("shade"));

        let offline = available.clone().with_available(false);
        handle
            .publish_publication(offline.publication(&descriptor))
            .unwrap()
            .unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "unavailable");
        assert!(!state.attributes.contains_key("is_closed"));
        assert!(!state.attributes.contains_key("current_position"));
        assert_eq!(state.attributes["supported_features"], json!(271));
        assert_eq!(state.attributes["supported_speeds"], json!(["slow"]));
        assert_eq!(state.attributes["device_class"], json!("shade"));
        lifecycle.shutdown().await.unwrap();
    }
}
