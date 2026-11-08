//! Typed switch descriptions, state projections, and standard commands.
//!
//! The contract follows Home Assistant Core commit
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Providers describe stable
//! registry identity with [`SwitchDescriptor`], project reported state through
//! [`SwitchSnapshot`], and can use [`SwitchCommand`] to validate the standard
//! `turn_on`, `turn_off`, and `toggle` service vocabulary. The shared
//! [`hass_entity::EntityLifecycle`] remains responsible for registry identity,
//! user overrides, rename/disable handling, and state publication.

use hass_core::{Context, ServiceCall};
use hass_entity::{EntityPublication, EntitySnapshot, EntitySpec};
use serde_json::{Map, Value, json};
use std::fmt;
use std::str::FromStr;

/// Home Assistant entity domain for native switches.
pub const SWITCH_DOMAIN: &str = "switch";

/// Standard selector keys understood by the shared service-target resolver.
///
/// Selector values are left to `hass_registry` for validation and resolution.
pub const SWITCH_TARGET_FIELDS: &[&str] =
    &["entity_id", "device_id", "area_id", "floor_id", "label_id"];

/// State-condition target domains supported by the switch component.
pub const SWITCH_STATE_CONDITION_DOMAINS: &[&str] = &["switch", "input_boolean"];

/// Standard behavior values for switch state conditions.
pub const SWITCH_CONDITION_BEHAVIORS: &[&str] = &["any", "all"];
/// Default behavior for a switch state condition.
pub const SWITCH_STATE_CONDITION_BEHAVIOR_DEFAULT: &str = "any";
/// Default `for` value for a switch state condition.
pub const SWITCH_STATE_CONDITION_FOR_DEFAULT: &str = "00:00:00";
/// State-condition duration field name.
pub const SWITCH_STATE_CONDITION_FOR_FIELD: &str = "for";
/// Whether the switch state-condition UI marks its `for` field required.
pub const SWITCH_STATE_CONDITION_FOR_REQUIRED: bool = true;

/// Optional duration field accepted by switch device conditions.
pub const SWITCH_DEVICE_CONDITION_FOR_FIELD: &str = "for";

/// Device-condition duration schema name used by Home Assistant Core.
pub const SWITCH_DEVICE_CONDITION_FOR_TYPE: &str = "positive_time_period_dict";
/// Whether the switch device-condition `for` field is required.
pub const SWITCH_DEVICE_CONDITION_FOR_REQUIRED: bool = false;

/// A device class supported by Home Assistant's switch component.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SwitchDeviceClass {
    /// A controllable mains outlet.
    Outlet,
    /// A generic switch.
    Switch,
}

impl SwitchDeviceClass {
    /// All device classes in upstream enum order.
    pub const ALL: [Self; 2] = [Self::Outlet, Self::Switch];

    /// Return the Home Assistant device-class string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Outlet => "outlet",
            Self::Switch => "switch",
        }
    }
}

impl fmt::Display for SwitchDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SwitchDeviceClass {
    type Err = UnknownSwitchDeviceClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|class| class.as_str() == normalized)
            .ok_or_else(|| UnknownSwitchDeviceClass(value.to_owned()))
    }
}

impl TryFrom<&str> for SwitchDeviceClass {
    type Error = UnknownSwitchDeviceClass;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Parse failure for an unsupported switch device class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownSwitchDeviceClass(String);

impl fmt::Display for UnknownSwitchDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown switch device class {:?}", self.0)
    }
}

impl std::error::Error for UnknownSwitchDeviceClass {}

/// Provider description for one `switch` registry entity.
///
/// Provider-owned original metadata is kept on the shared `EntitySpec`.
/// Registry overrides, including a user's chosen name or device class, remain
/// owned by `EntityLifecycle`.
#[derive(Clone, Debug)]
pub struct SwitchDescriptor {
    spec: EntitySpec,
    device_class: Option<SwitchDeviceClass>,
}

impl SwitchDescriptor {
    /// Create a switch descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            spec: EntitySpec::new(SWITCH_DOMAIN, platform, provider_id, provider_key),
            device_class: None,
        }
    }

    /// Return the currently described device class, if one was set.
    #[must_use]
    pub const fn device_class(&self) -> Option<SwitchDeviceClass> {
        self.device_class
    }

    /// Set the typed switch device class.
    #[must_use]
    pub fn with_device_class(mut self, device_class: SwitchDeviceClass) -> Self {
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

    /// Set provider-supported feature bits.
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

/// Current provider value and dynamic attributes for one switch.
///
/// `is_on` and `available` are independent. A missing state is `unknown` while
/// available, while an unavailable snapshot projects to `unavailable` through
/// `EntityLifecycle`. Home Assistant only publishes `assumed_state` when true.
#[derive(Clone, Debug)]
pub struct SwitchSnapshot {
    is_on: Option<bool>,
    available: bool,
    assumed_state: bool,
    extra_attributes: Map<String, Value>,
    context: Context,
}

impl SwitchSnapshot {
    /// Create an available snapshot with a typed switch state.
    #[must_use]
    pub fn new(is_on: Option<bool>) -> Self {
        Self {
            is_on,
            available: true,
            assumed_state: false,
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

    /// Set whether the provider currently considers this switch available.
    #[must_use]
    pub fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Set whether the reported state is assumed rather than confirmed.
    #[must_use]
    pub fn with_assumed_state(mut self, assumed_state: bool) -> Self {
        self.assumed_state = assumed_state;
        self
    }

    /// Add or omit one extra attribute.
    ///
    /// `None` removes the key; `Some(Value::Null)` retains an explicit JSON
    /// `null`. Standard metadata such as `device_class`, `icon`, and
    /// `friendly_name` is resolved by the shared lifecycle.
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

    /// Return whether this snapshot's state is assumed.
    #[must_use]
    pub const fn assumed_state(&self) -> bool {
        self.assumed_state
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

    /// Publish base switch metadata even while the reported state is unavailable.
    #[must_use]
    pub fn publication(&self) -> EntityPublication {
        let snapshot = self.entity_snapshot();
        if !self.assumed_state
            && self.available
            && snapshot.attributes.contains_key("assumed_state")
        {
            // BaseEntity leaves an ordinary extra attribute alone when its
            // assumed-state property is false. Extra data is still scoped to
            // availability, rather than promoted into the live metadata map.
            return EntityPublication::from(snapshot);
        }
        EntityPublication::from(snapshot).with_live_attribute(
            "assumed_state",
            self.assumed_state.then_some(Value::Bool(true)),
        )
    }

    /// Convert this value to a shared lifecycle snapshot.
    /// Use [`Self::publication`] to retain live metadata while unavailable.
    #[must_use]
    pub fn entity_snapshot(&self) -> EntitySnapshot {
        let mut attributes = self.extra_attributes.clone();
        if self.assumed_state {
            attributes.insert("assumed_state".into(), Value::Bool(true));
        }
        EntitySnapshot {
            state: self.state().map(str::to_owned),
            available: self.available,
            attributes,
            context: self.context.clone(),
        }
    }

    /// Consume this value and convert it to a shared lifecycle snapshot.
    #[must_use]
    pub fn into_entity_snapshot(self) -> EntitySnapshot {
        let state = self.state().map(str::to_owned);
        let mut attributes = self.extra_attributes;
        if self.assumed_state {
            attributes.insert("assumed_state".into(), Value::Bool(true));
        }
        EntitySnapshot {
            state,
            available: self.available,
            attributes,
            context: self.context,
        }
    }
}

/// One standard Home Assistant switch command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SwitchCommand {
    /// Request the switch turn on.
    TurnOn,
    /// Request the switch turn off.
    TurnOff,
    /// Toggle from the current state; unknown resolves to on.
    Toggle,
}

impl SwitchCommand {
    /// Return the standard service name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnOn => "turn_on",
            Self::TurnOff => "turn_off",
            Self::Toggle => "toggle",
        }
    }

    /// Resolve this service to the requested binary state.
    ///
    /// This follows `ToggleEntity.async_toggle`: only a known `on` value turns
    /// off. Both `off` and `unknown` resolve to on.
    #[must_use]
    pub const fn resolve(self, is_on: Option<bool>) -> SwitchAction {
        match self {
            Self::TurnOn => SwitchAction::On,
            Self::TurnOff => SwitchAction::Off,
            Self::Toggle => match is_on {
                Some(true) => SwitchAction::Off,
                Some(false) | None => SwitchAction::On,
            },
        }
    }

    /// Return descriptive metadata for this standard service.
    #[must_use]
    pub fn description(self) -> &'static SwitchServiceDescription {
        SWITCH_SERVICE_DESCRIPTIONS
            .iter()
            .find(|description| description.command == self)
            .expect("every switch command has a service description")
    }

    /// Parse a native service call, rejecting non-switch commands and fields
    /// outside the standard selector vocabulary.
    ///
    /// This validates selector field names but deliberately leaves selector
    /// values and entity resolution to the shared registry target resolver.
    pub fn parse_service_call(call: &ServiceCall) -> Result<Self, SwitchCommandError> {
        if call.domain != SWITCH_DOMAIN {
            return Err(SwitchCommandError::WrongDomain(call.domain.clone()));
        }
        if call.data.contains_key("entity_id") && call.target.contains_key("entity_id") {
            return Err(SwitchCommandError::EntityIdInDataAndTarget);
        }
        if let Some(field) = call
            .data
            .keys()
            .find(|field| !SWITCH_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(SwitchCommandError::UnknownServiceField(field.clone()));
        }
        if let Some(field) = call
            .target
            .keys()
            .find(|field| !SWITCH_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(SwitchCommandError::UnknownTargetField(field.clone()));
        }
        call.service.parse()
    }
}

impl FromStr for SwitchCommand {
    type Err = SwitchCommandError;

    fn from_str(service: &str) -> Result<Self, Self::Err> {
        match service {
            "turn_on" => Ok(Self::TurnOn),
            "turn_off" => Ok(Self::TurnOff),
            "toggle" => Ok(Self::Toggle),
            _ => Err(SwitchCommandError::UnknownService(service.to_owned())),
        }
    }
}

impl fmt::Display for SwitchCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Final state requested by a [`SwitchCommand`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SwitchAction {
    /// The switch should be on.
    On,
    /// The switch should be off.
    Off,
}

impl SwitchAction {
    /// Return the canonical Home Assistant state string.
    #[must_use]
    pub const fn as_state(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// Return the boolean switch value.
    #[must_use]
    pub const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// Descriptive service metadata sourced from Home Assistant's strings and
/// service definitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwitchServiceDescription {
    /// The parsed command for this service.
    pub command: SwitchCommand,
    /// User-facing service name.
    pub name: &'static str,
    /// User-facing service description.
    pub description: &'static str,
    /// Target entity domain declared in `services.yaml`.
    pub target_domain: &'static str,
    /// Standard service fields; the switch services define none.
    pub fields: &'static [&'static str],
}

impl SwitchServiceDescription {
    /// Return the standard service metadata object.
    #[must_use]
    pub fn metadata_json(self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "target": { "entity": { "domain": self.target_domain } }
        })
    }
}

/// Standard switch service names and their pinned descriptive metadata.
pub const SWITCH_SERVICE_DESCRIPTIONS: [SwitchServiceDescription; 3] = [
    SwitchServiceDescription {
        command: SwitchCommand::TurnOff,
        name: "Turn off switch",
        description: "Turns off a switch.",
        target_domain: SWITCH_DOMAIN,
        fields: &[],
    },
    SwitchServiceDescription {
        command: SwitchCommand::TurnOn,
        name: "Turn on switch",
        description: "Turns on a switch.",
        target_domain: SWITCH_DOMAIN,
        fields: &[],
    },
    SwitchServiceDescription {
        command: SwitchCommand::Toggle,
        name: "Toggle switch",
        description: "Toggles a switch on/off.",
        target_domain: SWITCH_DOMAIN,
        fields: &[],
    },
];

/// State condition supported by the switch component.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SwitchCondition {
    /// Match the exact `off` state.
    IsOff,
    /// Match the exact `on` state.
    IsOn,
}

impl SwitchCondition {
    /// Return the standard condition type string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IsOff => "is_off",
            Self::IsOn => "is_on",
        }
    }

    /// Return whether a Home Assistant state satisfies this condition.
    ///
    /// `unknown`, `unavailable`, and absent states do not satisfy either
    /// condition.
    #[must_use]
    pub fn matches_state(self, state: Option<&str>) -> bool {
        match self {
            Self::IsOff => state == Some("off"),
            Self::IsOn => state == Some("on"),
        }
    }
}

impl FromStr for SwitchCondition {
    type Err = UnknownSwitchCondition;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "is_off" => Ok(Self::IsOff),
            "is_on" => Ok(Self::IsOn),
            _ => Err(UnknownSwitchCondition(value.to_owned())),
        }
    }
}

/// Parse failure for an unsupported switch condition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownSwitchCondition(String);

impl fmt::Display for UnknownSwitchCondition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown switch condition {:?}", self.0)
    }
}

impl std::error::Error for UnknownSwitchCondition {}

/// User-facing metadata for one Home Assistant switch state condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwitchConditionDescription {
    /// Condition behavior.
    pub condition: SwitchCondition,
    /// User-facing condition name.
    pub name: &'static str,
    /// User-facing condition description.
    pub description: &'static str,
    /// Domains accepted by the condition target.
    pub target_domains: &'static [&'static str],
    /// Whether the condition supports any/all target behavior.
    pub supports_behavior: bool,
    /// Default any/all target behavior.
    pub behavior_default: &'static str,
    /// Whether the condition supports a duration field.
    pub supports_for: bool,
    /// Default duration string for the state-condition UI.
    pub for_default: &'static str,
    /// Whether the state-condition UI marks `for` required.
    pub for_required: bool,
}

/// Standard switch state-condition descriptions.
pub const SWITCH_CONDITION_DESCRIPTIONS: [SwitchConditionDescription; 2] = [
    SwitchConditionDescription {
        condition: SwitchCondition::IsOff,
        name: "Switch is off",
        description: "Tests if one or more switches are off.",
        target_domains: SWITCH_STATE_CONDITION_DOMAINS,
        supports_behavior: true,
        behavior_default: SWITCH_STATE_CONDITION_BEHAVIOR_DEFAULT,
        supports_for: true,
        for_default: SWITCH_STATE_CONDITION_FOR_DEFAULT,
        for_required: SWITCH_STATE_CONDITION_FOR_REQUIRED,
    },
    SwitchConditionDescription {
        condition: SwitchCondition::IsOn,
        name: "Switch is on",
        description: "Tests if one or more switches are on.",
        target_domains: SWITCH_STATE_CONDITION_DOMAINS,
        supports_behavior: true,
        behavior_default: SWITCH_STATE_CONDITION_BEHAVIOR_DEFAULT,
        supports_for: true,
        for_default: SWITCH_STATE_CONDITION_FOR_DEFAULT,
        for_required: SWITCH_STATE_CONDITION_FOR_REQUIRED,
    },
];

/// Device-condition types exposed by Home Assistant's toggle-device helper.
pub const SWITCH_DEVICE_CONDITIONS: &[SwitchCondition] =
    &[SwitchCondition::IsOff, SwitchCondition::IsOn];

/// Error returned when a service call is outside the standard switch contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitchCommandError {
    /// The call is for a domain other than `switch`.
    WrongDomain(String),
    /// The service name is not one of the three standard switch services.
    UnknownService(String),
    /// The service data contains an unsupported non-target field.
    UnknownServiceField(String),
    /// The target contains a field unknown to the shared target resolver.
    UnknownTargetField(String),
    /// Entity IDs were supplied in both the target and legacy data map.
    EntityIdInDataAndTarget,
}

impl fmt::Display for SwitchCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDomain(domain) => {
                write!(formatter, "expected switch domain, got {domain:?}")
            }
            Self::UnknownService(service) => {
                write!(formatter, "unknown switch service {service:?}")
            }
            Self::UnknownServiceField(field) => {
                write!(formatter, "unsupported switch service field {field:?}")
            }
            Self::UnknownTargetField(field) => {
                write!(formatter, "unsupported switch target field {field:?}")
            }
            Self::EntityIdInDataAndTarget => {
                formatter.write_str("entity_id must be supplied in target or data, not both")
            }
        }
    }
}

impl std::error::Error for SwitchCommandError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn service_call(
        service: &str,
        data: Map<String, Value>,
        target: Map<String, Value>,
    ) -> ServiceCall {
        ServiceCall {
            domain: SWITCH_DOMAIN.into(),
            service: service.into(),
            data,
            target,
            context: Context::default(),
        }
    }

    #[test]
    fn switch_descriptor_projects_registry_identity_and_original_metadata() {
        let descriptor = SwitchDescriptor::new("fixture", "switch-provider", "entry/outlet")
            .with_unique_id("outlet-17")
            .with_config_entry_id("config-1")
            .with_config_subentry_id("subentry-2")
            .with_device_id("device-3")
            .with_area_id("area-4")
            .with_suggested_object_id("kitchen_outlet")
            .with_name("Kitchen outlet")
            .with_icon("mdi:power-plug")
            .with_device_class(SwitchDeviceClass::Outlet)
            .with_entity_category("config")
            .with_has_entity_name(true)
            .with_translation_key("outlet")
            .with_enabled_by_default(false);
        let spec = descriptor.entity_spec();

        assert_eq!(spec.domain, SWITCH_DOMAIN);
        assert_eq!(spec.platform, "fixture");
        assert_eq!(spec.provider_id, "switch-provider");
        assert_eq!(spec.provider_key, "entry/outlet");
        assert_eq!(spec.unique_id.as_deref(), Some("outlet-17"));
        assert_eq!(spec.config_entry_id.as_deref(), Some("config-1"));
        assert_eq!(spec.config_subentry_id.as_deref(), Some("subentry-2"));
        assert_eq!(spec.device_id.as_deref(), Some("device-3"));
        assert_eq!(spec.area_id.as_deref(), Some("area-4"));
        assert_eq!(spec.suggested_object_id.as_deref(), Some("kitchen_outlet"));
        assert_eq!(spec.info.original_name.as_deref(), Some("Kitchen outlet"));
        assert_eq!(spec.info.original_icon.as_deref(), Some("mdi:power-plug"));
        assert_eq!(spec.info.original_device_class.as_deref(), Some("outlet"));
        assert_eq!(spec.info.entity_category.as_deref(), Some("config"));
        assert!(spec.info.has_entity_name);
        assert_eq!(spec.info.translation_key.as_deref(), Some("outlet"));
        assert!(!spec.enabled_by_default);
        assert_eq!(descriptor.device_class(), Some(SwitchDeviceClass::Outlet));
    }

    #[test]
    fn switch_device_classes_are_pinned_and_parse_case_insensitively() {
        assert_eq!(
            SwitchDeviceClass::ALL.map(SwitchDeviceClass::as_str),
            ["outlet", "switch"]
        );
        assert_eq!("OUTLET".parse(), Ok(SwitchDeviceClass::Outlet));
        assert_eq!("switch".parse(), Ok(SwitchDeviceClass::Switch));
        assert_eq!(
            "light"
                .parse::<SwitchDeviceClass>()
                .unwrap_err()
                .to_string(),
            "unknown switch device class \"light\""
        );
    }

    #[test]
    fn switch_snapshot_projects_known_unknown_unavailable_assumed_and_extra_values() {
        assert_eq!(SwitchSnapshot::new(Some(true)).state(), Some("on"));
        assert_eq!(SwitchSnapshot::new(Some(false)).state(), Some("off"));
        assert_eq!(SwitchSnapshot::new(None).state(), None);

        let context = Context {
            id: "context-1".into(),
            parent_id: Some("parent-1".into()),
            user_id: Some("user-1".into()),
        };
        let projected = SwitchSnapshot::new(None)
            .with_assumed_state(true)
            .with_context(context.clone())
            .with_extra_attribute("present_null", Some(Value::Null))
            .with_extra_attribute("omitted", None)
            .entity_snapshot();

        assert_eq!(projected.state, None);
        assert!(projected.available);
        assert_eq!(projected.context, context);
        assert_eq!(
            projected.attributes.get("assumed_state"),
            Some(&Value::Bool(true))
        );
        assert_eq!(projected.attributes.get("present_null"), Some(&Value::Null));
        assert!(!projected.attributes.contains_key("omitted"));

        let unavailable = SwitchSnapshot::unavailable(Some(true))
            .with_assumed_state(true)
            .into_entity_snapshot();
        assert_eq!(unavailable.state.as_deref(), Some("on"));
        assert!(!unavailable.available);
        assert_eq!(
            unavailable.attributes.get("assumed_state"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn switch_snapshot_merges_extra_attribute_maps_without_dropping_null() {
        let mut first = Map::new();
        first.insert("nullable".into(), Value::Null);
        let mut second = Map::new();
        second.insert("count".into(), json!(3));

        let projected = SwitchSnapshot::new(Some(false))
            .with_extra_attributes(first)
            .with_extra_attributes(second)
            .entity_snapshot();

        assert_eq!(projected.attributes.get("nullable"), Some(&Value::Null));
        assert_eq!(projected.attributes.get("count"), Some(&json!(3)));
    }

    #[test]
    fn switch_commands_are_strict_and_resolve_toggle_like_toggle_entity() {
        assert_eq!("turn_on".parse(), Ok(SwitchCommand::TurnOn));
        assert_eq!("turn_off".parse(), Ok(SwitchCommand::TurnOff));
        assert_eq!("toggle".parse(), Ok(SwitchCommand::Toggle));
        assert!("open_cover".parse::<SwitchCommand>().is_err());

        assert_eq!(SwitchCommand::TurnOn.resolve(Some(false)), SwitchAction::On);
        assert_eq!(
            SwitchCommand::TurnOff.resolve(Some(true)),
            SwitchAction::Off
        );
        assert_eq!(SwitchCommand::Toggle.resolve(Some(true)), SwitchAction::Off);
        assert_eq!(SwitchCommand::Toggle.resolve(Some(false)), SwitchAction::On);
        assert_eq!(SwitchCommand::Toggle.resolve(None), SwitchAction::On);
    }

    #[test]
    fn switch_service_call_accepts_standard_target_selectors_and_rejects_unknown_fields() {
        let mut target = Map::new();
        target.insert("device_id".into(), json!("device-1"));
        let parsed =
            SwitchCommand::parse_service_call(&service_call("turn_on", Map::new(), target))
                .unwrap();
        assert_eq!(parsed, SwitchCommand::TurnOn);

        let mut data = Map::new();
        data.insert("entity_id".into(), json!("switch.lamp"));
        let parsed =
            SwitchCommand::parse_service_call(&service_call("toggle", data, Map::new())).unwrap();
        assert_eq!(parsed, SwitchCommand::Toggle);

        let mut data = Map::new();
        data.insert("transition".into(), json!(1));
        assert_eq!(
            SwitchCommand::parse_service_call(&service_call("turn_on", data, Map::new())),
            Err(SwitchCommandError::UnknownServiceField("transition".into()))
        );

        let mut target = Map::new();
        target.insert("unknown_selector".into(), json!("value"));
        assert_eq!(
            SwitchCommand::parse_service_call(&service_call("turn_off", Map::new(), target)),
            Err(SwitchCommandError::UnknownTargetField(
                "unknown_selector".into()
            ))
        );

        let mut data = Map::new();
        data.insert("entity_id".into(), json!("switch.lamp"));
        let mut target = Map::new();
        target.insert("entity_id".into(), json!("switch.fan"));
        assert_eq!(
            SwitchCommand::parse_service_call(&service_call("toggle", data, target)),
            Err(SwitchCommandError::EntityIdInDataAndTarget)
        );
    }

    #[test]
    fn switch_service_metadata_matches_component_target_and_descriptions() {
        let turn_on = SwitchCommand::TurnOn.description();
        assert_eq!(turn_on.name, "Turn on switch");
        assert_eq!(turn_on.description, "Turns on a switch.");
        assert_eq!(turn_on.target_domain, SWITCH_DOMAIN);
        assert!(turn_on.fields.is_empty());
        assert_eq!(
            turn_on.metadata_json(),
            json!({
                "name": "Turn on switch",
                "description": "Turns on a switch.",
                "target": { "entity": { "domain": "switch" } }
            })
        );
        assert_eq!(SWITCH_SERVICE_DESCRIPTIONS.len(), 3);
    }

    #[test]
    fn switch_conditions_match_only_on_and_off() {
        assert!(SwitchCondition::IsOn.matches_state(Some("on")));
        assert!(!SwitchCondition::IsOn.matches_state(Some("off")));
        assert!(!SwitchCondition::IsOn.matches_state(Some("unknown")));
        assert!(!SwitchCondition::IsOn.matches_state(Some("unavailable")));
        assert!(!SwitchCondition::IsOn.matches_state(None));
        assert!(SwitchCondition::IsOff.matches_state(Some("off")));
        assert_eq!(SWITCH_STATE_CONDITION_DOMAINS, ["switch", "input_boolean"]);
        assert_eq!(SWITCH_CONDITION_BEHAVIORS, ["any", "all"]);
        assert_eq!(SWITCH_STATE_CONDITION_BEHAVIOR_DEFAULT, "any");
        assert_eq!(SWITCH_STATE_CONDITION_FOR_DEFAULT, "00:00:00");
        assert_eq!(SWITCH_STATE_CONDITION_FOR_FIELD, "for");
        assert_eq!(SWITCH_DEVICE_CONDITION_FOR_FIELD, "for");
        assert_eq!(
            SWITCH_DEVICE_CONDITION_FOR_TYPE,
            "positive_time_period_dict"
        );
    }
}
