//! Unit-system selection and sensor-unit preferences from Home Assistant.
//!
//! The pinned source contract and its boundaries are documented in
//! `docs/UNITS-PORT.md`. Selection values are typed, while conversion methods
//! reuse this crate's converter families.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use super::{ConversionError, UnitFamily, convert};

macro_rules! unit_enum {
    ($name:ident { $($variant:ident => $unit:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// Return Home Assistant's serialized unit identifier.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $unit),+
                }
            }

            fn parse(value: &str) -> Option<Self> {
                match value {
                    $($unit => Some(Self::$variant),)+
                    _ => None,
                }
            }

            const fn supported() -> &'static [&'static str] {
                const UNITS: &[&str] = &[$($unit),+];
                UNITS
            }
        }
    };
}

unit_enum!(LengthUnit {
    Meters => "m",
    Millimeters => "mm",
    Centimeters => "cm",
    Kilometers => "km",
    Inches => "in",
    Feet => "ft",
    Yards => "yd",
    Miles => "mi",
    NauticalMiles => "nmi",
});

unit_enum!(AreaUnit {
    SquareMeters => "m²",
    SquareCentimeters => "cm²",
    SquareMillimeters => "mm²",
    SquareKilometers => "km²",
    SquareInches => "in²",
    SquareFeet => "ft²",
    SquareYards => "yd²",
    SquareMiles => "mi²",
    Acres => "ac",
    Hectares => "ha",
});

unit_enum!(MassUnit {
    Pounds => "lb",
    Ounces => "oz",
    Kilograms => "kg",
    Grams => "g",
});

unit_enum!(PressureUnit {
    Millipascals => "mPa",
    Pascals => "Pa",
    Hectopascals => "hPa",
    Kilopascals => "kPa",
    Bars => "bar",
    Centibars => "cbar",
    Millibars => "mbar",
    InchesOfMercury => "inHg",
    InchesOfWater => "inH₂O",
    PoundsPerSquareInch => "psi",
    MillimetersOfMercury => "mmHg",
    Atmospheres => "atm",
});

unit_enum!(TemperatureUnit {
    Celsius => "°C",
    Fahrenheit => "°F",
});

unit_enum!(VolumeUnit {
    Liters => "L",
    Milliliters => "mL",
    Gallons => "gal",
    FluidOunces => "fl. oz.",
    CubicMeters => "m³",
    CubicFeet => "ft³",
    CentumCubicFeet => "CCF",
    MilleCubicFeet => "MCF",
});

// The source SpeedConverter accepts speed and volumetric-flux units here.
unit_enum!(WindSpeedUnit {
    InchesPerDay => "in/d",
    InchesPerHour => "in/h",
    MillimetersPerDay => "mm/d",
    MillimetersPerHour => "mm/h",
    FeetPerSecond => "ft/s",
    InchesPerSecond => "in/s",
    KilometersPerHour => "km/h",
    Knots => "kn",
    MetersPerMinute => "m/min",
    MetersPerSecond => "m/s",
    MillimetersPerSecond => "mm/s",
    MilesPerHour => "mph",
    Beaufort => "Beaufort",
});

/// A typed unit selection for the eight properties accepted by `UnitSystem`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnitSystemProperty {
    Length,
    AccumulatedPrecipitation,
    Area,
    Mass,
    Pressure,
    Temperature,
    Volume,
    WindSpeed,
}

impl UnitSystemProperty {
    /// Return the exact key used by Home Assistant's unit-system dictionary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::AccumulatedPrecipitation => "accumulated_precipitation",
            Self::Area => "area",
            Self::Mass => "mass",
            Self::Pressure => "pressure",
            Self::Temperature => "temperature",
            Self::Volume => "volume",
            Self::WindSpeed => "wind_speed",
        }
    }
}

/// Return all values accepted for a unit-system selection property.
///
/// These are the pinned `UnitSystem` constructor domains. They intentionally
/// differ from converter support for temperature (Kelvin is convertible but
/// is not an accepted configured temperature unit) and mass (stone is a
/// convertible mass unit but is not an accepted configured mass unit).
pub const fn configurable_units(property: UnitSystemProperty) -> &'static [&'static str] {
    match property {
        UnitSystemProperty::Length | UnitSystemProperty::AccumulatedPrecipitation => {
            LengthUnit::supported()
        }
        UnitSystemProperty::Area => AreaUnit::supported(),
        UnitSystemProperty::Mass => MassUnit::supported(),
        UnitSystemProperty::Pressure => PressureUnit::supported(),
        UnitSystemProperty::Temperature => TemperatureUnit::supported(),
        UnitSystemProperty::Volume => VolumeUnit::supported(),
        UnitSystemProperty::WindSpeed => WindSpeedUnit::supported(),
    }
}

/// All unit selections stored by a unit system.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitSystemUnits {
    pub accumulated_precipitation: LengthUnit,
    pub area: AreaUnit,
    pub length: LengthUnit,
    pub mass: MassUnit,
    pub pressure: PressureUnit,
    pub temperature: TemperatureUnit,
    pub volume: VolumeUnit,
    pub wind_speed: WindSpeedUnit,
}

/// The canonical metric selection defaults.
pub const METRIC_UNITS: UnitSystemUnits = UnitSystemUnits {
    accumulated_precipitation: LengthUnit::Millimeters,
    area: AreaUnit::SquareMeters,
    length: LengthUnit::Kilometers,
    mass: MassUnit::Grams,
    pressure: PressureUnit::Pascals,
    temperature: TemperatureUnit::Celsius,
    volume: VolumeUnit::Liters,
    wind_speed: WindSpeedUnit::MetersPerSecond,
};

/// The canonical US customary selection defaults.
pub const US_CUSTOMARY_UNITS: UnitSystemUnits = UnitSystemUnits {
    accumulated_precipitation: LengthUnit::Inches,
    area: AreaUnit::SquareFeet,
    length: LengthUnit::Miles,
    mass: MassUnit::Pounds,
    pressure: PressureUnit::PoundsPerSquareInch,
    temperature: TemperatureUnit::Fahrenheit,
    volume: VolumeUnit::Gallons,
    wind_speed: WindSpeedUnit::MilesPerHour,
};

/// Raw unit names for constructing a user-defined unit system.
///
/// This mirrors the pinned constructor's named selection arguments and lets
/// invalid source values be reported instead of silently coerced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitSystemUnitNames<'a> {
    pub accumulated_precipitation: &'a str,
    pub area: &'a str,
    pub length: &'a str,
    pub mass: &'a str,
    pub pressure: &'a str,
    pub temperature: &'a str,
    pub volume: &'a str,
    pub wind_speed: &'a str,
}

/// One `(device_class, original_unit) -> converted_unit` table entry.
///
/// `None` keys are kept separate from empty or unknown strings, matching the
/// tuple lookup used by Home Assistant's `UnitSystem.get_converted_unit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitSystemConversion<'a> {
    pub device_class: Option<&'a str>,
    pub original_unit: Option<&'a str>,
    pub converted_unit: &'a str,
}

impl<'a> UnitSystemConversion<'a> {
    pub const fn new(
        device_class: Option<&'a str>,
        original_unit: Option<&'a str>,
        converted_unit: &'a str,
    ) -> Self {
        Self {
            device_class,
            original_unit,
            converted_unit,
        }
    }
}

/// The built-in unit-system keys accepted by `get_unit_system`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnitSystemKind {
    Metric,
    UsCustomary,
}

impl UnitSystemKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metric => "metric",
            Self::UsCustomary => "us_customary",
        }
    }

    pub const fn unit_system(self) -> &'static UnitSystem<'static> {
        match self {
            Self::Metric => &METRIC_SYSTEM,
            Self::UsCustomary => &US_CUSTOMARY_SYSTEM,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversionTable<'a> {
    Metric,
    UsCustomary,
    Custom(&'a [UnitSystemConversion<'a>]),
}

/// An immutable, source-compatible unit system.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitSystem<'a> {
    name: &'a str,
    units: UnitSystemUnits,
    conversions: ConversionTable<'a>,
}

/// Canonical metric unit system.
pub static METRIC_SYSTEM: UnitSystem<'static> = UnitSystem {
    name: "metric",
    units: METRIC_UNITS,
    conversions: ConversionTable::Metric,
};

/// Canonical US customary unit system.
pub static US_CUSTOMARY_SYSTEM: UnitSystem<'static> = UnitSystem {
    name: "us_customary",
    units: US_CUSTOMARY_UNITS,
    conversions: ConversionTable::UsCustomary,
};

/// Deprecated source alias for [`US_CUSTOMARY_SYSTEM`].
pub static IMPERIAL_SYSTEM: &UnitSystem<'static> = &US_CUSTOMARY_SYSTEM;

impl<'a> UnitSystem<'a> {
    /// Build a custom system from typed selections and a replacement map.
    ///
    /// As in Home Assistant, this conversion table replaces canonical map
    /// behavior; it is not merged with metric or US customary mappings.
    pub const fn from_typed(
        name: &'a str,
        units: UnitSystemUnits,
        conversions: &'a [UnitSystemConversion<'a>],
    ) -> Self {
        Self {
            name,
            units,
            conversions: ConversionTable::Custom(conversions),
        }
    }

    /// Build a custom system after validating each raw selection.
    ///
    /// Invalid selections are collected and formatted in the same order as the
    /// pinned Python constructor. Custom conversion entries replace the
    /// canonical tables, as they do in the source.
    pub fn try_new(
        name: &'a str,
        names: UnitSystemUnitNames<'_>,
        conversions: &'a [UnitSystemConversion<'a>],
    ) -> Result<Self, UnitSystemError> {
        let mut invalid = Vec::new();
        let accumulated_precipitation = parse_unit::<LengthUnit>(names.accumulated_precipitation);
        if accumulated_precipitation.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::AccumulatedPrecipitation,
                names.accumulated_precipitation,
            ));
        }
        let area = parse_unit::<AreaUnit>(names.area);
        if area.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Area,
                names.area,
            ));
        }
        let temperature = parse_unit::<TemperatureUnit>(names.temperature);
        if temperature.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Temperature,
                names.temperature,
            ));
        }
        let length = parse_unit::<LengthUnit>(names.length);
        if length.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Length,
                names.length,
            ));
        }
        let wind_speed = parse_unit::<WindSpeedUnit>(names.wind_speed);
        if wind_speed.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::WindSpeed,
                names.wind_speed,
            ));
        }
        let volume = parse_unit::<VolumeUnit>(names.volume);
        if volume.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Volume,
                names.volume,
            ));
        }
        let mass = parse_unit::<MassUnit>(names.mass);
        if mass.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Mass,
                names.mass,
            ));
        }
        let pressure = parse_unit::<PressureUnit>(names.pressure);
        if pressure.is_none() {
            invalid.push(InvalidUnitSelection::new(
                UnitSystemProperty::Pressure,
                names.pressure,
            ));
        }
        if !invalid.is_empty() {
            return Err(UnitSystemError::InvalidUnitSelections(invalid));
        }

        Ok(Self::from_typed(
            name,
            UnitSystemUnits {
                accumulated_precipitation: accumulated_precipitation
                    .expect("validated accumulated precipitation unit"),
                area: area.expect("validated area unit"),
                length: length.expect("validated length unit"),
                mass: mass.expect("validated mass unit"),
                pressure: pressure.expect("validated pressure unit"),
                temperature: temperature.expect("validated temperature unit"),
                volume: volume.expect("validated volume unit"),
                wind_speed: wind_speed.expect("validated wind speed unit"),
            },
            conversions,
        ))
    }

    pub const fn name(self) -> &'a str {
        self.name
    }

    pub const fn units(self) -> UnitSystemUnits {
        self.units
    }

    pub const fn accumulated_precipitation_unit(self) -> LengthUnit {
        self.units.accumulated_precipitation
    }

    pub const fn area_unit(self) -> AreaUnit {
        self.units.area
    }

    pub const fn length_unit(self) -> LengthUnit {
        self.units.length
    }

    pub const fn mass_unit(self) -> MassUnit {
        self.units.mass
    }

    pub const fn pressure_unit(self) -> PressureUnit {
        self.units.pressure
    }

    pub const fn temperature_unit(self) -> TemperatureUnit {
        self.units.temperature
    }

    pub const fn volume_unit(self) -> VolumeUnit {
        self.units.volume
    }

    pub const fn wind_speed_unit(self) -> WindSpeedUnit {
        self.units.wind_speed
    }

    /// Return the same property names and string values as source `as_dict()`.
    pub fn as_dict(self) -> BTreeMap<&'static str, &'static str> {
        [
            ("length", self.units.length.as_str()),
            (
                "accumulated_precipitation",
                self.units.accumulated_precipitation.as_str(),
            ),
            ("area", self.units.area.as_str()),
            ("mass", self.units.mass.as_str()),
            ("pressure", self.units.pressure.as_str()),
            ("temperature", self.units.temperature.as_str()),
            ("volume", self.units.volume.as_str()),
            ("wind_speed", self.units.wind_speed.as_str()),
        ]
        .into_iter()
        .collect()
    }

    /// Look up the selected unit for a device class and source unit.
    pub fn get_converted_unit(
        self,
        device_class: Option<&str>,
        original_unit: Option<&str>,
    ) -> Option<&'a str> {
        match self.conversions {
            ConversionTable::Metric => {
                metric_converted_unit(device_class, original_unit).map(|unit| unit as &'a str)
            }
            ConversionTable::UsCustomary => {
                us_customary_converted_unit(device_class, original_unit).map(|unit| unit as &'a str)
            }
            ConversionTable::Custom(conversions) => conversions
                .iter()
                .rev()
                .find(|entry| {
                    entry.device_class == device_class && entry.original_unit == original_unit
                })
                .map(|entry| entry.converted_unit),
        }
    }

    /// Convert a temperature using the selected target temperature unit.
    pub fn temperature(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Temperature,
            value,
            from_unit,
            Some(self.units.temperature.as_str()),
        )
    }

    /// Convert a length using the selected target length unit.
    pub fn length(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Distance,
            value,
            from_unit,
            Some(self.units.length.as_str()),
        )
    }

    /// Convert accumulated precipitation using the distance converter.
    pub fn accumulated_precipitation(
        self,
        value: f64,
        from_unit: Option<&str>,
    ) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Distance,
            value,
            from_unit,
            Some(self.units.accumulated_precipitation.as_str()),
        )
    }

    /// Convert an area using the selected target area unit.
    pub fn area(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Area,
            value,
            from_unit,
            Some(self.units.area.as_str()),
        )
    }

    /// Convert a pressure using the selected target pressure unit.
    pub fn pressure(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Pressure,
            value,
            from_unit,
            Some(self.units.pressure.as_str()),
        )
    }

    /// Convert a wind speed using the selected target wind-speed unit.
    pub fn wind_speed(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Speed,
            value,
            from_unit,
            Some(self.units.wind_speed.as_str()),
        )
    }

    /// Convert a volume using the selected target volume unit.
    pub fn volume(self, value: f64, from_unit: Option<&str>) -> Result<f64, ConversionError> {
        convert(
            UnitFamily::Volume,
            value,
            from_unit,
            Some(self.units.volume.as_str()),
        )
    }
}

/// A selection rejected by the pinned `UnitSystem` constructor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidUnitSelection {
    pub property: UnitSystemProperty,
    pub unit: String,
}

impl InvalidUnitSelection {
    fn new(property: UnitSystemProperty, unit: &str) -> Self {
        Self {
            property,
            unit: unit.to_owned(),
        }
    }
}

/// Selection or lookup failures from the native UnitSystem API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnitSystemError {
    InvalidKey { key: Option<String> },
    InvalidUnitSelections(Vec<InvalidUnitSelection>),
    InvalidSystemValue { value: String },
}

impl fmt::Display for UnitSystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey { key } => match key {
                Some(key) => write!(f, "`{key}` is not a valid unit system key"),
                None => f.write_str("`None` is not a valid unit system key"),
            },
            Self::InvalidUnitSelections(invalid) => {
                for (index, selection) in invalid.iter().enumerate() {
                    if index != 0 {
                        f.write_str(", ")?;
                    }
                    write!(
                        f,
                        "{} is not a recognized {} unit.",
                        selection.unit,
                        selection.property.as_str()
                    )?;
                }
                Ok(())
            }
            Self::InvalidSystemValue { value } => {
                write!(f, "{value} is not a valid unit system value")
            }
        }
    }
}

impl Error for UnitSystemError {}

/// Return a canonical built-in by its exact, case-sensitive source key.
pub fn get_unit_system(key: &str) -> Result<&'static UnitSystem<'static>, UnitSystemError> {
    get_unit_system_optional(Some(key))
}

/// Return a canonical built-in while preserving a missing key as a distinct error.
pub fn get_unit_system_optional(
    key: Option<&str>,
) -> Result<&'static UnitSystem<'static>, UnitSystemError> {
    match key {
        Some("metric") => Ok(&METRIC_SYSTEM),
        Some("us_customary") => Ok(&US_CUSTOMARY_SYSTEM),
        _ => Err(UnitSystemError::InvalidKey {
            key: key.map(str::to_owned),
        }),
    }
}

/// Normalize a configured value and resolve the deprecated `imperial` alias.
///
/// This mirrors `validate_unit_system`; unlike `get_unit_system`, it lowercases
/// and accepts `imperial` as the US customary value.
pub fn validate_unit_system(value: &str) -> Result<UnitSystemKind, UnitSystemError> {
    match value.to_lowercase().as_str() {
        "metric" => Ok(UnitSystemKind::Metric),
        "us_customary" | "imperial" => Ok(UnitSystemKind::UsCustomary),
        _ => Err(UnitSystemError::InvalidSystemValue {
            value: value.to_owned(),
        }),
    }
}

trait ParseUnit: Copy {
    fn parse_unit(value: &str) -> Option<Self>;
}

macro_rules! impl_parse_unit {
    ($($name:ident),+ $(,)?) => {
        $(
            impl ParseUnit for $name {
                fn parse_unit(value: &str) -> Option<Self> {
                    Self::parse(value)
                }
            }
        )+
    };
}

impl_parse_unit!(
    LengthUnit,
    AreaUnit,
    MassUnit,
    PressureUnit,
    TemperatureUnit,
    VolumeUnit,
    WindSpeedUnit
);

fn parse_unit<T: ParseUnit>(value: &str) -> Option<T> {
    T::parse_unit(value)
}

const METRIC_WIND_SPEED_UNITS: &[&str] =
    &["Beaufort", "ft/s", "in/s", "m/min", "m/s", "mm/s", "mph"];

const US_CUSTOMARY_WIND_SPEED_UNITS: &[&str] =
    &["Beaufort", "ft/s", "in/s", "km/h", "m/min", "m/s", "mm/s"];

fn metric_converted_unit(
    device_class: Option<&str>,
    original_unit: Option<&str>,
) -> Option<&'static str> {
    let (Some(device_class), Some(unit)) = (device_class, original_unit) else {
        return None;
    };
    let converted = match (device_class, unit) {
        ("area", "in²") => "cm²",
        ("area", "ft²" | "yd²") => "m²",
        ("area", "mi²") => "km²",
        ("area", "ac") => "ha",
        ("distance", "ft" | "yd") => "m",
        ("distance", "in") => "mm",
        ("distance", "mi" | "nmi") => "km",
        ("gas", "CCF" | "ft³" | "MCF") => "m³",
        ("precipitation", "in") => "mm",
        ("precipitation_intensity", "in/d") => "mm/d",
        ("precipitation_intensity", "in/h") => "mm/h",
        ("pressure", "psi" | "inH₂O") => "kPa",
        ("pressure", "inHg") => "hPa",
        ("radon", "pCi/L") => "Bq/m³",
        ("speed", "ft/s" | "mph") => "km/h",
        ("speed", "in/s") => "mm/s",
        ("speed", "in/d") => "mm/d",
        ("speed", "in/h") => "mm/h",
        ("volume", "CCF" | "ft³" | "MCF") => "m³",
        ("volume", "fl. oz.") => "mL",
        ("volume", "gal") => "L",
        ("volume_flow_rate", "ft³/min") => "m³/min",
        ("volume_flow_rate", "gal/min") => "L/min",
        ("volume_flow_rate", "gal/h") => "L/h",
        ("volume_flow_rate", "gal/d") => "L/h",
        ("water", "CCF" | "ft³" | "MCF") => "m³",
        ("water", "gal") => "L",
        _ => {
            if device_class == "atmospheric_pressure"
                && unit != "hPa"
                && configurable_units(UnitSystemProperty::Pressure).contains(&unit)
            {
                "hPa"
            } else if device_class == "wind_speed"
                && unit != "km/h"
                && unit != "kn"
                && METRIC_WIND_SPEED_UNITS.contains(&unit)
            {
                "km/h"
            } else {
                return None;
            }
        }
    };
    Some(converted)
}

fn us_customary_converted_unit(
    device_class: Option<&str>,
    original_unit: Option<&str>,
) -> Option<&'static str> {
    let (Some(device_class), Some(unit)) = (device_class, original_unit) else {
        return None;
    };
    let converted = match (device_class, unit) {
        ("area", "m²") => "ft²",
        ("area", "cm²" | "mm²") => "in²",
        ("area", "km²") => "mi²",
        ("area", "ha") => "ac",
        ("distance", "cm" | "mm") => "in",
        ("distance", "km") => "mi",
        ("distance", "m") => "ft",
        ("gas", "m³" | "L") => "ft³",
        ("precipitation", "cm" | "mm") => "in",
        ("precipitation_intensity", "mm/d") => "in/d",
        ("precipitation_intensity", "mm/h") => "in/h",
        ("pressure", "mbar" | "cbar" | "bar" | "mPa" | "Pa" | "hPa" | "kPa" | "inH₂O" | "atm") => {
            "psi"
        }
        ("pressure", "mmHg") => "inHg",
        ("radon", "Bq/m³") => "pCi/L",
        ("speed", "m/min" | "mm/s") => "in/s",
        ("speed", "m/s" | "km/h") => "mph",
        ("speed", "mm/d") => "in/d",
        ("speed", "mm/h") => "in/h",
        ("volume", "m³") => "ft³",
        ("volume", "L") => "gal",
        ("volume", "mL") => "fl. oz.",
        ("volume_flow_rate", "m³/min" | "m³/h" | "m³/s") => "ft³/min",
        ("volume_flow_rate", "L/min" | "L/s" | "mL/s") => "gal/min",
        ("volume_flow_rate", "L/h") => "gal/h",
        ("water", "m³") => "ft³",
        ("water", "L") => "gal",
        _ => {
            if device_class == "atmospheric_pressure"
                && unit != "inHg"
                && configurable_units(UnitSystemProperty::Pressure).contains(&unit)
            {
                "inHg"
            } else if device_class == "wind_speed"
                && unit != "mph"
                && unit != "kn"
                && US_CUSTOMARY_WIND_SPEED_UNITS.contains(&unit)
            {
                "mph"
            } else {
                return None;
            }
        }
    };
    Some(converted)
}
