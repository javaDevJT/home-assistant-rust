//! Typed descriptions and state projection for native `sensor` entities.
//!
//! This is a source-pinned subset of Home Assistant Core's sensor platform.
//! Descriptions are stable; readings are supplied independently and projected
//! to [`EntitySnapshot`] values for the domain-agnostic [`EntityLifecycle`].

mod unit_tables;

use std::error::Error;
use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use hass_core::Context;
use hass_entity::{EntitySnapshot, EntitySpec};
use hass_units::{ConversionError, UnitFamily};
use serde_json::{Map, Value, json};

/// Home Assistant sensor device classes at the pinned Core revision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SensorDeviceClass {
    Date,
    Enum,
    Timestamp,
    Uptime,
    AbsoluteHumidity,
    ApparentPower,
    Aqi,
    Area,
    AtmosphericPressure,
    Battery,
    BloodGlucoseConcentration,
    CarbonMonoxide,
    CarbonDioxide,
    Conductivity,
    Current,
    DataRate,
    DataSize,
    Distance,
    Duration,
    Energy,
    EnergyDistance,
    EnergyStorage,
    Frequency,
    Gas,
    Humidity,
    Illuminance,
    Irradiance,
    Moisture,
    Monetary,
    NitrogenDioxide,
    NitrogenMonoxide,
    NitrousOxide,
    Ozone,
    Ph,
    Pm1,
    Pm10,
    Pm25,
    Pm4,
    PowerFactor,
    Power,
    Precipitation,
    PrecipitationIntensity,
    Pressure,
    Radon,
    ReactiveEnergy,
    ReactivePower,
    SignalStrength,
    SoundPressure,
    Speed,
    SulphurDioxide,
    Temperature,
    TemperatureDelta,
    VolatileOrganicCompounds,
    VolatileOrganicCompoundsParts,
    Voltage,
    Volume,
    VolumeStorage,
    VolumeFlowRate,
    Water,
    Weight,
    WindDirection,
    WindSpeed,
}

impl SensorDeviceClass {
    /// Return the pinned Home Assistant class value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Enum => "enum",
            Self::Timestamp => "timestamp",
            Self::Uptime => "uptime",
            Self::AbsoluteHumidity => "absolute_humidity",
            Self::ApparentPower => "apparent_power",
            Self::Aqi => "aqi",
            Self::Area => "area",
            Self::AtmosphericPressure => "atmospheric_pressure",
            Self::Battery => "battery",
            Self::BloodGlucoseConcentration => "blood_glucose_concentration",
            Self::CarbonMonoxide => "carbon_monoxide",
            Self::CarbonDioxide => "carbon_dioxide",
            Self::Conductivity => "conductivity",
            Self::Current => "current",
            Self::DataRate => "data_rate",
            Self::DataSize => "data_size",
            Self::Distance => "distance",
            Self::Duration => "duration",
            Self::Energy => "energy",
            Self::EnergyDistance => "energy_distance",
            Self::EnergyStorage => "energy_storage",
            Self::Frequency => "frequency",
            Self::Gas => "gas",
            Self::Humidity => "humidity",
            Self::Illuminance => "illuminance",
            Self::Irradiance => "irradiance",
            Self::Moisture => "moisture",
            Self::Monetary => "monetary",
            Self::NitrogenDioxide => "nitrogen_dioxide",
            Self::NitrogenMonoxide => "nitrogen_monoxide",
            Self::NitrousOxide => "nitrous_oxide",
            Self::Ozone => "ozone",
            Self::Ph => "ph",
            Self::Pm1 => "pm1",
            Self::Pm10 => "pm10",
            Self::Pm25 => "pm25",
            Self::Pm4 => "pm4",
            Self::PowerFactor => "power_factor",
            Self::Power => "power",
            Self::Precipitation => "precipitation",
            Self::PrecipitationIntensity => "precipitation_intensity",
            Self::Pressure => "pressure",
            Self::Radon => "radon",
            Self::ReactiveEnergy => "reactive_energy",
            Self::ReactivePower => "reactive_power",
            Self::SignalStrength => "signal_strength",
            Self::SoundPressure => "sound_pressure",
            Self::Speed => "speed",
            Self::SulphurDioxide => "sulphur_dioxide",
            Self::Temperature => "temperature",
            Self::TemperatureDelta => "temperature_delta",
            Self::VolatileOrganicCompounds => "volatile_organic_compounds",
            Self::VolatileOrganicCompoundsParts => "volatile_organic_compounds_parts",
            Self::Voltage => "voltage",
            Self::Volume => "volume",
            Self::VolumeStorage => "volume_storage",
            Self::VolumeFlowRate => "volume_flow_rate",
            Self::Water => "water",
            Self::Weight => "weight",
            Self::WindDirection => "wind_direction",
            Self::WindSpeed => "wind_speed",
        }
    }

    /// Parse a class value from Home Assistant metadata at the pinned revision.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        const ALL: &[SensorDeviceClass] = &[
            SensorDeviceClass::Date,
            SensorDeviceClass::Enum,
            SensorDeviceClass::Timestamp,
            SensorDeviceClass::Uptime,
            SensorDeviceClass::AbsoluteHumidity,
            SensorDeviceClass::ApparentPower,
            SensorDeviceClass::Aqi,
            SensorDeviceClass::Area,
            SensorDeviceClass::AtmosphericPressure,
            SensorDeviceClass::Battery,
            SensorDeviceClass::BloodGlucoseConcentration,
            SensorDeviceClass::CarbonMonoxide,
            SensorDeviceClass::CarbonDioxide,
            SensorDeviceClass::Conductivity,
            SensorDeviceClass::Current,
            SensorDeviceClass::DataRate,
            SensorDeviceClass::DataSize,
            SensorDeviceClass::Distance,
            SensorDeviceClass::Duration,
            SensorDeviceClass::Energy,
            SensorDeviceClass::EnergyDistance,
            SensorDeviceClass::EnergyStorage,
            SensorDeviceClass::Frequency,
            SensorDeviceClass::Gas,
            SensorDeviceClass::Humidity,
            SensorDeviceClass::Illuminance,
            SensorDeviceClass::Irradiance,
            SensorDeviceClass::Moisture,
            SensorDeviceClass::Monetary,
            SensorDeviceClass::NitrogenDioxide,
            SensorDeviceClass::NitrogenMonoxide,
            SensorDeviceClass::NitrousOxide,
            SensorDeviceClass::Ozone,
            SensorDeviceClass::Ph,
            SensorDeviceClass::Pm1,
            SensorDeviceClass::Pm10,
            SensorDeviceClass::Pm25,
            SensorDeviceClass::Pm4,
            SensorDeviceClass::PowerFactor,
            SensorDeviceClass::Power,
            SensorDeviceClass::Precipitation,
            SensorDeviceClass::PrecipitationIntensity,
            SensorDeviceClass::Pressure,
            SensorDeviceClass::Radon,
            SensorDeviceClass::ReactiveEnergy,
            SensorDeviceClass::ReactivePower,
            SensorDeviceClass::SignalStrength,
            SensorDeviceClass::SoundPressure,
            SensorDeviceClass::Speed,
            SensorDeviceClass::SulphurDioxide,
            SensorDeviceClass::Temperature,
            SensorDeviceClass::TemperatureDelta,
            SensorDeviceClass::VolatileOrganicCompounds,
            SensorDeviceClass::VolatileOrganicCompoundsParts,
            SensorDeviceClass::Voltage,
            SensorDeviceClass::Volume,
            SensorDeviceClass::VolumeStorage,
            SensorDeviceClass::VolumeFlowRate,
            SensorDeviceClass::Water,
            SensorDeviceClass::Weight,
            SensorDeviceClass::WindDirection,
            SensorDeviceClass::WindSpeed,
        ];
        ALL.iter().copied().find(|class| class.as_str() == value)
    }

    const fn is_non_numeric(self) -> bool {
        matches!(
            self,
            Self::Date | Self::Enum | Self::Timestamp | Self::Uptime
        )
    }
}

impl fmt::Display for SensorDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Home Assistant sensor state classes at the pinned Core revision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SensorStateClass {
    Measurement,
    MeasurementAngle,
    Total,
    TotalIncreasing,
}

impl SensorStateClass {
    /// Return the pinned Home Assistant state-class value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measurement => "measurement",
            Self::MeasurementAngle => "measurement_angle",
            Self::Total => "total",
            Self::TotalIncreasing => "total_increasing",
        }
    }

    /// Parse a state-class value from Home Assistant metadata.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "measurement" => Some(Self::Measurement),
            "measurement_angle" => Some(Self::MeasurementAngle),
            "total" => Some(Self::Total),
            "total_increasing" => Some(Self::TotalIncreasing),
            _ => None,
        }
    }
}

impl fmt::Display for SensorStateClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A unit string retained exactly as supplied by a provider or unit selector.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SensorUnit(String);

impl SensorUnit {
    /// Create a unit value without normalizing its spelling.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the unit spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SensorUnit {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SensorUnit {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for SensorUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Typed provider reading accepted by the sensor projector.
#[derive(Clone, Debug, PartialEq)]
pub enum SensorValue {
    /// Plain provider text. Numeric metadata causes numeric parsing.
    Text(String),
    /// A signed integer that can be serialized without floating-point loss.
    Integer(i64),
    /// An unsigned counter without integer state-string precision loss.
    Unsigned(u64),
    /// Python treats booleans as integers on numeric paths.
    Boolean(bool),
    /// A floating-point measurement.
    Number(f64),
    /// Decimal text. Numeric validation is applied while the spelling is kept
    /// when no unit conversion is needed.
    Decimal(String),
    /// Calendar date without a time or timezone.
    Date(NaiveDate),
    /// Timezone-aware timestamp.
    DateTime(DateTime<chrono::FixedOffset>),
    /// Naive datetime. It can be text, but is invalid for `timestamp` and
    /// `uptime` classes because those classes require an offset.
    NaiveDateTime(NaiveDateTime),
}

impl From<String> for SensorValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for SensorValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<i64> for SensorValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for SensorValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<u64> for SensorValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<bool> for SensorValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<NaiveDate> for SensorValue {
    fn from(value: NaiveDate) -> Self {
        Self::Date(value)
    }
}

impl From<NaiveDateTime> for SensorValue {
    fn from(value: NaiveDateTime) -> Self {
        Self::NaiveDateTime(value)
    }
}

impl From<DateTime<chrono::FixedOffset>> for SensorValue {
    fn from(value: DateTime<chrono::FixedOffset>) -> Self {
        Self::DateTime(value)
    }
}

impl From<DateTime<Utc>> for SensorValue {
    fn from(value: DateTime<Utc>) -> Self {
        Self::DateTime(value.fixed_offset())
    }
}

/// One dynamic reading, independent of its stable descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorReading {
    /// `None` means unknown while available; availability is independent.
    pub value: Option<SensorValue>,
    /// Whether the provider currently considers the sensor available.
    pub available: bool,
    /// Dynamic reset instant. Core permits it only for `total` state class.
    pub last_reset: Option<DateTime<chrono::FixedOffset>>,
    /// Additional provider-owned state attributes.
    pub attributes: Map<String, Value>,
    /// Causal context carried into the runtime state change.
    pub context: Context,
}

impl SensorReading {
    /// Construct an available reading with a value.
    #[must_use]
    pub fn new(value: impl Into<SensorValue>) -> Self {
        Self {
            value: Some(value.into()),
            available: true,
            last_reset: None,
            attributes: Map::new(),
            context: Context::default(),
        }
    }

    /// Construct an available reading with no value (`unknown`).
    #[must_use]
    pub fn without_value() -> Self {
        Self {
            value: None,
            ..Self::new(SensorValue::Text(String::new()))
        }
    }

    /// Construct an unavailable reading.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            value: None,
            available: false,
            ..Self::without_value()
        }
    }

    /// Set provider availability independently from the reading value.
    #[must_use]
    pub fn with_availability(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Attach the `last_reset` value for a `total` sensor.
    #[must_use]
    pub fn with_last_reset(mut self, last_reset: DateTime<chrono::FixedOffset>) -> Self {
        self.last_reset = Some(last_reset);
        self
    }

    /// Add a provider-owned state attribute.
    #[must_use]
    pub fn with_attribute(mut self, name: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(name.into(), value);
        self
    }

    /// Attach the event context used when the projected snapshot is published.
    #[must_use]
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }
}

/// Stable metadata for one native sensor entity.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorDescriptor {
    entity_spec: EntitySpec,
    device_class: Option<SensorDeviceClass>,
    state_class: Option<SensorStateClass>,
    raw_native_unit: Option<SensorUnit>,
    native_unit: Option<SensorUnit>,
    suggested_unit: Option<SensorUnit>,
    options: Option<Vec<String>>,
    suggested_display_precision: Option<usize>,
}

impl SensorDescriptor {
    /// Create a sensor description from its stable lifecycle identity.
    ///
    /// The supplied entity spec must use the `sensor` domain.
    pub fn new(entity_spec: EntitySpec) -> Result<Self, SensorProjectionError> {
        if entity_spec.domain != "sensor" {
            return Err(SensorProjectionError::new(
                SensorErrorKind::WrongDomain {
                    actual: entity_spec.domain.clone(),
                },
                Vec::new(),
            ));
        }
        let capabilities = entity_spec
            .info
            .capabilities
            .as_ref()
            .and_then(Value::as_object);
        let device_class = entity_spec
            .info
            .original_device_class
            .as_deref()
            .and_then(SensorDeviceClass::parse);
        let state_class = capabilities
            .and_then(|values| values.get("state_class"))
            .and_then(Value::as_str)
            .and_then(SensorStateClass::parse);
        let options = capabilities
            .and_then(|values| values.get("options"))
            .and_then(Value::as_array)
            .and_then(|values| values.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            .map(|values| values.into_iter().map(str::to_owned).collect());
        let suggested_display_precision = capabilities
            .and_then(|values| values.get("suggested_display_precision"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        let raw_native_unit = entity_spec
            .info
            .unit_of_measurement
            .as_deref()
            .map(SensorUnit::from);
        let native_unit = raw_native_unit
            .as_ref()
            .map(|unit| SensorUnit::from(compatible_native_unit(unit.as_str())));
        Ok(Self {
            entity_spec,
            device_class,
            state_class,
            raw_native_unit,
            native_unit,
            suggested_unit: None,
            options,
            suggested_display_precision,
        })
    }

    /// Set a friendly name carried through the lifecycle's provider metadata.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.entity_spec.info.original_name = Some(name.into());
        self
    }

    /// Set the sensor device class.
    #[must_use]
    pub fn with_device_class(mut self, device_class: SensorDeviceClass) -> Self {
        self.entity_spec.info.original_device_class = Some(device_class.as_str().to_owned());
        self.device_class = Some(device_class);
        self
    }

    /// Set the sensor state class.
    #[must_use]
    pub fn with_state_class(mut self, state_class: SensorStateClass) -> Self {
        self.state_class = Some(state_class);
        self
    }

    /// Set the provider's native unit.
    #[must_use]
    pub fn with_native_unit(mut self, unit: impl Into<SensorUnit>) -> Self {
        let unit = unit.into();
        self.entity_spec.info.unit_of_measurement = Some(unit.as_str().to_owned());
        self.native_unit = Some(SensorUnit::from(compatible_native_unit(unit.as_str())));
        self.raw_native_unit = Some(unit);
        self
    }

    /// Suggest a display unit. A supported class converter is required when it
    /// differs from the native unit.
    #[must_use]
    pub fn with_suggested_unit(mut self, unit: impl Into<SensorUnit>) -> Self {
        self.suggested_unit = Some(unit.into());
        self
    }

    /// Set the enum values accepted for an `enum` sensor.
    #[must_use]
    pub fn with_options(mut self, options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options = Some(options.into_iter().map(Into::into).collect());
        self
    }

    /// Set the precision suggested to consumers of the descriptor.
    #[must_use]
    pub fn with_suggested_display_precision(mut self, precision: usize) -> Self {
        self.suggested_display_precision = Some(precision);
        self
    }

    /// Return the provider identity to register with [`EntityLifecycle`].
    #[must_use]
    pub fn entity_spec(&self) -> EntitySpec {
        let mut entity_spec = self.entity_spec.clone();
        if let Some(device_class) = self.device_class {
            entity_spec.info.original_device_class = Some(device_class.as_str().to_owned());
        }
        if let Some(unit) = self.suggested_unit.as_ref().or(self.native_unit.as_ref()) {
            // EntitySpec has one unit fallback; retain the initial display unit
            // there while the descriptor keeps the provider's native unit.
            entity_spec.info.unit_of_measurement = Some(unit.as_str().to_owned());
        }
        if let Some(state_class) = self.state_class {
            if let Some(capabilities) = entity_spec
                .info
                .capabilities
                .as_mut()
                .and_then(Value::as_object_mut)
            {
                capabilities.insert("state_class".into(), json!(state_class.as_str()));
            } else if entity_spec.info.capabilities.is_none() {
                entity_spec.info.capabilities = Some(json!({
                    "state_class": state_class.as_str()
                }));
            }
        } else if let Some(options) = self.options.as_ref().filter(|options| !options.is_empty()) {
            if let Some(capabilities) = entity_spec
                .info
                .capabilities
                .as_mut()
                .and_then(Value::as_object_mut)
            {
                capabilities.insert("options".into(), json!(options));
            } else if entity_spec.info.capabilities.is_none() {
                entity_spec.info.capabilities = Some(json!({ "options": options }));
            }
        }
        entity_spec
    }

    /// Return the device class, if any.
    #[must_use]
    pub const fn device_class(&self) -> Option<SensorDeviceClass> {
        self.device_class
    }

    /// Return the state class, if any.
    #[must_use]
    pub const fn state_class(&self) -> Option<SensorStateClass> {
        self.state_class
    }

    /// Return the provider's native unit, if any.
    #[must_use]
    pub fn native_unit(&self) -> Option<&SensorUnit> {
        self.raw_native_unit.as_ref()
    }

    /// Return the suggested display unit, if any.
    #[must_use]
    pub fn suggested_unit(&self) -> Option<&SensorUnit> {
        self.suggested_unit.as_ref()
    }

    /// Return the enum options, if the description provides them.
    #[must_use]
    pub fn options(&self) -> Option<&[String]> {
        self.options.as_deref()
    }

    /// Return the suggested display precision, if set.
    #[must_use]
    pub const fn suggested_display_precision(&self) -> Option<usize> {
        self.suggested_display_precision
    }

    /// Project one dynamic reading into a lifecycle snapshot.
    pub fn project(
        &self,
        reading: SensorReading,
    ) -> Result<SensorProjection, SensorProjectionError> {
        self.project_with_options(reading, SensorProjectionOptions::default())
    }

    /// Project a reading with an effective user unit or automatic temperature
    /// unit supplied by the application.
    pub fn project_with_options(
        &self,
        reading: SensorReading,
        options: SensorProjectionOptions,
    ) -> Result<SensorProjection, SensorProjectionError> {
        let mut warnings = Vec::new();
        if let (Some(device_class), Some(state_class)) = (self.device_class, self.state_class)
            && !allowed_state_classes(device_class).contains(&state_class)
        {
            warnings.push(SensorWarning::InvalidDeviceStateClass {
                device_class,
                state_class,
                allowed: allowed_state_classes(device_class).to_vec(),
            });
        }

        // An unavailable snapshot has no provider value or dynamic attributes.
        // EntityLifecycle will write `unavailable` independently of `None`.
        if !reading.available {
            return Ok(SensorProjection {
                snapshot: EntitySnapshot {
                    state: None,
                    available: false,
                    attributes: Map::new(),
                    context: reading.context,
                },
                warnings,
            });
        }

        let display_unit = match self.resolve_display_unit(&options) {
            Ok(unit) => unit,
            Err(error) => return Err(SensorProjectionError::new(error, warnings)),
        };

        if self
            .device_class
            .is_some_and(SensorDeviceClass::is_non_numeric)
            && display_unit
                .as_ref()
                .is_some_and(|unit| !unit.as_str().is_empty())
        {
            return Err(SensorProjectionError::new(
                SensorErrorKind::NonNumericDeviceClassHasUnit {
                    device_class: self.device_class.expect("checked above"),
                    unit: display_unit.expect("checked above"),
                },
                warnings,
            ));
        }

        let state = if let Some(value) = reading.value.as_ref() {
            match self.project_value(value, display_unit.as_ref(), &mut warnings) {
                Ok(state) => state,
                Err(error) => return Err(SensorProjectionError::new(error, warnings)),
            }
        } else {
            None
        };

        if reading.last_reset.is_some() && self.state_class != Some(SensorStateClass::Total) {
            return Err(SensorProjectionError::new(
                SensorErrorKind::LastResetRequiresTotal {
                    state_class: self.state_class,
                },
                warnings,
            ));
        }

        let mut attributes = reading.attributes;
        if let Some(device_class) = self.device_class {
            attributes.insert("device_class".into(), json!(device_class.as_str()));
        }
        if let Some(state_class) = self.state_class {
            attributes.insert("state_class".into(), json!(state_class.as_str()));
        }
        if let Some(unit) = display_unit {
            attributes.insert("unit_of_measurement".into(), json!(unit.as_str()));
        }
        if let Some(options) = &self.options {
            attributes.insert("options".into(), json!(options));
        }
        if let Some(precision) = self.suggested_display_precision {
            attributes.insert("suggested_display_precision".into(), json!(precision));
        }
        if let Some(last_reset) = reading.last_reset {
            attributes.insert("last_reset".into(), json!(format_datetime_iso(last_reset)));
        }

        Ok(SensorProjection {
            snapshot: EntitySnapshot {
                state,
                available: true,
                attributes,
                context: reading.context,
            },
            warnings,
        })
    }

    fn resolve_display_unit(
        &self,
        options: &SensorProjectionOptions,
    ) -> Result<Option<SensorUnit>, SensorErrorKind> {
        let automatic_temperature_unit = if self.device_class
            == Some(SensorDeviceClass::Temperature)
            && self
                .native_unit
                .as_ref()
                .is_some_and(|unit| is_temperature_unit(unit.as_str()))
        {
            options.temperature_unit.as_ref()
        } else {
            None
        };
        let display_unit = options
            .user_unit
            .as_ref()
            .or(self.suggested_unit.as_ref())
            .or(automatic_temperature_unit)
            .or(self.native_unit.as_ref());

        let Some(display_unit) = display_unit else {
            return Ok(None);
        };
        if self.native_unit.as_ref() != Some(display_unit)
            && unit_family_for(self.device_class).is_none()
        {
            return Err(SensorErrorKind::UnsupportedUnitConversion {
                device_class: self.device_class,
                from: self.native_unit.clone(),
                to: display_unit.clone(),
            });
        }
        Ok(Some(display_unit.clone()))
    }

    fn project_value(
        &self,
        value: &SensorValue,
        display_unit: Option<&SensorUnit>,
        warnings: &mut Vec<SensorWarning>,
    ) -> Result<Option<String>, SensorErrorKind> {
        match self.device_class {
            Some(SensorDeviceClass::Timestamp | SensorDeviceClass::Uptime) => {
                return timestamp_value(value, self.device_class.expect("matched above")).map(Some);
            }
            Some(SensorDeviceClass::Date) => return date_value(value).map(Some),
            _ => {}
        }

        if self.options.is_some() || self.device_class == Some(SensorDeviceClass::Enum) {
            if self.device_class != Some(SensorDeviceClass::Enum) {
                return Err(SensorErrorKind::OptionsRequireEnum {
                    device_class: self.device_class,
                });
            }
            if let Some(options) = &self.options
                && !options.is_empty()
                && !options
                    .iter()
                    .any(|option| matches!(value, SensorValue::Text(state) if option == state))
            {
                return Err(SensorErrorKind::EnumValueNotInOptions {
                    value: format_plain_value(value),
                });
            }
            return Ok(Some(format_plain_value(value)));
        }

        if !numeric_state_expected(self) {
            return Ok(Some(format_plain_value(value)));
        }

        let from_unit = self.native_unit.as_ref().map(SensorUnit::as_str);
        let to_unit = display_unit.map(SensorUnit::as_str);
        let conversion_needed = from_unit != to_unit;
        let numeric = parse_numeric_value(value, conversion_needed)?;
        if numeric.value.is_some_and(|number| !number.is_finite()) {
            return Err(SensorErrorKind::NonFiniteValue {
                value: numeric.original,
            });
        }

        let converted = if conversion_needed {
            let family = unit_family_for(self.device_class)
                .expect("display-unit changes were checked by resolve_display_unit");
            hass_units::convert_optional(family, numeric.value, from_unit, to_unit)
                .map_err(|error| SensorErrorKind::UnitConversion { error })?
        } else {
            numeric.value
        };
        if let Some(number) = converted
            && !number.is_finite()
        {
            return Err(SensorErrorKind::NonFiniteValue {
                value: number.to_string(),
            });
        }

        if let Some(device_class) = self.device_class
            && let Some(units) = unit_tables::native_units(device_class.as_str())
            && !units.contains(&self.native_unit.as_ref().map(SensorUnit::as_str))
        {
            warnings.push(SensorWarning::InvalidDeviceClassUnit {
                device_class,
                unit: self.native_unit.clone(),
                expected: units
                    .iter()
                    .map(|unit| unit.unwrap_or("no unit of measurement").to_owned())
                    .collect(),
            });
        }

        if self.state_class == Some(SensorStateClass::MeasurementAngle)
            && self.native_unit.as_ref().map(SensorUnit::as_str) != Some("°")
        {
            return Err(SensorErrorKind::InvalidStateClassUnit {
                state_class: SensorStateClass::MeasurementAngle,
                unit: self.native_unit.clone(),
                expected: vec!["°".to_owned()],
            });
        }

        Ok(
            match (conversion_needed, numeric.unchanged_state, converted) {
                (false, Some(state), _) => Some(state),
                (_, _, Some(number)) => Some(format_number(number)),
                (_, _, None) => None,
            },
        )
    }
}

/// Unit choices supplied by the application for one projection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SensorProjectionOptions {
    /// Effective user-selected unit from entity registry options, if present.
    pub user_unit: Option<SensorUnit>,
    /// Host temperature unit, considered only for a temperature class with a
    /// recognized native temperature unit.
    pub temperature_unit: Option<SensorUnit>,
}

/// Projected state plus warning-only metadata issues.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorProjection {
    /// Snapshot ready for `EntityHandle::publish`.
    pub snapshot: EntitySnapshot,
    /// Source-compatible warning conditions that did not reject the reading.
    pub warnings: Vec<SensorWarning>,
}

/// Warning-only mismatch, matching the corresponding Core warning path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SensorWarning {
    /// The pinned device-class table does not permit the supplied state class.
    InvalidDeviceStateClass {
        device_class: SensorDeviceClass,
        state_class: SensorStateClass,
        allowed: Vec<SensorStateClass>,
    },
    /// The native unit is outside the pinned device-class unit set.
    InvalidDeviceClassUnit {
        device_class: SensorDeviceClass,
        unit: Option<SensorUnit>,
        expected: Vec<String>,
    },
}

/// Projection error kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SensorErrorKind {
    WrongDomain {
        actual: String,
    },
    NonNumericDeviceClassHasUnit {
        device_class: SensorDeviceClass,
        unit: SensorUnit,
    },
    InvalidDateValue,
    InvalidTimestampValue {
        device_class: SensorDeviceClass,
    },
    OptionsRequireEnum {
        device_class: Option<SensorDeviceClass>,
    },
    EnumValueNotInOptions {
        value: String,
    },
    NumericValueRequired {
        value: String,
    },
    NonFiniteValue {
        value: String,
    },
    UnsupportedUnitConversion {
        device_class: Option<SensorDeviceClass>,
        from: Option<SensorUnit>,
        to: SensorUnit,
    },
    UnitConversion {
        error: ConversionError,
    },
    InvalidStateClassUnit {
        state_class: SensorStateClass,
        unit: Option<SensorUnit>,
        expected: Vec<String>,
    },
    LastResetRequiresTotal {
        state_class: Option<SensorStateClass>,
    },
}

/// Projection failure together with any warning-only metadata issues observed
/// before the failing value was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SensorProjectionError {
    /// Specific validation failure.
    pub kind: SensorErrorKind,
    /// Warning-only metadata conditions observed before the error.
    pub warnings: Vec<SensorWarning>,
}

impl SensorProjectionError {
    fn new(kind: SensorErrorKind, warnings: Vec<SensorWarning>) -> Self {
        Self { kind, warnings }
    }
}

impl fmt::Display for SensorProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SensorErrorKind::WrongDomain { actual } => {
                write!(
                    formatter,
                    "sensor descriptor requires domain sensor, got {actual:?}"
                )
            }
            SensorErrorKind::NonNumericDeviceClassHasUnit { device_class, unit } => write!(
                formatter,
                "non-numeric sensor class {device_class} cannot publish unit {unit}"
            ),
            SensorErrorKind::InvalidDateValue => {
                formatter.write_str("date device class requires a date value")
            }
            SensorErrorKind::InvalidTimestampValue { device_class } => write!(
                formatter,
                "{device_class} device class requires an aware datetime value"
            ),
            SensorErrorKind::OptionsRequireEnum { device_class } => write!(
                formatter,
                "sensor options require the enum device class, got {device_class:?}"
            ),
            SensorErrorKind::EnumValueNotInOptions { value } => {
                write!(formatter, "enum state {value:?} is not in its options")
            }
            SensorErrorKind::NumericValueRequired { value } => {
                write!(
                    formatter,
                    "numeric sensor metadata requires a number, got {value:?}"
                )
            }
            SensorErrorKind::NonFiniteValue { value } => {
                write!(formatter, "sensor value must be finite, got {value:?}")
            }
            SensorErrorKind::UnsupportedUnitConversion {
                device_class,
                from,
                to,
            } => write!(
                formatter,
                "cannot convert sensor unit from {from:?} to {to} for {device_class:?}"
            ),
            SensorErrorKind::UnitConversion { error } => write!(formatter, "{error}"),
            SensorErrorKind::InvalidStateClassUnit {
                state_class,
                unit,
                expected,
            } => write!(
                formatter,
                "state class {state_class} does not permit unit {unit:?}; expected {expected:?}"
            ),
            SensorErrorKind::LastResetRequiresTotal { state_class } => write!(
                formatter,
                "last_reset requires total state class, got {state_class:?}"
            ),
        }
    }
}

impl Error for SensorProjectionError {}

struct NumericValue {
    value: Option<f64>,
    original: String,
    unchanged_state: Option<String>,
}

fn numeric_state_expected(descriptor: &SensorDescriptor) -> bool {
    descriptor
        .device_class
        .is_some_and(|class| !class.is_non_numeric())
        || descriptor.state_class.is_some()
        || descriptor.native_unit.is_some()
        || descriptor.suggested_display_precision.is_some()
}

fn allowed_state_classes(device_class: SensorDeviceClass) -> &'static [SensorStateClass] {
    use SensorDeviceClass as D;
    use SensorStateClass as S;
    const NONE: &[S] = &[];
    const MEASUREMENT: &[S] = &[S::Measurement];
    const TOTAL: &[S] = &[S::Total];
    const TOTALS: &[S] = &[S::Total, S::TotalIncreasing];
    const MEASUREMENT_TOTALS: &[S] = &[S::Measurement, S::Total, S::TotalIncreasing];
    const MEASUREMENT_ANGLE: &[S] = &[S::MeasurementAngle];
    const ALL: &[S] = &[
        S::Measurement,
        S::MeasurementAngle,
        S::Total,
        S::TotalIncreasing,
    ];

    match device_class {
        D::Date | D::Enum | D::Timestamp | D::Uptime => NONE,
        D::Area | D::DataSize | D::Distance | D::Duration | D::Precipitation => ALL,
        D::Energy | D::Gas | D::ReactiveEnergy | D::Volume | D::Water => TOTALS,
        D::Monetary => TOTAL,
        D::Weight => MEASUREMENT_TOTALS,
        D::WindDirection => MEASUREMENT_ANGLE,
        D::AbsoluteHumidity
        | D::ApparentPower
        | D::Aqi
        | D::AtmosphericPressure
        | D::Battery
        | D::BloodGlucoseConcentration
        | D::CarbonMonoxide
        | D::CarbonDioxide
        | D::Conductivity
        | D::Current
        | D::DataRate
        | D::EnergyDistance
        | D::EnergyStorage
        | D::Frequency
        | D::Humidity
        | D::Illuminance
        | D::Irradiance
        | D::Moisture
        | D::NitrogenDioxide
        | D::NitrogenMonoxide
        | D::NitrousOxide
        | D::Ozone
        | D::Ph
        | D::Pm1
        | D::Pm10
        | D::Pm25
        | D::Pm4
        | D::PowerFactor
        | D::Power
        | D::PrecipitationIntensity
        | D::Pressure
        | D::Radon
        | D::ReactivePower
        | D::SignalStrength
        | D::SoundPressure
        | D::Speed
        | D::SulphurDioxide
        | D::Temperature
        | D::TemperatureDelta
        | D::VolatileOrganicCompounds
        | D::VolatileOrganicCompoundsParts
        | D::Voltage
        | D::VolumeStorage
        | D::VolumeFlowRate
        | D::WindSpeed => MEASUREMENT,
    }
}

fn compatible_native_unit(unit: &str) -> &str {
    // sensor/const.py AMBIGUOUS_UNITS, applied only to provider-native units.
    match unit {
        "µSv/h" => "μSv/h",
        "µS/cm" => "μS/cm",
        "µV" => "μV",
        "µg/ft³" => "μg/ft³",
        "µg/m³" => "μg/m³",
        "µmol/s⋅m²" => "μmol/s⋅m²",
        "µg" => "μg",
        "µs" => "μs",
        "mVAr" => "mvar",
        "VAr" => "var",
        "kVAr" => "kvar",
        "VArh" => "varh",
        "kVArh" => "kvarh",
        _ => unit,
    }
}

fn unit_family_for(device_class: Option<SensorDeviceClass>) -> Option<UnitFamily> {
    use SensorDeviceClass as D;
    use UnitFamily as F;
    match device_class? {
        D::ApparentPower => Some(F::ApparentPower),
        D::AbsoluteHumidity | D::VolatileOrganicCompounds => Some(F::MassVolumeConcentration),
        D::Area => Some(F::Area),
        D::AtmosphericPressure | D::Pressure => Some(F::Pressure),
        D::BloodGlucoseConcentration => Some(F::BloodGlucoseConcentration),
        D::CarbonMonoxide => Some(F::CarbonMonoxideConcentration),
        D::Conductivity => Some(F::Conductivity),
        D::Current => Some(F::ElectricCurrent),
        D::DataRate => Some(F::DataRate),
        D::DataSize => Some(F::Information),
        D::Distance | D::Precipitation => Some(F::Distance),
        D::Duration => Some(F::Duration),
        D::Energy | D::EnergyStorage => Some(F::Energy),
        D::EnergyDistance => Some(F::EnergyDistance),
        D::Frequency => Some(F::Frequency),
        D::Gas | D::Volume | D::VolumeStorage | D::Water => Some(F::Volume),
        D::NitrogenDioxide => Some(F::NitrogenDioxideConcentration),
        D::NitrogenMonoxide => Some(F::NitrogenMonoxideConcentration),
        D::Ozone => Some(F::OzoneConcentration),
        D::Power => Some(F::Power),
        D::PowerFactor | D::VolatileOrganicCompoundsParts => Some(F::UnitlessRatio),
        D::PrecipitationIntensity | D::Speed | D::WindSpeed => Some(F::Speed),
        D::Radon => Some(F::RadiationConcentration),
        D::ReactiveEnergy => Some(F::ReactiveEnergy),
        D::ReactivePower => Some(F::ReactivePower),
        D::SulphurDioxide => Some(F::SulphurDioxideConcentration),
        D::Temperature => Some(F::Temperature),
        D::TemperatureDelta => Some(F::TemperatureDelta),
        D::Voltage => Some(F::ElectricPotential),
        D::VolumeFlowRate => Some(F::VolumeFlowRate),
        D::Weight => Some(F::Mass),
        _ => None,
    }
}

fn is_temperature_unit(unit: &str) -> bool {
    hass_units::is_valid_unit(UnitFamily::Temperature, Some(unit))
}

fn parse_numeric_value(
    value: &SensorValue,
    conversion_needed: bool,
) -> Result<NumericValue, SensorErrorKind> {
    let invalid = || SensorErrorKind::NumericValueRequired {
        value: format_plain_value(value),
    };
    match value {
        SensorValue::Integer(integer) => Ok(NumericValue {
            value: if conversion_needed {
                Some(*integer as f64)
            } else {
                None
            },
            original: integer.to_string(),
            unchanged_state: Some(integer.to_string()),
        }),
        SensorValue::Unsigned(integer) => Ok(NumericValue {
            value: if conversion_needed {
                Some(*integer as f64)
            } else {
                None
            },
            original: integer.to_string(),
            unchanged_state: Some(integer.to_string()),
        }),
        SensorValue::Boolean(boolean) => {
            let state = if *boolean { "True" } else { "False" }.to_owned();
            Ok(NumericValue {
                value: if conversion_needed {
                    Some(f64::from(u8::from(*boolean)))
                } else {
                    None
                },
                original: state.clone(),
                unchanged_state: Some(state),
            })
        }
        SensorValue::Number(number) => Ok(NumericValue {
            value: Some(*number),
            original: number.to_string(),
            unchanged_state: None,
        }),
        SensorValue::Decimal(decimal) => {
            let number = decimal.parse::<f64>().map_err(|_| invalid())?;
            Ok(NumericValue {
                value: Some(number),
                original: decimal.clone(),
                unchanged_state: Some(decimal.clone()),
            })
        }
        SensorValue::Text(text) => {
            let trimmed = text.trim();
            if !trimmed
                .chars()
                .any(|character| matches!(character, '.' | 'e' | 'E'))
                && let Some(integer) = canonical_integer_text(trimmed)
            {
                return Ok(NumericValue {
                    value: if conversion_needed {
                        Some(integer.parse::<f64>().map_err(|_| invalid())?)
                    } else {
                        None
                    },
                    original: text.clone(),
                    unchanged_state: Some(integer),
                });
            }
            let number = trimmed.parse::<f64>().map_err(|_| invalid())?;
            Ok(NumericValue {
                value: Some(number),
                original: text.clone(),
                unchanged_state: None,
            })
        }
        SensorValue::Date(_) | SensorValue::DateTime(_) | SensorValue::NaiveDateTime(_) => {
            Err(invalid())
        }
    }
}

fn canonical_integer_text(text: &str) -> Option<String> {
    let (negative, digits) = if let Some(digits) = text.strip_prefix('-') {
        (true, digits)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty()
        || bytes.iter().enumerate().any(|(index, byte)| {
            !byte.is_ascii_digit()
                && !(*byte == b'_'
                    && index > 0
                    && bytes[index - 1].is_ascii_digit()
                    && bytes.get(index + 1).is_some_and(u8::is_ascii_digit))
        })
    {
        return None;
    }
    let digits = digits.replace('_', "");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        Some("0".to_owned())
    } else if negative {
        Some(format!("-{digits}"))
    } else {
        Some(digits.to_owned())
    }
}

fn timestamp_value(
    value: &SensorValue,
    device_class: SensorDeviceClass,
) -> Result<String, SensorErrorKind> {
    let SensorValue::DateTime(datetime) = value else {
        return Err(SensorErrorKind::InvalidTimestampValue { device_class });
    };
    let utc = datetime.with_timezone(&Utc);
    Ok(utc.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}

fn date_value(value: &SensorValue) -> Result<String, SensorErrorKind> {
    let SensorValue::Date(date) = value else {
        return Err(SensorErrorKind::InvalidDateValue);
    };
    Ok(date.format("%Y-%m-%d").to_string())
}

fn format_plain_value(value: &SensorValue) -> String {
    match value {
        SensorValue::Text(text) => text.clone(),
        SensorValue::Integer(integer) => integer.to_string(),
        SensorValue::Unsigned(integer) => integer.to_string(),
        SensorValue::Boolean(boolean) => if *boolean { "True" } else { "False" }.to_owned(),
        SensorValue::Number(number) => format_number(*number),
        SensorValue::Decimal(decimal) => decimal.clone(),
        SensorValue::Date(date) => date.format("%Y-%m-%d").to_string(),
        SensorValue::DateTime(datetime) => format_python_datetime(*datetime),
        SensorValue::NaiveDateTime(datetime) => format_naive_datetime(*datetime),
    }
}

fn format_number(number: f64) -> String {
    if number.is_nan() {
        return "nan".to_owned();
    }
    // Rust's debug float representation uses the same short decimal form and
    // fixed/scientific boundary as Python. Python additionally writes an
    // explicit exponent sign and at least two exponent digits.
    let state = format!("{number:?}");
    if let Some((mantissa, exponent)) = state.split_once('e') {
        let (sign, digits) = if let Some(digits) = exponent.strip_prefix('-') {
            ('-', digits)
        } else {
            ('+', exponent.strip_prefix('+').unwrap_or(exponent))
        };
        format!("{mantissa}e{sign}{digits:0>2}")
    } else {
        state
    }
}

fn format_python_datetime(datetime: DateTime<chrono::FixedOffset>) -> String {
    let micros = datetime.timestamp_subsec_micros();
    let fraction = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    format!(
        "{}{}{}",
        datetime.format("%Y-%m-%d %H:%M:%S"),
        fraction,
        datetime.format("%:z")
    )
}

fn format_naive_datetime(datetime: NaiveDateTime) -> String {
    let micros = datetime.and_utc().timestamp_subsec_micros();
    let fraction = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    format!("{}{}", datetime.format("%Y-%m-%d %H:%M:%S"), fraction)
}

fn format_datetime_iso(datetime: DateTime<chrono::FixedOffset>) -> String {
    let micros = datetime.timestamp_subsec_micros();
    let precision = if micros == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    datetime.to_rfc3339_opts(precision, false)
}
