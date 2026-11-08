//! Native light service-call parsing and normalization.

use super::color;
use super::named_colors::color_name_to_rgb;
use super::profiles::LightProfileCatalog;
use super::{
    LIGHT_DOMAIN, LightColorMode, LightDescriptor, LightError, LightFeature, LightFeatures,
    LightSnapshot, round_ties_even,
};
use hass_core::ServiceCall;
use serde_json::{Map, Value, json};
use std::fmt;
use std::str::FromStr;

/// Selector keys understood by the shared service-target resolver.
pub const LIGHT_TARGET_FIELDS: &[&str] =
    &["entity_id", "device_id", "area_id", "floor_id", "label_id"];

/// Standard parameter names accepted by `turn_on` and `toggle` at the pinned
/// Home Assistant revision.
pub const LIGHT_TURN_ON_ARGUMENTS: &[&str] = &[
    "brightness",
    "brightness_pct",
    "brightness_step",
    "brightness_step_pct",
    "color_name",
    "color_temp_kelvin",
    "effect",
    "flash",
    "hs_color",
    "profile",
    "rgb_color",
    "rgbw_color",
    "rgbww_color",
    "transition",
    "white",
    "xy_color",
];

/// Standard parameter names accepted by `turn_off`.
pub const LIGHT_TURN_OFF_ARGUMENTS: &[&str] = &["flash", "transition"];

/// All standard light service argument names at the pinned revision.
pub const LIGHT_SERVICE_ARGUMENTS: &[&str] = LIGHT_TURN_ON_ARGUMENTS;

/// Stable diagnostic code for an unknown `color_name` fallback.
pub const LIGHT_UNKNOWN_COLOR_NAME_DIAGNOSTIC: &str = "light.unknown_color_name";

/// One native light service.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightCommand {
    /// Turn the light on.
    TurnOn,
    /// Turn the light off.
    TurnOff,
    /// Toggle according to the current state.
    Toggle,
}

impl LightCommand {
    /// Return the standard service name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnOn => "turn_on",
            Self::TurnOff => "turn_off",
            Self::Toggle => "toggle",
        }
    }

    /// Parse a native service call and validate its target and parameter keys.
    pub fn parse_service_call(
        call: &ServiceCall,
    ) -> Result<LightServiceRequest, LightServiceError> {
        LightServiceRequest::parse(call)
    }
}

impl FromStr for LightCommand {
    type Err = LightServiceError;

    fn from_str(service: &str) -> Result<Self, Self::Err> {
        match service {
            "turn_on" => Ok(Self::TurnOn),
            "turn_off" => Ok(Self::TurnOff),
            "toggle" => Ok(Self::Toggle),
            _ => Err(LightServiceError::UnknownService(service.to_owned())),
        }
    }
}

impl fmt::Display for LightCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parsed, schema-checked native light service request.
#[derive(Clone, Debug, PartialEq)]
pub struct LightServiceRequest {
    /// The requested standard light operation.
    pub command: LightCommand,
    /// Unresolved selector values. Resolution remains the registry's job.
    pub target: Map<String, Value>,
    /// Validated service arguments in Home Assistant field names.
    pub params: Map<String, Value>,
}

impl LightServiceRequest {
    /// Parse and validate a light service call.
    pub fn parse(call: &ServiceCall) -> Result<Self, LightServiceError> {
        if call.domain != LIGHT_DOMAIN {
            return Err(LightServiceError::WrongDomain(call.domain.clone()));
        }
        let command = call.service.parse()?;
        let allowed_arguments = if command == LightCommand::TurnOff {
            LIGHT_TURN_OFF_ARGUMENTS
        } else {
            LIGHT_TURN_ON_ARGUMENTS
        };
        let mut target = call.target.clone();
        let mut params = call.data.clone();

        if params.contains_key("entity_id") && target.contains_key("entity_id") {
            return Err(LightServiceError::EntityIdInDataAndTarget);
        }
        for field in LIGHT_TARGET_FIELDS {
            if let Some(value) = params.remove(*field) {
                if target.contains_key(*field) {
                    return Err(LightServiceError::DuplicateTargetField((*field).to_owned()));
                }
                target.insert((*field).to_owned(), value);
            }
        }
        if let Some(field) = target
            .keys()
            .find(|field| !LIGHT_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(LightServiceError::UnknownTargetField(field.clone()));
        }
        if let Some(field) = params
            .keys()
            .find(|field| !allowed_arguments.contains(&field.as_str()))
        {
            return Err(LightServiceError::UnknownServiceField(field.clone()));
        }
        validate_params(command, &mut params)?;
        Ok(Self {
            command,
            target,
            params,
        })
    }

    /// Normalize standard arguments using the built-in profile catalog.
    ///
    /// This compatibility entry point has no registry-resolved entity ID, so
    /// it cannot apply entity-specific default profiles. Use
    /// [`LightServiceRequest::normalize_with_context`] when the caller has the
    /// current entity ID and a host-loaded catalog.
    pub fn normalize(
        &self,
        descriptor: &LightDescriptor,
        current: &LightSnapshot,
    ) -> Result<NormalizedLightCommand, LightServiceError> {
        let profiles = LightProfileCatalog::built_in();
        let context = LightNormalizationContext::without_entity_id(&profiles);
        self.normalize_with_context(descriptor, current, &context)
    }

    /// Normalize using the registry-resolved entity ID and the host's ready
    /// profile catalog. The SDK does not read files or maintain a global
    /// catalog.
    pub fn normalize_with_context(
        &self,
        descriptor: &LightDescriptor,
        current: &LightSnapshot,
        context: &LightNormalizationContext<'_>,
    ) -> Result<NormalizedLightCommand, LightServiceError> {
        descriptor.validate()?;
        let modes = descriptor
            .supported_color_modes()
            .expect("validated supported color modes are present");
        let features = descriptor.supported_features();

        let mut params = self.params.clone();
        let has_brightness_step =
            params.contains_key("brightness_step") || params.contains_key("brightness_step_pct");
        let mut diagnostics = Vec::new();

        // This is the common pre-fanout preprocessing path. Brightness steps
        // defer alternatives until the per-entity current state is available.
        if self.command != LightCommand::TurnOff && !has_brightness_step {
            preprocess_turn_on_alternatives(
                &mut params,
                context.profile_catalog,
                &mut diagnostics,
            )?;
        }

        let mut action = match self.command {
            LightCommand::TurnOn => LightAction::On,
            LightCommand::TurnOff => LightAction::Off,
            LightCommand::Toggle if current.is_on() == Some(true) => LightAction::Off,
            LightCommand::Toggle => LightAction::On,
        };
        // A zero-brightness turn-on reroutes the original, commonly
        // preprocessed call through the off handler, not its per-entity copy.
        let off_request_params = params.clone();

        if action == LightAction::On {
            if has_brightness_step {
                process_brightness_step(&mut params, current)?;
                preprocess_turn_on_alternatives(
                    &mut params,
                    context.profile_catalog,
                    &mut diagnostics,
                )?;
            }
            apply_default_profiles(&mut params, context, current.is_on());
            process_colors(&mut params, descriptor, current, &modes)?;
            if params.get("brightness").and_then(Value::as_u64) == Some(0)
                || params.get("white").and_then(Value::as_u64) == Some(0)
            {
                action = LightAction::Off;
                params = off_request_params;
            }
        }

        if action == LightAction::Off {
            // Core's turn-off path supplies `state_on = True` when consulting
            // defaults. For a nonempty request this fills only transition;
            // an empty request can apply the full default profile first.
            if !params.contains_key("transition") {
                apply_default_profiles(&mut params, context, Some(true));
            }
            let mut off_params = Map::new();
            if features.contains(LightFeature::Transition)
                && let Some(value) = params.get("transition")
            {
                off_params.insert("transition".into(), value.clone());
            }
            if features.contains(LightFeature::Flash)
                && let Some(value) = params.get("flash")
            {
                off_params.insert("flash".into(), value.clone());
            }
            params = off_params;
        } else {
            filter_turn_on_params(&mut params, &modes, features);
        }

        Ok(NormalizedLightCommand {
            action,
            target: self.target.clone(),
            params,
            diagnostics,
        })
    }
}

/// Host-owned normalization context for light profiles.
#[derive(Clone, Copy, Debug)]
pub struct LightNormalizationContext<'a> {
    resolved_entity_id: Option<&'a str>,
    profile_catalog: &'a LightProfileCatalog,
}

impl<'a> LightNormalizationContext<'a> {
    /// Use an entity ID resolved from the host's live registry.
    #[must_use]
    pub const fn new(
        resolved_entity_id: &'a str,
        profile_catalog: &'a LightProfileCatalog,
    ) -> Self {
        Self {
            resolved_entity_id: Some(resolved_entity_id),
            profile_catalog,
        }
    }

    /// Build a compatibility context without inferring an ID from a
    /// descriptor. Group defaults can still be applied from the catalog.
    #[must_use]
    pub const fn without_entity_id(profile_catalog: &'a LightProfileCatalog) -> Self {
        Self {
            resolved_entity_id: None,
            profile_catalog,
        }
    }

    /// Return the canonical registry-resolved entity ID, when supplied.
    #[must_use]
    pub const fn resolved_entity_id(self) -> Option<&'a str> {
        self.resolved_entity_id
    }

    /// Return the ready catalog supplied by the host.
    #[must_use]
    pub const fn profile_catalog(self) -> &'a LightProfileCatalog {
        self.profile_catalog
    }
}

/// Final action requested of a native light provider.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightAction {
    /// Request that the provider turn the light on.
    On,
    /// Request that the provider turn the light off.
    Off,
}

/// Normalized action and still-unresolved target selectors ready for a light
/// provider adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedLightCommand {
    /// Requested provider action.
    pub action: LightAction,
    /// Target selector values left for the shared registry resolver.
    pub target: Map<String, Value>,
    /// Parameters filtered to the light's advertised features and color modes.
    pub params: Map<String, Value>,
    /// Nonfatal diagnostics generated while applying common service data.
    pub diagnostics: Vec<LightServiceDiagnostic>,
}

/// Nonfatal, source-aligned diagnostic attached to a normalized command.
///
/// Providers and hosts decide whether and how to log it. Retaining the
/// original field and value lets a request boundary deduplicate warnings
/// across all resolved target entities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightServiceDiagnostic {
    /// Stable SDK diagnostic identifier.
    pub code: &'static str,
    /// Input service field that caused this diagnostic.
    pub original_field: &'static str,
    /// Exact original string value from the parsed request.
    pub original_value: String,
}

/// Error returned while parsing or normalizing a light service request.
#[derive(Clone, Debug, PartialEq)]
pub enum LightServiceError {
    /// The call is for a domain other than `light`.
    WrongDomain(String),
    /// The service name is not one of the standard light services.
    UnknownService(String),
    /// The service data contains an unsupported field.
    UnknownServiceField(String),
    /// The target contains an unsupported selector.
    UnknownTargetField(String),
    /// One target selector was supplied in both data and target.
    DuplicateTargetField(String),
    /// Entity IDs were supplied in both target and data.
    EntityIdInDataAndTarget,
    /// A pinned light argument had an invalid value.
    InvalidParameter {
        /// Invalid service field name.
        field: String,
        /// Short reason for rejection.
        reason: &'static str,
    },
    /// Reserved compatibility error for provider-owned light parameters.
    ProviderOwnedParameter(&'static str),
    /// Light descriptor or state projection error.
    Light(LightError),
}

impl fmt::Display for LightServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDomain(domain) => write!(formatter, "expected light domain, got {domain:?}"),
            Self::UnknownService(service) => write!(formatter, "unknown light service {service:?}"),
            Self::UnknownServiceField(field) => {
                write!(formatter, "unsupported light service field {field:?}")
            }
            Self::UnknownTargetField(field) => {
                write!(formatter, "unsupported light target field {field:?}")
            }
            Self::DuplicateTargetField(field) => {
                write!(formatter, "target selector {field:?} was supplied twice")
            }
            Self::EntityIdInDataAndTarget => {
                formatter.write_str("entity_id must be supplied in target or data, not both")
            }
            Self::InvalidParameter { field, reason } => {
                write!(formatter, "invalid light parameter {field:?}: {reason}")
            }
            Self::ProviderOwnedParameter(field) => write!(
                formatter,
                "light parameter {field:?} requires integration-owned data"
            ),
            Self::Light(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LightServiceError {}

impl From<LightError> for LightServiceError {
    fn from(error: LightError) -> Self {
        Self::Light(error)
    }
}

fn validate_params(
    command: LightCommand,
    params: &mut Map<String, Value>,
) -> Result<(), LightServiceError> {
    if command != LightCommand::TurnOff {
        let brightness_fields = [
            "brightness",
            "brightness_pct",
            "brightness_step",
            "brightness_step_pct",
        ];
        if brightness_fields
            .iter()
            .filter(|field| params.contains_key(**field))
            .count()
            > 1
        {
            return invalid("brightness", "brightness forms are mutually exclusive");
        }
        let color_fields = [
            "color_name",
            "color_temp_kelvin",
            "hs_color",
            "profile",
            "rgb_color",
            "rgbw_color",
            "rgbww_color",
            "white",
            "xy_color",
        ];
        if color_fields
            .iter()
            .filter(|field| params.contains_key(**field))
            .count()
            > 1
        {
            return invalid("color", "color forms are mutually exclusive");
        }
    }

    let fields = params.keys().cloned().collect::<Vec<_>>();
    for field in fields {
        let value = params
            .get(&field)
            .expect("field came from this map")
            .clone();
        let normalized = match field.as_str() {
            "brightness" => json!(clamp_integer(&field, &value, 0, 255)?),
            "brightness_pct" => json!(percentage(&field, &value)?),
            "brightness_step" => json!(clamp_integer(&field, &value, -255, 255)?),
            "brightness_step_pct" => json!(clamp_number(&field, &value, -100.0, 100.0)?),
            "transition" => json!(clamp_number(&field, &value, 0.0, 6_553.0)?),
            "color_temp_kelvin" => {
                let kelvin = integer(&field, &value)?;
                if kelvin <= 0 {
                    return invalid(&field, "must be a positive integer");
                }
                json!(kelvin)
            }
            "hs_color" => {
                let hs = number_pair(&field, &value)?;
                if !(0.0..=360.0).contains(&hs[0]) || !(0.0..=100.0).contains(&hs[1]) {
                    return invalid(&field, "expected [hue 0..360, saturation 0..100]");
                }
                json!(hs)
            }
            "rgb_color" => json!(byte_array::<3>(&field, &value)?),
            "rgbw_color" => json!(byte_array::<4>(&field, &value)?),
            "rgbww_color" => json!(byte_array::<5>(&field, &value)?),
            "xy_color" => {
                let xy = number_pair(&field, &value)?;
                if xy.iter().any(|component| !component.is_finite()) {
                    return invalid(&field, "coordinates must be finite numbers");
                }
                json!(xy)
            }
            "effect" | "color_name" | "profile" => {
                if value.as_str().is_none() {
                    return invalid(&field, "expected a string");
                }
                value
            }
            "flash" => match value.as_str() {
                Some("short" | "long") => value,
                _ => return invalid(&field, "expected short or long"),
            },
            "white" => {
                if value == Value::Bool(true) {
                    Value::Bool(true)
                } else {
                    json!(clamp_integer(&field, &value, 0, 255)?)
                }
            }
            _ => return Err(LightServiceError::UnknownServiceField(field)),
        };
        params.insert(field, normalized);
    }
    Ok(())
}

fn preprocess_turn_on_alternatives(
    params: &mut Map<String, Value>,
    profiles: &LightProfileCatalog,
    diagnostics: &mut Vec<LightServiceDiagnostic>,
) -> Result<(), LightServiceError> {
    if params.contains_key("brightness_step") || params.contains_key("brightness_step_pct") {
        return Ok(());
    }

    if let Some(Value::String(name)) = params.remove("profile") {
        apply_profile(&name, params, profiles);
    }

    if let Some(value) = params.remove("color_name") {
        let name = value
            .as_str()
            .expect("validated color_name must be a string");
        let original_value = name.to_owned();
        let rgb = color_name_to_rgb(name).unwrap_or_else(|| {
            diagnostics.push(LightServiceDiagnostic {
                code: LIGHT_UNKNOWN_COLOR_NAME_DIAGNOSTIC,
                original_field: "color_name",
                original_value: original_value.clone(),
            });
            [255, 255, 255]
        });
        params.insert("rgb_color".into(), json!(rgb));
    }

    if let Some(value) = params.remove("brightness_pct") {
        let percentage = number("brightness_pct", &value)?;
        let brightness = round_ties_even(255.0 * percentage / 100.0).clamp(0.0, 255.0) as u8;
        params.insert("brightness".into(), json!(brightness));
    }

    Ok(())
}

fn apply_profile(name: &str, params: &mut Map<String, Value>, profiles: &LightProfileCatalog) {
    let Some(profile) = profiles.get(name) else {
        return;
    };

    let color_fields = [
        "color_name",
        "color_temp_kelvin",
        "hs_color",
        "rgb_color",
        "rgbw_color",
        "rgbww_color",
        "xy_color",
        "white",
    ];
    if let Some(hs_color) = profile.hs_color()
        && !color_fields.iter().any(|field| params.contains_key(*field))
    {
        params.insert("hs_color".into(), json!(hs_color));
    }
    if let Some(brightness) = profile.brightness {
        params.entry("brightness").or_insert(json!(brightness));
    }
    if let Some(transition) = profile.transition {
        params.entry("transition").or_insert(json!(transition));
    }
}

fn apply_default_profiles(
    params: &mut Map<String, Value>,
    context: &LightNormalizationContext<'_>,
    state_on: Option<bool>,
) {
    if let Some(entity_id) = context.resolved_entity_id {
        apply_default_profile(
            &format!("{entity_id}.default"),
            params,
            context.profile_catalog,
            state_on,
        );
    }
    apply_default_profile(
        "group.all_lights.default",
        params,
        context.profile_catalog,
        state_on,
    );
}

fn apply_default_profile(
    profile_name: &str,
    params: &mut Map<String, Value>,
    profiles: &LightProfileCatalog,
    state_on: Option<bool>,
) {
    let Some(profile) = profiles.get(profile_name) else {
        return;
    };
    if state_on != Some(true) || params.is_empty() {
        apply_profile(&profile.name, params, profiles);
    } else if let Some(transition) = profile.transition {
        params.entry("transition").or_insert(json!(transition));
    }
}

fn process_brightness_step(
    params: &mut Map<String, Value>,
    current: &LightSnapshot,
) -> Result<(), LightServiceError> {
    if let Some(value) = params.remove("brightness_step") {
        let base = if current.is_on() == Some(true) {
            current.brightness().unwrap_or_default()
        } else {
            0
        };
        let brightness =
            (i64::from(base) + integer("brightness_step", &value)?).clamp(0, 255) as u8;
        params.insert("brightness".into(), json!(brightness));
    } else if let Some(value) = params.remove("brightness_step_pct") {
        let base = if current.is_on() == Some(true) {
            current.brightness().unwrap_or_default()
        } else {
            0
        };
        let current_pct = round_ties_even(f64::from(base) / 255.0 * 100.0);
        let requested = (current_pct + number("brightness_step_pct", &value)?).clamp(0.0, 100.0);
        let brightness = round_ties_even(requested / 100.0 * 255.0).clamp(0.0, 255.0) as u8;
        params.insert("brightness".into(), json!(brightness));
    }
    Ok(())
}

fn process_colors(
    params: &mut Map<String, Value>,
    descriptor: &LightDescriptor,
    current: &LightSnapshot,
    modes: &[LightColorMode],
) -> Result<(), LightServiceError> {
    let rgb_color;
    if let Some(value) = params.get("color_temp_kelvin").cloned()
        && !modes.contains(&LightColorMode::ColorTemp)
    {
        let temperature = u32::try_from(integer("color_temp_kelvin", &value)?)
            .map_err(|_| invalid_error("color_temp_kelvin", "Kelvin value is too large"))?;
        params.remove("color_temp_kelvin");
        if modes.contains(&LightColorMode::Rgbww) {
            let brightness = params
                .get("brightness")
                .and_then(Value::as_u64)
                .map_or(current.brightness().unwrap_or(255), |value| value as u8);
            let rgbww = color::kelvin_to_rgbww(
                temperature,
                brightness,
                descriptor.min_color_temp_kelvin(),
                descriptor.max_color_temp_kelvin(),
            )?;
            params.insert("rgbww_color".into(), json!(rgbww));
        } else if modes.iter().any(|mode| mode.is_color()) {
            params.insert("hs_color".into(), json!(color::kelvin_to_hs(temperature)));
        }
    }

    if params.contains_key("hs_color") && !modes.contains(&LightColorMode::Hs) {
        let hs = number_pair("hs_color", params.get("hs_color").expect("checked"))?;
        params.remove("hs_color");
        if modes.contains(&LightColorMode::Rgb) {
            params.insert("rgb_color".into(), json!(color::hs_to_rgb(hs[0], hs[1])));
        } else if modes.contains(&LightColorMode::Rgbw) {
            let rgb = color::hs_to_rgb(hs[0], hs[1]);
            params.insert("rgbw_color".into(), json!(color::rgb_to_rgbw(rgb)));
        } else if modes.contains(&LightColorMode::Rgbww) {
            let rgb = color::hs_to_rgb(hs[0], hs[1]);
            params.insert(
                "rgbww_color".into(),
                json!(color::rgb_to_rgbww(
                    rgb,
                    descriptor.min_color_temp_kelvin(),
                    descriptor.max_color_temp_kelvin(),
                )?),
            );
        } else if modes.contains(&LightColorMode::Xy) {
            params.insert(
                "xy_color".into(),
                json!(color::rgb_to_xy(color::hs_to_rgb(hs[0], hs[1]))),
            );
        } else if modes.contains(&LightColorMode::ColorTemp) {
            let xy = color::rgb_to_xy(color::hs_to_rgb(hs[0], hs[1]));
            params.insert(
                "color_temp_kelvin".into(),
                json!(xy_temperature(xy, descriptor)?),
            );
        }
    } else if params.contains_key("rgb_color") && !modes.contains(&LightColorMode::Rgb) {
        rgb_color = byte_array::<3>("rgb_color", params.get("rgb_color").expect("checked"))?;
        params.remove("rgb_color");
        if modes.contains(&LightColorMode::Rgbw) {
            params.insert("rgbw_color".into(), json!(color::rgb_to_rgbw(rgb_color)));
        } else if modes.contains(&LightColorMode::Rgbww) {
            params.insert(
                "rgbww_color".into(),
                json!(color::rgb_to_rgbww(
                    rgb_color,
                    descriptor.min_color_temp_kelvin(),
                    descriptor.max_color_temp_kelvin(),
                )?),
            );
        } else if modes.contains(&LightColorMode::Hs) {
            params.insert("hs_color".into(), json!(color::rgb_to_hs(rgb_color)));
        } else if modes.contains(&LightColorMode::Xy) {
            params.insert("xy_color".into(), json!(color::rgb_to_xy(rgb_color)));
        } else if modes.contains(&LightColorMode::ColorTemp) {
            let xy = color::rgb_to_xy(rgb_color);
            params.insert(
                "color_temp_kelvin".into(),
                json!(xy_temperature(xy, descriptor)?),
            );
        }
    } else if params.contains_key("xy_color") && !modes.contains(&LightColorMode::Xy) {
        let xy = number_pair("xy_color", params.get("xy_color").expect("checked"))?;
        params.remove("xy_color");
        if modes.contains(&LightColorMode::Hs) {
            params.insert(
                "hs_color".into(),
                json!(color::rgb_to_hs(color::xy_to_rgb(xy))),
            );
        } else if modes.contains(&LightColorMode::Rgb) {
            params.insert("rgb_color".into(), json!(color::xy_to_rgb(xy)));
        } else if modes.contains(&LightColorMode::Rgbw) {
            params.insert(
                "rgbw_color".into(),
                json!(color::rgb_to_rgbw(color::xy_to_rgb(xy))),
            );
        } else if modes.contains(&LightColorMode::Rgbww) {
            params.insert(
                "rgbww_color".into(),
                json!(color::rgb_to_rgbww(
                    color::xy_to_rgb(xy),
                    descriptor.min_color_temp_kelvin(),
                    descriptor.max_color_temp_kelvin(),
                )?),
            );
        } else if modes.contains(&LightColorMode::ColorTemp) {
            params.insert(
                "color_temp_kelvin".into(),
                json!(xy_temperature(xy, descriptor)?),
            );
        }
    } else if params.contains_key("rgbw_color") && !modes.contains(&LightColorMode::Rgbw) {
        let rgbw = byte_array::<4>("rgbw_color", params.get("rgbw_color").expect("checked"))?;
        params.remove("rgbw_color");
        rgb_color = color::rgbw_to_rgb(rgbw);
        if modes.contains(&LightColorMode::Rgb) {
            params.insert("rgb_color".into(), json!(rgb_color));
        } else if modes.contains(&LightColorMode::Rgbww) {
            params.insert(
                "rgbww_color".into(),
                json!(color::rgb_to_rgbww(
                    rgb_color,
                    descriptor.min_color_temp_kelvin(),
                    descriptor.max_color_temp_kelvin(),
                )?),
            );
        } else if modes.contains(&LightColorMode::Hs) {
            params.insert("hs_color".into(), json!(color::rgb_to_hs(rgb_color)));
        } else if modes.contains(&LightColorMode::Xy) {
            params.insert("xy_color".into(), json!(color::rgb_to_xy(rgb_color)));
        } else if modes.contains(&LightColorMode::ColorTemp) {
            params.insert(
                "color_temp_kelvin".into(),
                json!(xy_temperature(color::rgb_to_xy(rgb_color), descriptor)?),
            );
        }
    } else if params.contains_key("rgbww_color") && !modes.contains(&LightColorMode::Rgbww) {
        let rgbww = byte_array::<5>("rgbww_color", params.get("rgbww_color").expect("checked"))?;
        params.remove("rgbww_color");
        rgb_color = color::rgbww_to_rgb(
            rgbww,
            descriptor.min_color_temp_kelvin(),
            descriptor.max_color_temp_kelvin(),
        )?;
        if modes.contains(&LightColorMode::Rgb) {
            params.insert("rgb_color".into(), json!(rgb_color));
        } else if modes.contains(&LightColorMode::Rgbw) {
            params.insert("rgbw_color".into(), json!(color::rgb_to_rgbw(rgb_color)));
        } else if modes.contains(&LightColorMode::Hs) {
            params.insert("hs_color".into(), json!(color::rgb_to_hs(rgb_color)));
        } else if modes.contains(&LightColorMode::Xy) {
            params.insert("xy_color".into(), json!(color::rgb_to_xy(rgb_color)));
        } else if modes.contains(&LightColorMode::ColorTemp) {
            params.insert(
                "color_temp_kelvin".into(),
                json!(xy_temperature(color::rgb_to_xy(rgb_color), descriptor)?),
            );
        }
    }

    if params.get("white") == Some(&Value::Bool(true)) {
        params.insert(
            "white".into(),
            current
                .brightness()
                .map_or(Value::Null, |value| json!(value)),
        );
    }
    if modes.contains(&LightColorMode::White) && params.contains_key("white") {
        let white = params
            .remove("brightness")
            .unwrap_or_else(|| params.get("white").cloned().unwrap_or(Value::Null));
        params.insert("white".into(), white);
    }
    Ok(())
}

fn xy_temperature(xy: [f64; 2], descriptor: &LightDescriptor) -> Result<u32, LightServiceError> {
    let value = match color::xy_to_kelvin(xy) {
        Ok(value) => value,
        Err(LightError::InvalidColorConversion) if xy[1] == 0.1858 => {
            descriptor.max_color_temp_kelvin()
        }
        Err(error) => return Err(error.into()),
    };
    Ok(value.clamp(
        descriptor.min_color_temp_kelvin(),
        descriptor.max_color_temp_kelvin(),
    ))
}

fn filter_turn_on_params(
    params: &mut Map<String, Value>,
    modes: &[LightColorMode],
    features: LightFeatures,
) {
    if !features.contains(LightFeature::Effect) {
        params.remove("effect");
    }
    if !features.contains(LightFeature::Flash) {
        params.remove("flash");
    }
    if !features.contains(LightFeature::Transition) {
        params.remove("transition");
    }
    if !modes.iter().any(|mode| mode.supports_brightness()) {
        params.remove("brightness");
    }
    let mode_for_field = [
        ("color_temp_kelvin", LightColorMode::ColorTemp),
        ("hs_color", LightColorMode::Hs),
        ("rgb_color", LightColorMode::Rgb),
        ("rgbw_color", LightColorMode::Rgbw),
        ("rgbww_color", LightColorMode::Rgbww),
        ("white", LightColorMode::White),
        ("xy_color", LightColorMode::Xy),
    ];
    for (field, mode) in mode_for_field {
        if !modes.contains(&mode) {
            params.remove(field);
        }
    }
}

fn number_pair(field: &str, value: &Value) -> Result<[f64; 2], LightServiceError> {
    let Some(values) = value.as_array() else {
        return invalid(field, "expected an array with exactly two numeric values");
    };
    if values.len() != 2 {
        return invalid(field, "expected an array with exactly two numeric values");
    }
    Ok([number(field, &values[0])?, number(field, &values[1])?])
}

fn byte_array<const N: usize>(field: &str, value: &Value) -> Result<[u8; N], LightServiceError> {
    let Some(values) = value.as_array() else {
        return invalid(field, "expected an array of byte values");
    };
    if values.len() != N {
        return invalid(field, "color array has the wrong number of channels");
    }
    let mut result = [0; N];
    for (target, value) in result.iter_mut().zip(values) {
        let component = integer(field, value)?;
        if !(0..=255).contains(&component) {
            return invalid(field, "channel values must be in 0..255");
        }
        *target = component as u8;
    }
    Ok(result)
}

fn percentage(field: &str, value: &Value) -> Result<f64, LightServiceError> {
    let percentage = number(field, value)?;
    if !(0.0..=100.0).contains(&percentage) {
        return invalid(field, "must be in 0..100");
    }
    Ok(percentage)
}

fn clamp_number(
    field: &str,
    value: &Value,
    minimum: f64,
    maximum: f64,
) -> Result<f64, LightServiceError> {
    Ok(number(field, value)?.clamp(minimum, maximum))
}

fn clamp_integer(
    field: &str,
    value: &Value,
    minimum: i64,
    maximum: i64,
) -> Result<i64, LightServiceError> {
    Ok(integer(field, value)?.clamp(minimum, maximum))
}

fn integer(field: &str, value: &Value) -> Result<i64, LightServiceError> {
    let number = if let Some(value) = value.as_i64() {
        value
    } else if let Some(value) = value.as_u64() {
        i64::try_from(value).map_err(|_| invalid_error(field, "integer is too large"))?
    } else if let Some(value) = value.as_bool() {
        i64::from(value)
    } else if let Some(value) = value.as_f64() {
        if !value.is_finite() || value.trunc() < i64::MIN as f64 || value.trunc() > i64::MAX as f64
        {
            return invalid(field, "expected a finite integer");
        }
        value.trunc() as i64
    } else if let Some(value) = value.as_str() {
        value
            .parse::<i64>()
            .map_err(|_| invalid_error(field, "expected an integer"))?
    } else {
        return invalid(field, "expected an integer");
    };
    Ok(number)
}

fn number(field: &str, value: &Value) -> Result<f64, LightServiceError> {
    let number = if let Some(value) = value.as_f64() {
        value
    } else if let Some(value) = value.as_str() {
        value
            .parse::<f64>()
            .map_err(|_| invalid_error(field, "expected a number"))?
    } else {
        return invalid(field, "expected a number");
    };
    if number.is_finite() {
        Ok(number)
    } else {
        invalid(field, "expected a finite number")
    }
}

fn invalid<T>(field: &str, reason: &'static str) -> Result<T, LightServiceError> {
    Err(invalid_error(field, reason))
}

fn invalid_error(field: &str, reason: &'static str) -> LightServiceError {
    LightServiceError::InvalidParameter {
        field: field.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::Context;

    fn call(service: &str, data: Map<String, Value>, target: Map<String, Value>) -> ServiceCall {
        ServiceCall {
            domain: LIGHT_DOMAIN.into(),
            service: service.into(),
            data,
            target,
            context: Context::default(),
        }
    }

    #[test]
    fn parses_targets_and_normalizes_brightness_and_color_to_native_mode() {
        let mut data = Map::new();
        data.insert("entity_id".into(), json!("light.desk"));
        data.insert("brightness_pct".into(), json!(50));
        data.insert("hs_color".into(), json!([0, 100]));
        data.insert("transition".into(), json!(12));
        let request = LightCommand::parse_service_call(&call("turn_on", data, Map::new())).unwrap();
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Rgb])
            .with_feature(LightFeature::Transition);
        let normalized = request
            .normalize(&descriptor, &LightSnapshot::new(Some(false)))
            .unwrap();
        assert_eq!(normalized.action, LightAction::On);
        assert_eq!(
            normalized.target.get("entity_id"),
            Some(&json!("light.desk"))
        );
        assert_eq!(normalized.params.get("brightness"), Some(&json!(128)));
        assert_eq!(
            normalized.params.get("rgb_color"),
            Some(&json!([255, 0, 0]))
        );
        assert_eq!(normalized.params.get("transition"), Some(&json!(12.0)));
        assert!(!normalized.params.contains_key("hs_color"));
    }

    #[test]
    fn brightness_steps_use_current_light_state_and_clamp_to_home_assistant_range() {
        let mut data = Map::new();
        data.insert("brightness_step_pct".into(), json!(80));
        let request = LightServiceRequest::parse(&call("turn_on", data, Map::new())).unwrap();
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Brightness]);
        let normalized = request
            .normalize(
                &descriptor,
                &LightSnapshot::new(Some(true)).with_brightness(Some(200)),
            )
            .unwrap();
        assert_eq!(normalized.params.get("brightness"), Some(&json!(255)));
    }

    #[test]
    fn zero_brightness_turns_on_request_into_off_and_filters_on_only_fields() {
        let mut data = Map::new();
        data.insert("brightness".into(), json!(0));
        data.insert("effect".into(), json!("rainbow"));
        data.insert("transition".into(), json!(2));
        let request = LightServiceRequest::parse(&call("turn_on", data, Map::new())).unwrap();
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Brightness])
            .with_feature(LightFeature::Transition);
        let normalized = request
            .normalize(&descriptor, &LightSnapshot::new(Some(true)))
            .unwrap();
        assert_eq!(normalized.action, LightAction::Off);
        assert_eq!(
            normalized.params,
            Map::from_iter([("transition".into(), json!(2.0))])
        );
    }

    #[test]
    fn xy_to_temperature_uses_the_pinned_singularity_fallback() {
        let data = Map::from_iter([("xy_color".into(), json!([0.3320, 0.1858]))]);
        let request = LightServiceRequest::parse(&call("turn_on", data, Map::new())).unwrap();
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::ColorTemp])
            .with_min_color_temp_kelvin(2_000)
            .with_max_color_temp_kelvin(6_500);
        let normalized = request
            .normalize(&descriptor, &LightSnapshot::new(Some(false)))
            .unwrap();
        assert_eq!(
            normalized.params.get("color_temp_kelvin"),
            Some(&json!(6_500))
        );
    }

    #[test]
    fn rejects_schema_conflicts_unknown_target_fields_and_invalid_color_arrays() {
        let mut conflict = Map::new();
        conflict.insert("brightness".into(), json!(10));
        conflict.insert("brightness_pct".into(), json!(10));
        assert!(LightServiceRequest::parse(&call("turn_on", conflict, Map::new())).is_err());

        let invalid_target = Map::from_iter([("object_id".into(), json!("desk"))]);
        assert!(matches!(
            LightServiceRequest::parse(&call("toggle", Map::new(), invalid_target)),
            Err(LightServiceError::UnknownTargetField(_))
        ));

        let invalid_color = Map::from_iter([("rgb_color".into(), json!([10, 20]))]);
        assert!(matches!(
            LightServiceRequest::parse(&call("turn_on", invalid_color, Map::new())),
            Err(LightServiceError::InvalidParameter { .. })
        ));
    }

    #[test]
    fn named_colors_convert_and_unknown_names_fall_back_with_original_diagnostic() {
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Rgb]);
        let profiles = LightProfileCatalog::default();
        let context = LightNormalizationContext::new("light.desk", &profiles);

        let known = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("color_name".into(), json!("Light Blue"))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
        .unwrap();
        assert_eq!(known.params.get("rgb_color"), Some(&json!([173, 216, 230])));
        assert!(known.diagnostics.is_empty());

        let unknown = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("color_name".into(), json!("No Such Color"))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
        .unwrap();
        assert_eq!(
            unknown.params.get("rgb_color"),
            Some(&json!([255, 255, 255]))
        );
        assert_eq!(
            unknown.diagnostics,
            vec![LightServiceDiagnostic {
                code: LIGHT_UNKNOWN_COLOR_NAME_DIAGNOSTIC,
                original_field: "color_name",
                original_value: "No Such Color".into(),
            }]
        );
    }

    #[test]
    fn explicit_profiles_apply_defaults_and_caller_brightness_forms_win() {
        let profiles = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\n",
            "soft,,,,7000\n"
        )]);
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Brightness])
            .with_feature(LightFeature::Transition);
        let context = LightNormalizationContext::new("light.desk", &profiles);

        let explicit = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("profile".into(), json!("soft"))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
        .unwrap();
        assert_eq!(explicit.params.get("transition"), Some(&json!(6_553.0)));

        let caller_percent = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\n",
            "soft,,,,90\n"
        )]);
        let context = LightNormalizationContext::new("light.desk", &caller_percent);
        let percentage = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([
                ("profile".into(), json!("soft")),
                ("brightness_pct".into(), json!(50)),
            ]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
        .unwrap();
        assert_eq!(percentage.params.get("brightness"), Some(&json!(128)));

        let step_profile = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\n",
            "soft,,,,90\n"
        )]);
        let context = LightNormalizationContext::new("light.desk", &step_profile);
        let stepped = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([
                ("profile".into(), json!("soft")),
                ("brightness_step".into(), json!(25)),
            ]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(
            &descriptor,
            &LightSnapshot::new(Some(true)).with_brightness(Some(100)),
            &context,
        )
        .unwrap();
        assert_eq!(stepped.params.get("brightness"), Some(&json!(125)));
    }

    #[test]
    fn zero_brightness_step_reroutes_original_params_and_uses_only_off_defaults() {
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Brightness])
            .with_feature(LightFeature::Transition);
        for (csv, explicit_transition, expected_transition) in [
            ("name,x,y,brightness,transition\nsoft,,,,4\n", None, None),
            (
                "name,x,y,brightness,transition\nsoft,,,,4\nlight.desk.default,,,,7\n",
                None,
                Some(7.0),
            ),
            (
                "name,x,y,brightness,transition\nsoft,,,,4\nlight.desk.default,,,,7\n",
                Some(2.0),
                Some(2.0),
            ),
        ] {
            let profiles = LightProfileCatalog::from_csv_sources([csv]);
            let context = LightNormalizationContext::new("light.desk", &profiles);
            for step in ["brightness_step", "brightness_step_pct"] {
                let mut params =
                    Map::from_iter([("profile".into(), json!("soft")), (step.into(), json!(-1))]);
                if let Some(transition) = explicit_transition {
                    params.insert("transition".into(), json!(transition));
                }
                let normalized = LightServiceRequest::parse(&call("turn_on", params, Map::new()))
                    .unwrap()
                    .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
                    .unwrap();
                assert_eq!(normalized.action, LightAction::Off);
                assert_eq!(
                    normalized.params.get("transition"),
                    expected_transition.map(|value| json!(value)).as_ref()
                );
                assert!(normalized.params.keys().all(|key| key == "transition"));
            }
        }
    }

    #[test]
    fn legacy_normalize_uses_builtin_profiles_without_descriptor_id_inference() {
        let request = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("profile".into(), json!("relax"))]),
            Map::new(),
        ))
        .unwrap();
        let descriptor = LightDescriptor::new("fixture", "provider-id", "name-only")
            .with_supported_color_modes([LightColorMode::Hs]);
        let normalized = request
            .normalize(&descriptor, &LightSnapshot::new(Some(false)))
            .unwrap();
        assert_eq!(normalized.params.get("brightness"), Some(&json!(144)));
        assert!(normalized.params.contains_key("hs_color"));
    }

    #[test]
    fn default_profiles_use_resolved_entity_then_group_and_fill_transition_only_when_on() {
        let profiles = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\n",
            "light.desk.default,0.1,0.2,100,3\n",
            "group.all_lights.default,0.2,0.3,200,5\n"
        )]);
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Hs])
            .with_feature(LightFeature::Transition);
        let context = LightNormalizationContext::new("light.desk", &profiles);
        let empty = LightServiceRequest::parse(&call("turn_on", Map::new(), Map::new())).unwrap();
        let off_state = empty
            .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
            .unwrap();
        assert_eq!(off_state.params.get("brightness"), Some(&json!(100)));
        assert_eq!(off_state.params.get("transition"), Some(&json!(3.0)));
        assert_eq!(
            off_state.params.get("hs_color"),
            Some(&json!(color::rgb_to_hs(color::xy_to_rgb([0.1, 0.2]))))
        );

        let unknown_state = empty
            .normalize_with_context(&descriptor, &LightSnapshot::new(None), &context)
            .unwrap();
        assert_eq!(unknown_state.params.get("brightness"), Some(&json!(100)));
        assert_eq!(unknown_state.params.get("transition"), Some(&json!(3.0)));

        let caller_color = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("hs_color".into(), json!([120, 100]))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(false)), &context)
        .unwrap();
        assert_eq!(
            caller_color.params.get("hs_color"),
            Some(&json!([120.0, 100.0]))
        );
        assert_eq!(caller_color.params.get("brightness"), Some(&json!(100)));

        let caller_params = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("brightness".into(), json!(40))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(
            &descriptor,
            &LightSnapshot::new(Some(true)).with_brightness(Some(10)),
            &context,
        )
        .unwrap();
        assert_eq!(caller_params.params.get("brightness"), Some(&json!(40)));
        assert_eq!(caller_params.params.get("transition"), Some(&json!(3.0)));
        assert!(!caller_params.params.contains_key("hs_color"));

        let explicit_transition = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([
                ("brightness".into(), json!(40)),
                ("transition".into(), json!(9)),
            ]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(
            &descriptor,
            &LightSnapshot::new(Some(true)).with_brightness(Some(10)),
            &context,
        )
        .unwrap();
        assert_eq!(
            explicit_transition.params.get("transition"),
            Some(&json!(9.0))
        );
        assert!(!explicit_transition.params.contains_key("hs_color"));
    }

    #[test]
    fn unknown_profile_can_leave_empty_request_for_defaults_and_turn_off_keeps_only_default_transition()
     {
        let profiles = LightProfileCatalog::from_csv_sources([concat!(
            "name,x,y,brightness,transition\n",
            "group.all_lights.default,0.2,0.3,99,4\n"
        )]);
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Hs])
            .with_supported_features(LightFeatures::TRANSITION | LightFeatures::FLASH);
        let context = LightNormalizationContext::without_entity_id(&profiles);
        let unknown_profile = LightServiceRequest::parse(&call(
            "turn_on",
            Map::from_iter([("profile".into(), json!("not-installed"))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(true)), &context)
        .unwrap();
        assert_eq!(unknown_profile.params.get("brightness"), Some(&json!(99)));
        assert_eq!(unknown_profile.params.get("transition"), Some(&json!(4.0)));
        assert!(unknown_profile.params.contains_key("hs_color"));

        let turn_off = LightServiceRequest::parse(&call(
            "turn_off",
            Map::from_iter([("flash".into(), json!("short"))]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(true)), &context)
        .unwrap();
        assert_eq!(turn_off.action, LightAction::Off);
        assert_eq!(
            turn_off.params,
            Map::from_iter([
                ("transition".into(), json!(4.0)),
                ("flash".into(), json!("short")),
            ])
        );
    }

    #[test]
    fn toggle_off_retains_common_color_diagnostic_but_step_bypass_does_not_create_one() {
        let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
            .with_supported_color_modes([LightColorMode::Rgb]);
        let profiles = LightProfileCatalog::default();
        let context = LightNormalizationContext::new("light.desk", &profiles);
        let request = LightServiceRequest::parse(&call(
            "toggle",
            Map::from_iter([("color_name".into(), json!("unknown-color"))]),
            Map::new(),
        ))
        .unwrap();
        let toggled_off = request
            .normalize_with_context(&descriptor, &LightSnapshot::new(Some(true)), &context)
            .unwrap();
        assert_eq!(toggled_off.action, LightAction::Off);
        assert_eq!(toggled_off.diagnostics.len(), 1);
        assert_eq!(toggled_off.diagnostics[0].original_value, "unknown-color");

        let stepped = LightServiceRequest::parse(&call(
            "toggle",
            Map::from_iter([
                ("color_name".into(), json!("unknown-color")),
                ("brightness_step".into(), json!(1)),
            ]),
            Map::new(),
        ))
        .unwrap()
        .normalize_with_context(&descriptor, &LightSnapshot::new(Some(true)), &context)
        .unwrap();
        assert!(stepped.diagnostics.is_empty());
    }
}
