//! Native fan service validation and provider-effect planning.

use super::{
    FAN_DOMAIN, FanDescriptor, FanError, FanFeatures, FanSnapshot, adjust_speed_percentage,
};
use hass_core::{Context, ServiceCall};
use serde_json::{Map, Value};
use std::fmt;
use std::str::FromStr;

/// Target selector fields accepted by the shared service-target resolver.
pub const FAN_TARGET_FIELDS: &[&str] =
    &["entity_id", "device_id", "area_id", "floor_id", "label_id"];

/// Fan services registered by the pinned source revision.
///
/// The older `set_speed` service is absent and is intentionally not accepted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FanService {
    IncreaseSpeed,
    DecreaseSpeed,
    Oscillate,
    SetDirection,
    SetPercentage,
    SetPresetMode,
    Toggle,
    TurnOff,
    TurnOn,
}

impl FanService {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IncreaseSpeed => "increase_speed",
            Self::DecreaseSpeed => "decrease_speed",
            Self::Oscillate => "oscillate",
            Self::SetDirection => "set_direction",
            Self::SetPercentage => "set_percentage",
            Self::SetPresetMode => "set_preset_mode",
            Self::Toggle => "toggle",
            Self::TurnOff => "turn_off",
            Self::TurnOn => "turn_on",
        }
    }

    const fn allowed_arguments(self) -> &'static [&'static str] {
        match self {
            Self::IncreaseSpeed | Self::DecreaseSpeed => &["percentage_step"],
            Self::Oscillate => &["oscillating"],
            Self::SetDirection => &["direction"],
            Self::SetPercentage => &["percentage"],
            Self::SetPresetMode => &["preset_mode"],
            Self::TurnOn => &["percentage", "preset_mode"],
            Self::TurnOff | Self::Toggle => &[],
        }
    }
}

impl fmt::Display for FanService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FanService {
    type Err = FanError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "increase_speed" => Ok(Self::IncreaseSpeed),
            "decrease_speed" => Ok(Self::DecreaseSpeed),
            "oscillate" => Ok(Self::Oscillate),
            "set_direction" => Ok(Self::SetDirection),
            "set_percentage" => Ok(Self::SetPercentage),
            "set_preset_mode" => Ok(Self::SetPresetMode),
            "toggle" => Ok(Self::Toggle),
            "turn_off" => Ok(Self::TurnOff),
            "turn_on" => Ok(Self::TurnOn),
            _ => Err(FanError::UnknownService(value.to_owned())),
        }
    }
}

/// One validated fan service, typed before it reaches provider command code.
#[derive(Clone, Debug, PartialEq)]
pub enum FanCommand {
    TurnOn {
        percentage: Option<i64>,
        preset_mode: Option<String>,
    },
    TurnOff,
    Toggle,
    IncreaseSpeed {
        percentage_step: Option<i64>,
    },
    DecreaseSpeed {
        percentage_step: Option<i64>,
    },
    Oscillate {
        oscillating: bool,
    },
    SetDirection {
        direction: Option<String>,
    },
    SetPercentage {
        percentage: i64,
    },
    SetPresetMode {
        preset_mode: String,
    },
}

impl FanCommand {
    /// Parse the Home Assistant service name and validate its data and target.
    pub fn parse_service_call(call: &ServiceCall) -> Result<FanServiceRequest, FanError> {
        FanServiceRequest::try_from(call)
    }

    #[must_use]
    pub const fn service(&self) -> FanService {
        match self {
            Self::TurnOn { .. } => FanService::TurnOn,
            Self::TurnOff => FanService::TurnOff,
            Self::Toggle => FanService::Toggle,
            Self::IncreaseSpeed { .. } => FanService::IncreaseSpeed,
            Self::DecreaseSpeed { .. } => FanService::DecreaseSpeed,
            Self::Oscillate { .. } => FanService::Oscillate,
            Self::SetDirection { .. } => FanService::SetDirection,
            Self::SetPercentage { .. } => FanService::SetPercentage,
            Self::SetPresetMode { .. } => FanService::SetPresetMode,
        }
    }

    /// Feature bits required by the source service registration.
    #[must_use]
    pub const fn required_features(&self) -> FanFeatures {
        match self {
            Self::TurnOn { .. } => FanFeatures::TURN_ON,
            Self::TurnOff => FanFeatures::TURN_OFF,
            Self::Toggle => FanFeatures::TURN_ON.union(FanFeatures::TURN_OFF),
            Self::IncreaseSpeed { .. }
            | Self::DecreaseSpeed { .. }
            | Self::SetPercentage { .. } => FanFeatures::SET_SPEED,
            Self::Oscillate { .. } => FanFeatures::OSCILLATE,
            Self::SetDirection { .. } => FanFeatures::DIRECTION,
            Self::SetPresetMode { .. } => FanFeatures::SET_SPEED.union(FanFeatures::PRESET_MODE),
        }
    }

    /// Resolve a validated service into ordered provider method effects.
    ///
    /// This is a pure plan. The host must perform the returned effects through
    /// its real provider implementation and apply target/availability policy.
    pub fn resolve(
        &self,
        descriptor: &FanDescriptor,
        current: &FanSnapshot,
    ) -> Result<FanCommandPlan, FanError> {
        validate_command_percentages(self)?;
        let required = self.required_features();
        let supported = descriptor.features();
        if !supported.contains(required) {
            return Err(FanError::UnsupportedFeature {
                required: required.bits(),
                supported: supported.bits(),
            });
        }

        let effects = match self {
            Self::TurnOn {
                percentage,
                preset_mode,
            } => {
                if let Some(preset_mode) = preset_mode {
                    descriptor.validate_preset_mode(preset_mode)?;
                }
                vec![FanEffect::TurnOn {
                    percentage: *percentage,
                    preset_mode: preset_mode.clone(),
                }]
            }
            Self::TurnOff => vec![FanEffect::TurnOff],
            Self::Toggle if current.state() == super::FanState::On => {
                vec![FanEffect::TurnOff]
            }
            Self::Toggle => vec![FanEffect::TurnOn {
                percentage: None,
                preset_mode: None,
            }],
            Self::IncreaseSpeed { percentage_step } => {
                let percentage = adjust_speed_percentage(
                    current.percentage(),
                    descriptor.speed_count(),
                    *percentage_step,
                    1,
                )?;
                effects_for_percentage(percentage)
            }
            Self::DecreaseSpeed { percentage_step } => {
                let percentage = adjust_speed_percentage(
                    current.percentage(),
                    descriptor.speed_count(),
                    *percentage_step,
                    -1,
                )?;
                effects_for_percentage(percentage)
            }
            Self::Oscillate { oscillating } => vec![FanEffect::Oscillate {
                oscillating: *oscillating,
            }],
            Self::SetDirection { direction } => vec![FanEffect::SetDirection {
                direction: direction.clone(),
            }],
            Self::SetPercentage { percentage } => effects_for_percentage(*percentage),
            Self::SetPresetMode { preset_mode } => {
                descriptor.validate_preset_mode(preset_mode)?;
                vec![FanEffect::SetPresetMode {
                    preset_mode: preset_mode.clone(),
                }]
            }
        };
        Ok(FanCommandPlan { effects })
    }
}

fn validate_command_percentages(command: &FanCommand) -> Result<(), FanError> {
    match command {
        FanCommand::TurnOn { percentage, .. } => {
            if let Some(percentage) = percentage {
                validate_percentage_value("percentage", *percentage)?;
            }
        }
        FanCommand::IncreaseSpeed { percentage_step }
        | FanCommand::DecreaseSpeed { percentage_step } => {
            if let Some(percentage_step) = percentage_step {
                validate_percentage_value("percentage_step", *percentage_step)?;
            }
        }
        FanCommand::SetPercentage { percentage } => {
            validate_percentage_value("percentage", *percentage)?;
        }
        FanCommand::TurnOff
        | FanCommand::Toggle
        | FanCommand::Oscillate { .. }
        | FanCommand::SetDirection { .. }
        | FanCommand::SetPresetMode { .. } => {}
    }
    Ok(())
}

fn validate_percentage_value(field: &str, value: i64) -> Result<(), FanError> {
    if (0..=100).contains(&value) {
        Ok(())
    } else {
        Err(FanError::PercentageOutOfRange {
            field: field.to_owned(),
            value,
        })
    }
}

/// A parsed service request. Target selectors and causal context remain
/// available to the host that resolves entity IDs and invokes a provider.
#[derive(Clone, Debug, PartialEq)]
pub struct FanServiceRequest {
    pub command: FanCommand,
    pub target: Map<String, Value>,
    pub context: Context,
}

impl TryFrom<&ServiceCall> for FanServiceRequest {
    type Error = FanError;

    fn try_from(call: &ServiceCall) -> Result<Self, Self::Error> {
        if call.domain != FAN_DOMAIN {
            return Err(FanError::WrongDomain(call.domain.clone()));
        }
        let service: FanService = call.service.parse()?;
        if call.data.contains_key("entity_id") && call.target.contains_key("entity_id") {
            return Err(FanError::EntityIdInDataAndTarget);
        }
        if let Some(field) = call
            .target
            .keys()
            .find(|field| !FAN_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(FanError::UnknownTargetField(field.clone()));
        }
        if let Some(field) = call.data.keys().find(|field| {
            !FAN_TARGET_FIELDS.contains(&field.as_str())
                && !service.allowed_arguments().contains(&field.as_str())
        }) {
            return Err(FanError::UnknownServiceField(field.clone()));
        }

        let command = parse_command(service, &call.data)?;
        let mut target = call.target.clone();
        for field in FAN_TARGET_FIELDS {
            if let Some(value) = call.data.get(*field) {
                if target.contains_key(*field) {
                    return Err(FanError::InvalidArgument {
                        field: (*field).to_owned(),
                        expected: "present in either service data or target, not both",
                    });
                }
                target.insert((*field).to_owned(), value.clone());
            }
        }

        Ok(Self {
            command,
            target,
            context: call.context.clone(),
        })
    }
}

/// One actual provider method call requested by the fan service behavior.
/// No variant performs I/O by itself.
#[derive(Clone, Debug, PartialEq)]
pub enum FanEffect {
    TurnOn {
        percentage: Option<i64>,
        preset_mode: Option<String>,
    },
    TurnOff,
    SetPercentage {
        percentage: i64,
    },
    SetPresetMode {
        preset_mode: String,
    },
    SetDirection {
        direction: Option<String>,
    },
    Oscillate {
        oscillating: bool,
    },
}

/// Ordered provider calls produced by one Home Assistant fan service.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FanCommandPlan {
    pub effects: Vec<FanEffect>,
}

fn effects_for_percentage(percentage: i64) -> Vec<FanEffect> {
    if percentage == 0 {
        // The pinned async_set_percentage invokes async_turn_off but does not
        // return before it calls the provider's set_percentage(0) method.
        vec![FanEffect::TurnOff, FanEffect::SetPercentage { percentage }]
    } else {
        vec![FanEffect::SetPercentage { percentage }]
    }
}

fn parse_command(service: FanService, data: &Map<String, Value>) -> Result<FanCommand, FanError> {
    match service {
        FanService::TurnOn => Ok(FanCommand::TurnOn {
            percentage: optional_percentage(data, "percentage")?,
            preset_mode: optional_string(data, "preset_mode")?,
        }),
        FanService::TurnOff => Ok(FanCommand::TurnOff),
        FanService::Toggle => Ok(FanCommand::Toggle),
        FanService::IncreaseSpeed => Ok(FanCommand::IncreaseSpeed {
            percentage_step: optional_percentage(data, "percentage_step")?,
        }),
        FanService::DecreaseSpeed => Ok(FanCommand::DecreaseSpeed {
            percentage_step: optional_percentage(data, "percentage_step")?,
        }),
        FanService::Oscillate => Ok(FanCommand::Oscillate {
            oscillating: required_boolean(data, "oscillating")?,
        }),
        FanService::SetDirection => Ok(FanCommand::SetDirection {
            direction: optional_string(data, "direction")?,
        }),
        FanService::SetPercentage => Ok(FanCommand::SetPercentage {
            percentage: required_percentage(data, "percentage")?,
        }),
        FanService::SetPresetMode => Ok(FanCommand::SetPresetMode {
            preset_mode: required_string(data, "preset_mode")?,
        }),
    }
}

fn required_percentage(data: &Map<String, Value>, field: &'static str) -> Result<i64, FanError> {
    let value = data.get(field).ok_or(FanError::MissingArgument(field))?;
    parse_percentage(field, value)
}

fn optional_percentage(
    data: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<i64>, FanError> {
    data.get(field)
        .map(|value| parse_percentage(field, value))
        .transpose()
}

fn parse_percentage(field: &str, value: &Value) -> Result<i64, FanError> {
    let percentage = coerce_integer(value).ok_or_else(|| FanError::InvalidArgument {
        field: field.to_owned(),
        expected: "an integer percentage",
    })?;
    if !(0..=100).contains(&percentage) {
        return Err(FanError::PercentageOutOfRange {
            field: field.to_owned(),
            value: percentage,
        });
    }
    Ok(percentage)
}

fn required_string(data: &Map<String, Value>, field: &'static str) -> Result<String, FanError> {
    data.get(field)
        .ok_or(FanError::MissingArgument(field))?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| FanError::InvalidArgument {
            field: field.to_owned(),
            expected: "a string",
        })
}

fn optional_string(
    data: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, FanError> {
    data.get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| FanError::InvalidArgument {
                    field: field.to_owned(),
                    expected: "a string",
                })
        })
        .transpose()
}

fn required_boolean(data: &Map<String, Value>, field: &'static str) -> Result<bool, FanError> {
    let value = data.get(field).ok_or(FanError::MissingArgument(field))?;
    coerce_boolean(value).ok_or_else(|| FanError::InvalidArgument {
        field: field.to_owned(),
        expected: "a boolean",
    })
}

/// Match the service schema's `Coerce(int)` before its 0..100 percentage check.
/// JSON booleans map to 0/1, finite JSON floats truncate toward zero, and
/// decimal strings accept surrounding whitespace and valid digit separators.
fn coerce_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(value) => value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| {
                let number = value.as_f64()?;
                let truncated = number.trunc();
                (number.is_finite() && truncated >= i64::MIN as f64 && truncated <= i64::MAX as f64)
                    .then_some(truncated as i64)
            }),
        Value::String(value) => parse_integer_string(value),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn parse_integer_string(value: &str) -> Option<i64> {
    let value = value.trim();
    let digits = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    if digits.is_empty() {
        return None;
    }
    let bytes = digits.as_bytes();
    if bytes.iter().enumerate().any(|(index, byte)| {
        if *byte == b'_' {
            index == 0
                || index + 1 == bytes.len()
                || !bytes[index - 1].is_ascii_digit()
                || !bytes[index + 1].is_ascii_digit()
        } else {
            !byte.is_ascii_digit()
        }
    }) {
        return None;
    }
    value.replace('_', "").parse().ok()
}

/// Match Home Assistant's config-validation boolean spellings after service
/// schema coercion. Numeric values are accepted only at 0 or 1.
fn coerce_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => match value.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" | "enabled" => Some(true),
            "0" | "false" | "no" | "off" | "disable" | "disabled" => Some(false),
            _ => None,
        },
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}
