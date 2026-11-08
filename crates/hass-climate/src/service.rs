//! Native climate service parsing and provider-effect planning.

use super::{
    CLIMATE_DOMAIN, ClimateDescriptor, ClimateError, ClimateFeatures, ClimateUnitContext, HvacMode,
    attr, round_temperature,
};
use hass_core::{Context, ServiceCall};
use serde_json::{Map, Value};
use std::fmt;
use std::str::FromStr;

/// Selector fields accepted by the shared service-target resolver.
pub const CLIMATE_TARGET_FIELDS: &[&str] = &[
    "entity_id",
    "device_id",
    "area_id",
    "floor_id",
    "label_id",
    "exclude",
];

/// Services registered by the pinned climate source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClimateService {
    SetFanMode,
    SetHumidity,
    SetHvacMode,
    SetPresetMode,
    SetSwingHorizontalMode,
    SetSwingMode,
    SetTemperature,
    Toggle,
    TurnOff,
    TurnOn,
}

impl ClimateService {
    pub const ALL: [Self; 10] = [
        Self::SetFanMode,
        Self::SetHumidity,
        Self::SetHvacMode,
        Self::SetPresetMode,
        Self::SetSwingHorizontalMode,
        Self::SetSwingMode,
        Self::SetTemperature,
        Self::Toggle,
        Self::TurnOff,
        Self::TurnOn,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SetFanMode => "set_fan_mode",
            Self::SetHumidity => "set_humidity",
            Self::SetHvacMode => "set_hvac_mode",
            Self::SetPresetMode => "set_preset_mode",
            Self::SetSwingHorizontalMode => "set_swing_horizontal_mode",
            Self::SetSwingMode => "set_swing_mode",
            Self::SetTemperature => "set_temperature",
            Self::Toggle => "toggle",
            Self::TurnOff => "turn_off",
            Self::TurnOn => "turn_on",
        }
    }

    const fn allowed_arguments(self) -> &'static [&'static str] {
        match self {
            Self::SetFanMode => &["fan_mode"],
            Self::SetHumidity => &["humidity"],
            Self::SetHvacMode => &["hvac_mode"],
            Self::SetPresetMode => &["preset_mode"],
            Self::SetSwingHorizontalMode => &["swing_horizontal_mode"],
            Self::SetSwingMode => &["swing_mode"],
            Self::SetTemperature => &[
                "temperature",
                "target_temp_high",
                "target_temp_low",
                "hvac_mode",
            ],
            Self::Toggle | Self::TurnOff | Self::TurnOn => &[],
        }
    }
}

impl fmt::Display for ClimateService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ClimateService {
    type Err = ClimateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|service| service.as_str() == value)
            .ok_or_else(|| ClimateError::UnknownService(value.to_owned()))
    }
}

/// Typed operation parsed from a Home Assistant climate service call.
#[derive(Clone, Debug, PartialEq)]
pub enum ClimateCommand {
    TurnOn,
    TurnOff,
    Toggle,
    SetHvacMode {
        hvac_mode: HvacMode,
    },
    SetTemperature {
        temperature: Option<f64>,
        target_temp_high: Option<f64>,
        target_temp_low: Option<f64>,
        hvac_mode: Option<HvacMode>,
    },
    SetHumidity {
        humidity: i64,
    },
    SetFanMode {
        fan_mode: String,
    },
    SetPresetMode {
        preset_mode: String,
    },
    SetSwingMode {
        swing_mode: String,
    },
    SetSwingHorizontalMode {
        swing_horizontal_mode: String,
    },
}

impl ClimateCommand {
    /// Parse a standard climate service call, preserving its target and causal
    /// context for the host's registry resolver and later state publication.
    pub fn parse_service_call(call: &ServiceCall) -> Result<ClimateServiceRequest, ClimateError> {
        ClimateServiceRequest::try_from(call)
    }

    #[must_use]
    pub const fn service(&self) -> ClimateService {
        match self {
            Self::TurnOn => ClimateService::TurnOn,
            Self::TurnOff => ClimateService::TurnOff,
            Self::Toggle => ClimateService::Toggle,
            Self::SetHvacMode { .. } => ClimateService::SetHvacMode,
            Self::SetTemperature { .. } => ClimateService::SetTemperature,
            Self::SetHumidity { .. } => ClimateService::SetHumidity,
            Self::SetFanMode { .. } => ClimateService::SetFanMode,
            Self::SetPresetMode { .. } => ClimateService::SetPresetMode,
            Self::SetSwingMode { .. } => ClimateService::SetSwingMode,
            Self::SetSwingHorizontalMode { .. } => ClimateService::SetSwingHorizontalMode,
        }
    }

    /// Resolve descriptor-dependent validation and build a pure ordered plan.
    /// This does not execute provider methods, resolve selectors, or enforce
    /// availability/admission policy.
    pub fn resolve(
        &self,
        descriptor: &ClimateDescriptor,
        units: ClimateUnitContext,
    ) -> Result<ClimateCommandPlan, ClimateError> {
        descriptor.validate()?;
        self.validate_shape()?;
        let required = self.required_features();
        if !descriptor.features().contains(required) {
            return Err(ClimateError::UnsupportedFeature {
                required: required.bits(),
                supported: descriptor.features().bits(),
            });
        }
        let effect = match self {
            Self::TurnOn => ClimateEffect::TurnOn,
            Self::TurnOff => ClimateEffect::TurnOff,
            Self::Toggle => ClimateEffect::Toggle,
            Self::SetHvacMode { hvac_mode } => {
                descriptor.validate_mode("hvac", hvac_mode.as_str())?;
                ClimateEffect::SetHvacMode {
                    hvac_mode: *hvac_mode,
                }
            }
            Self::SetTemperature {
                temperature,
                target_temp_high,
                target_temp_low,
                hvac_mode,
            } => {
                let (min, max, native, precision) = descriptor.temperature_limits(units)?;
                let temperature = temperature
                    .map(|value| {
                        normalize_temperature(
                            value,
                            "temperature",
                            min,
                            max,
                            native,
                            units,
                            precision,
                        )
                    })
                    .transpose()?;
                let target_temp_high = target_temp_high
                    .map(|value| {
                        normalize_temperature(
                            value,
                            "target_temp_high",
                            min,
                            max,
                            native,
                            units,
                            precision,
                        )
                    })
                    .transpose()?;
                let target_temp_low = target_temp_low
                    .map(|value| {
                        normalize_temperature(
                            value,
                            "target_temp_low",
                            min,
                            max,
                            native,
                            units,
                            precision,
                        )
                    })
                    .transpose()?;
                ClimateEffect::SetTemperature {
                    temperature,
                    target_temp_high,
                    target_temp_low,
                    hvac_mode: *hvac_mode,
                }
            }
            Self::SetHumidity { humidity } => {
                if (*humidity as f64) < descriptor.min_humidity
                    || (*humidity as f64) > descriptor.max_humidity
                {
                    return Err(ClimateError::HumidityOutOfRange {
                        value: *humidity,
                        min: descriptor.min_humidity,
                        max: descriptor.max_humidity,
                    });
                }
                ClimateEffect::SetHumidity {
                    humidity: *humidity,
                }
            }
            Self::SetFanMode { fan_mode } => {
                descriptor.validate_mode("fan", fan_mode)?;
                ClimateEffect::SetFanMode {
                    fan_mode: fan_mode.clone(),
                }
            }
            Self::SetPresetMode { preset_mode } => {
                descriptor.validate_mode("preset", preset_mode)?;
                ClimateEffect::SetPresetMode {
                    preset_mode: preset_mode.clone(),
                }
            }
            Self::SetSwingMode { swing_mode } => {
                descriptor.validate_mode("swing", swing_mode)?;
                ClimateEffect::SetSwingMode {
                    swing_mode: swing_mode.clone(),
                }
            }
            Self::SetSwingHorizontalMode {
                swing_horizontal_mode,
            } => {
                descriptor.validate_mode("horizontal_swing", swing_horizontal_mode)?;
                ClimateEffect::SetSwingHorizontalMode {
                    swing_horizontal_mode: swing_horizontal_mode.clone(),
                }
            }
        };
        Ok(ClimateCommandPlan {
            effects: vec![effect],
            target: Map::new(),
            context: Context::default(),
        })
    }

    fn validate_shape(&self) -> Result<(), ClimateError> {
        let Self::SetTemperature {
            temperature,
            target_temp_high,
            target_temp_low,
            ..
        } = self
        else {
            return Ok(());
        };
        if temperature.is_none() && target_temp_high.is_none() && target_temp_low.is_none() {
            return Err(ClimateError::MissingArgument(
                "temperature or target_temp_high/target_temp_low",
            ));
        }
        if temperature.is_some() && (target_temp_high.is_some() || target_temp_low.is_some()) {
            return Err(ClimateError::InvalidArgument {
                field: "temperature".to_owned(),
                expected: "not be combined with target_temp_high or target_temp_low",
            });
        }
        if target_temp_high.is_some() != target_temp_low.is_some() {
            return Err(ClimateError::InvalidArgument {
                field: if target_temp_high.is_some() {
                    "target_temp_low"
                } else {
                    "target_temp_high"
                }
                .to_owned(),
                expected: "be supplied together",
            });
        }
        if let (Some(low), Some(high)) = (target_temp_low, target_temp_high)
            && *low != 0.0
            && low > high
        {
            return Err(ClimateError::InvalidTemperatureRange);
        }
        Ok(())
    }

    /// Feature bits required by the pinned service registration and each
    /// temperature field's explicit feature check.
    #[must_use]
    pub const fn required_features(&self) -> ClimateFeatures {
        match self {
            Self::TurnOn => ClimateFeatures::TURN_ON,
            Self::TurnOff => ClimateFeatures::TURN_OFF,
            Self::Toggle => ClimateFeatures::TURN_ON.union(ClimateFeatures::TURN_OFF),
            Self::SetHvacMode { .. } => ClimateFeatures::NONE,
            Self::SetTemperature {
                temperature: Some(_),
                ..
            } => ClimateFeatures::TARGET_TEMPERATURE,
            Self::SetTemperature {
                target_temp_high: Some(_),
                ..
            }
            | Self::SetTemperature {
                target_temp_low: Some(_),
                ..
            } => ClimateFeatures::TARGET_TEMPERATURE_RANGE,
            Self::SetTemperature { .. } => ClimateFeatures::NONE,
            Self::SetHumidity { .. } => ClimateFeatures::TARGET_HUMIDITY,
            Self::SetFanMode { .. } => ClimateFeatures::FAN_MODE,
            Self::SetPresetMode { .. } => ClimateFeatures::PRESET_MODE,
            Self::SetSwingMode { .. } => ClimateFeatures::SWING_MODE,
            Self::SetSwingHorizontalMode { .. } => ClimateFeatures::SWING_HORIZONTAL_MODE,
        }
    }
}

/// A concrete provider operation in the order returned by the planner.
#[derive(Clone, Debug, PartialEq)]
pub enum ClimateEffect {
    TurnOn,
    TurnOff,
    Toggle,
    SetHvacMode {
        hvac_mode: HvacMode,
    },
    SetTemperature {
        temperature: Option<f64>,
        target_temp_high: Option<f64>,
        target_temp_low: Option<f64>,
        hvac_mode: Option<HvacMode>,
    },
    SetHumidity {
        humidity: i64,
    },
    SetFanMode {
        fan_mode: String,
    },
    SetPresetMode {
        preset_mode: String,
    },
    SetSwingMode {
        swing_mode: String,
    },
    SetSwingHorizontalMode {
        swing_horizontal_mode: String,
    },
}

/// Parsed climate command and unresolved target selectors.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimateServiceRequest {
    pub command: ClimateCommand,
    pub target: Map<String, Value>,
    pub context: Context,
}

impl ClimateServiceRequest {
    /// Resolve into ordered provider effects while carrying target and cause.
    pub fn resolve(
        &self,
        descriptor: &ClimateDescriptor,
        units: ClimateUnitContext,
    ) -> Result<ClimateCommandPlan, ClimateError> {
        let mut plan = self.command.resolve(descriptor, units)?;
        plan.target = self.target.clone();
        plan.context = self.context.clone();
        Ok(plan)
    }
}

/// Ordered provider effects plus unresolved target and causal context.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimateCommandPlan {
    pub effects: Vec<ClimateEffect>,
    pub target: Map<String, Value>,
    pub context: Context,
}

impl TryFrom<&ServiceCall> for ClimateServiceRequest {
    type Error = ClimateError;

    fn try_from(call: &ServiceCall) -> Result<Self, Self::Error> {
        if call.domain != CLIMATE_DOMAIN {
            return Err(ClimateError::WrongDomain(call.domain.clone()));
        }
        let service = ClimateService::from_str(&call.service)?;
        let mut data = call.data.clone();
        if let Some(metadata) = data.remove("metadata")
            && !metadata.is_object()
        {
            return Err(ClimateError::InvalidArgument {
                field: "metadata".to_owned(),
                expected: "an object",
            });
        }
        let mut target = call.target.clone();
        if data.contains_key("entity_id") && target.contains_key("entity_id") {
            return Err(ClimateError::EntityIdInDataAndTarget);
        }
        for field in CLIMATE_TARGET_FIELDS {
            if let Some(value) = data.remove(*field) {
                if target.contains_key(*field) {
                    return Err(ClimateError::DuplicateTargetField((*field).to_owned()));
                }
                target.insert((*field).to_owned(), value);
            }
        }
        if let Some(field) = target
            .keys()
            .find(|field| !CLIMATE_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(ClimateError::UnknownTargetField(field.clone()));
        }
        if let Some(field) = data
            .keys()
            .find(|field| !service.allowed_arguments().contains(&field.as_str()))
        {
            return Err(ClimateError::UnknownServiceField(field.clone()));
        }
        let command = parse_command(service, &data)?;
        if !target
            .keys()
            .any(|field| field != "exclude" && CLIMATE_TARGET_FIELDS.contains(&field.as_str()))
        {
            return Err(ClimateError::InvalidArgument {
                field: "target".to_owned(),
                expected: "at least one positive selector",
            });
        }
        if let Some(exclude) = target.get("exclude") {
            let Some(exclude) = exclude.as_object() else {
                return Err(ClimateError::InvalidArgument {
                    field: "exclude".to_owned(),
                    expected: "a selector object",
                });
            };
            if let Some(field) = exclude.keys().find(|field| {
                field.as_str() == "exclude" || !CLIMATE_TARGET_FIELDS.contains(&field.as_str())
            }) {
                return Err(ClimateError::UnknownTargetField(format!("exclude.{field}")));
            }
        }
        Ok(Self {
            command,
            target,
            context: call.context.clone(),
        })
    }
}

fn parse_command(
    service: ClimateService,
    data: &Map<String, Value>,
) -> Result<ClimateCommand, ClimateError> {
    let required = |field| data.get(field).ok_or(ClimateError::MissingArgument(field));
    match service {
        ClimateService::TurnOn => Ok(ClimateCommand::TurnOn),
        ClimateService::TurnOff => Ok(ClimateCommand::TurnOff),
        ClimateService::Toggle => Ok(ClimateCommand::Toggle),
        ClimateService::SetHvacMode => Ok(ClimateCommand::SetHvacMode {
            hvac_mode: parse_hvac_mode("hvac_mode", required("hvac_mode")?)?,
        }),
        ClimateService::SetTemperature => {
            let temperature = data
                .get(attr::TARGET_TEMPERATURE)
                .map(|value| parse_float(attr::TARGET_TEMPERATURE, value))
                .transpose()?;
            let target_temp_high = data
                .get(attr::TARGET_TEMP_HIGH)
                .map(|value| parse_float(attr::TARGET_TEMP_HIGH, value))
                .transpose()?;
            let target_temp_low = data
                .get(attr::TARGET_TEMP_LOW)
                .map(|value| parse_float(attr::TARGET_TEMP_LOW, value))
                .transpose()?;
            if temperature.is_none() && target_temp_high.is_none() && target_temp_low.is_none() {
                return Err(ClimateError::MissingArgument(
                    "temperature or target_temp_high/target_temp_low",
                ));
            }
            if temperature.is_some() && (target_temp_high.is_some() || target_temp_low.is_some()) {
                return Err(ClimateError::InvalidArgument {
                    field: "temperature".to_owned(),
                    expected: "not be combined with target_temp_high or target_temp_low",
                });
            }
            if target_temp_high.is_some() != target_temp_low.is_some() {
                return Err(ClimateError::InvalidArgument {
                    field: if target_temp_high.is_some() {
                        "target_temp_low"
                    } else {
                        "target_temp_high"
                    }
                    .to_owned(),
                    expected: "be supplied together",
                });
            }
            let hvac_mode = data
                .get("hvac_mode")
                .map(|value| parse_hvac_mode("hvac_mode", value))
                .transpose()?;
            Ok(ClimateCommand::SetTemperature {
                temperature,
                target_temp_high,
                target_temp_low,
                hvac_mode,
            })
        }
        ClimateService::SetHumidity => Ok(ClimateCommand::SetHumidity {
            humidity: parse_integer("humidity", required("humidity")?)?,
        }),
        ClimateService::SetFanMode => Ok(ClimateCommand::SetFanMode {
            fan_mode: parse_string("fan_mode", required("fan_mode")?)?,
        }),
        ClimateService::SetPresetMode => Ok(ClimateCommand::SetPresetMode {
            preset_mode: parse_string("preset_mode", required("preset_mode")?)?,
        }),
        ClimateService::SetSwingMode => Ok(ClimateCommand::SetSwingMode {
            swing_mode: parse_string("swing_mode", required("swing_mode")?)?,
        }),
        ClimateService::SetSwingHorizontalMode => Ok(ClimateCommand::SetSwingHorizontalMode {
            swing_horizontal_mode: parse_string(
                "swing_horizontal_mode",
                required("swing_horizontal_mode")?,
            )?,
        }),
    }
}

fn parse_hvac_mode(field: &str, value: &Value) -> Result<HvacMode, ClimateError> {
    let value = parse_string(field, value)?;
    HvacMode::from_str(&value).map_err(|_| ClimateError::InvalidArgument {
        field: field.to_owned(),
        expected: "a Home Assistant HVAC mode",
    })
}

fn parse_string(field: &str, value: &Value) -> Result<String, ClimateError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(if *value { "True" } else { "False" }.to_owned()),
        Value::Number(number) if number.is_f64() => {
            let formatted = format!("{:?}", number.as_f64().expect("JSON float"));
            if let Some((mantissa, exponent)) = formatted.split_once('e') {
                let (sign, digits) = if let Some(digits) = exponent.strip_prefix('-') {
                    ('-', digits)
                } else {
                    ('+', exponent.strip_prefix('+').unwrap_or(exponent))
                };
                Ok(format!("{mantissa}e{sign}{digits:0>2}"))
            } else {
                Ok(formatted)
            }
        }
        Value::Number(number) => Ok(number.to_string()),
        _ => Err(ClimateError::InvalidArgument {
            field: field.to_owned(),
            expected: "a scalar string value",
        }),
    }
}

fn parse_float(field: &str, value: &Value) -> Result<f64, ClimateError> {
    let value = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        _ => None,
    }
    .filter(|value| value.is_finite())
    .ok_or_else(|| ClimateError::InvalidArgument {
        field: field.to_owned(),
        expected: "a finite number",
    })?;
    Ok(value)
}

fn parse_integer(field: &str, value: &Value) -> Result<i64, ClimateError> {
    match value {
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(value)
            } else if let Some(value) = number.as_u64().and_then(|value| i64::try_from(value).ok())
            {
                Ok(value)
            } else if let Some(value) = number.as_f64().filter(|value| value.is_finite()) {
                let value = value.trunc();
                if value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                    Ok(value as i64)
                } else {
                    Err(ClimateError::InvalidArgument {
                        field: field.to_owned(),
                        expected: "an integer",
                    })
                }
            } else {
                Err(ClimateError::InvalidArgument {
                    field: field.to_owned(),
                    expected: "an integer",
                })
            }
        }
        Value::String(value) => {
            value
                .trim()
                .parse::<i64>()
                .map_err(|_| ClimateError::InvalidArgument {
                    field: field.to_owned(),
                    expected: "an integer",
                })
        }
        _ => Err(ClimateError::InvalidArgument {
            field: field.to_owned(),
            expected: "an integer",
        }),
    }
}

fn normalize_temperature(
    value: f64,
    field: &str,
    min: f64,
    max: f64,
    native: super::ClimateTemperatureUnit,
    units: ClimateUnitContext,
    precision: f64,
) -> Result<f64, ClimateError> {
    if !value.is_finite() {
        return Err(ClimateError::InvalidArgument {
            field: field.to_owned(),
            expected: "a finite number",
        });
    }
    let native_value = hass_units::convert(
        hass_units::UnitFamily::Temperature,
        value,
        Some(units.temperature_unit.as_str()),
        Some(native.as_str()),
    )
    .map_err(|error| ClimateError::UnitConversion(error.to_string()))?;
    let shown_min = hass_units::convert(
        hass_units::UnitFamily::Temperature,
        min,
        Some(native.as_str()),
        Some(units.temperature_unit.as_str()),
    )
    .map_err(|error| ClimateError::UnitConversion(error.to_string()))?;
    let shown_max = hass_units::convert(
        hass_units::UnitFamily::Temperature,
        max,
        Some(native.as_str()),
        Some(units.temperature_unit.as_str()),
    )
    .map_err(|error| ClimateError::UnitConversion(error.to_string()))?;
    let shown_min = round_temperature(shown_min, precision);
    let shown_max = round_temperature(shown_max, precision);
    if (native_value < min && value < shown_min) || (native_value > max && value > shown_max) {
        return Err(ClimateError::TemperatureOutOfRange {
            field: field.to_owned(),
            value,
            min: shown_min,
            max: shown_max,
        });
    }
    Ok(native_value.clamp(min, max))
}
