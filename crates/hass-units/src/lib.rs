//! Native unit conversion behavior transcribed from Home Assistant.
//!
//! Source provenance and the family-by-family mapping are in docs/UNITS-PORT.md.

use std::error::Error;
use std::fmt;

/// A pinned converter family from Home Assistant's unit_conversion module.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnitFamily {
    ApparentPower,
    Area,
    BloodGlucoseConcentration,
    CarbonMonoxideConcentration,
    Conductivity,
    DataRate,
    Distance,
    Duration,
    ElectricCurrent,
    Frequency,
    ElectricPotential,
    Energy,
    EnergyDistance,
    Information,
    Mass,
    MassVolumeConcentration,
    NitrogenDioxideConcentration,
    NitrogenMonoxideConcentration,
    OzoneConcentration,
    Power,
    Pressure,
    RadiationConcentration,
    ReactiveEnergy,
    ReactivePower,
    Speed,
    SulphurDioxideConcentration,
    Temperature,
    TemperatureDelta,
    UnitlessRatio,
    Volume,
    VolumeFlowRate,
}

mod unit_system;

pub use unit_system::{
    AreaUnit, IMPERIAL_SYSTEM, InvalidUnitSelection, LengthUnit, METRIC_SYSTEM, METRIC_UNITS,
    MassUnit, PressureUnit, TemperatureUnit, US_CUSTOMARY_SYSTEM, US_CUSTOMARY_UNITS, UnitSystem,
    UnitSystemConversion, UnitSystemError, UnitSystemKind, UnitSystemProperty, UnitSystemUnitNames,
    UnitSystemUnits, VolumeUnit, WindSpeedUnit, configurable_units, get_unit_system,
    get_unit_system_optional, validate_unit_system,
};

/// The side of a conversion pair that contains an unsupported unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnitSide {
    From,
    To,
}

/// An error that preserves the source converter's invalid-unit distinctions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversionError {
    /// An identifier was not present in the pinned conversion table.
    ///
    /// The unit field identifies the invalid side. reported_unit retains the
    /// exact unit string used in Home Assistant's original error text. Those
    /// can differ for SpeedConverter, which always reports its target.
    UnrecognizedUnit {
        family: UnitFamily,
        side: UnitSide,
        unit: Option<String>,
        reported_unit: String,
    },
    /// A reciprocal conversion divided by a zero denominator.
    DivisionByZero { family: UnitFamily },
    /// A finite negative Beaufort value yields a complex result in the Python source.
    NonRealResult { family: UnitFamily },
    /// Python integer rounding rejects a NaN result.
    CannotRoundNaN { family: UnitFamily },
    /// Python integer rounding rejects an infinite result.
    CannotRoundInfinity { family: UnitFamily },
    /// A finite exponentiation overflowed before later scaling or rounding.
    ArithmeticOverflow { family: UnitFamily },
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecognizedUnit {
                family,
                reported_unit,
                ..
            } => write!(
                f,
                "{} is not a recognized {} unit.",
                reported_unit,
                family.unit_class()
            ),
            Self::DivisionByZero { family } => {
                write!(
                    f,
                    "division by zero during {} unit conversion",
                    family.unit_class()
                )
            }
            Self::NonRealResult { family } => {
                write!(
                    f,
                    "conversion produced a non-real value for {}",
                    family.unit_class()
                )
            }
            Self::CannotRoundNaN { .. } => f.write_str("cannot convert float NaN to integer"),
            Self::CannotRoundInfinity { .. } => {
                f.write_str("cannot convert float infinity to integer")
            }
            Self::ArithmeticOverflow { family } => write!(
                f,
                "numeric overflow during {} unit conversion",
                family.unit_class()
            ),
        }
    }
}

impl Error for ConversionError {}

impl UnitFamily {
    /// The unit class label used by Home Assistant's invalid-unit message.
    pub const fn unit_class(self) -> &'static str {
        match self {
            Self::ApparentPower => "apparent_power",
            Self::Area => "area",
            Self::BloodGlucoseConcentration => "blood_glucose_concentration",
            Self::CarbonMonoxideConcentration => "carbon_monoxide",
            Self::Conductivity => "conductivity",
            Self::DataRate => "data_rate",
            Self::Distance => "distance",
            Self::Duration => "duration",
            Self::ElectricCurrent => "electric_current",
            Self::Frequency => "frequency",
            Self::ElectricPotential => "voltage",
            Self::Energy => "energy",
            Self::EnergyDistance => "energy_distance",
            Self::Information => "information",
            Self::Mass => "mass",
            Self::MassVolumeConcentration => "concentration",
            Self::NitrogenDioxideConcentration => "nitrogen_dioxide",
            Self::NitrogenMonoxideConcentration => "nitrogen_monoxide",
            Self::OzoneConcentration => "ozone",
            Self::Power => "power",
            Self::Pressure => "pressure",
            Self::RadiationConcentration => "radiation_concentration",
            Self::ReactiveEnergy => "reactive_energy",
            Self::ReactivePower => "reactive_power",
            Self::Speed => "speed",
            Self::SulphurDioxideConcentration => "sulphur_dioxide",
            Self::Temperature => "temperature",
            Self::TemperatureDelta => "temperature_delta",
            Self::UnitlessRatio => "unitless",
            Self::Volume => "volume",
            Self::VolumeFlowRate => "volume_flow_rate",
        }
    }
}

/// Return the pinned source converter's supported unit identifiers.
///
/// None is a supported unit only for unitless ratios. Conversion separately
/// preserves the source same-unit shortcut, which accepts equal unknown values
/// (including None/None) before checking this list.
pub fn supported_units(family: UnitFamily) -> &'static [Option<&'static str>] {
    use UnitFamily as F;
    match family {
        F::ApparentPower => &[Some("mVA"), Some("VA"), Some("kVA")],
        F::Area => &[
            Some("m²"),
            Some("cm²"),
            Some("mm²"),
            Some("km²"),
            Some("in²"),
            Some("ft²"),
            Some("yd²"),
            Some("mi²"),
            Some("ac"),
            Some("ha"),
        ],
        F::BloodGlucoseConcentration => &[Some("mg/dL"), Some("mmol/L")],
        F::CarbonMonoxideConcentration => &[Some("ppb"), Some("ppm"), Some("mg/m³"), Some("μg/m³")],
        F::Conductivity => &[Some("μS/cm"), Some("mS/cm"), Some("S/cm")],
        F::DataRate => &[
            Some("bit/s"),
            Some("kbit/s"),
            Some("Mbit/s"),
            Some("Gbit/s"),
            Some("B/s"),
            Some("kB/s"),
            Some("MB/s"),
            Some("GB/s"),
            Some("KiB/s"),
            Some("MiB/s"),
            Some("GiB/s"),
        ],
        F::Distance => &[
            Some("m"),
            Some("mm"),
            Some("cm"),
            Some("km"),
            Some("in"),
            Some("ft"),
            Some("yd"),
            Some("mi"),
            Some("nmi"),
        ],
        F::Duration => &[
            Some("μs"),
            Some("ms"),
            Some("s"),
            Some("min"),
            Some("h"),
            Some("d"),
            Some("w"),
        ],
        F::ElectricCurrent => &[Some("A"), Some("mA"), Some("μA")],
        F::Frequency => &[
            Some("mHz"),
            Some("Hz"),
            Some("kHz"),
            Some("MHz"),
            Some("GHz"),
        ],
        F::ElectricPotential => &[Some("V"), Some("mV"), Some("μV"), Some("kV"), Some("MV")],
        F::Energy => &[
            Some("J"),
            Some("kJ"),
            Some("MJ"),
            Some("GJ"),
            Some("mWh"),
            Some("Wh"),
            Some("kWh"),
            Some("MWh"),
            Some("GWh"),
            Some("TWh"),
            Some("cal"),
            Some("kcal"),
            Some("Mcal"),
            Some("Gcal"),
            Some("thm"),
        ],
        F::EnergyDistance => &[
            Some("kWh/100km"),
            Some("Wh/km"),
            Some("mi/kWh"),
            Some("km/kWh"),
        ],
        F::Information => &[
            Some("bit"),
            Some("kbit"),
            Some("Mbit"),
            Some("Gbit"),
            Some("B"),
            Some("kB"),
            Some("MB"),
            Some("GB"),
            Some("TB"),
            Some("PB"),
            Some("EB"),
            Some("ZB"),
            Some("YB"),
            Some("KiB"),
            Some("MiB"),
            Some("GiB"),
            Some("TiB"),
            Some("PiB"),
            Some("EiB"),
            Some("ZiB"),
            Some("YiB"),
        ],
        F::Mass => &[
            Some("μg"),
            Some("mg"),
            Some("g"),
            Some("kg"),
            Some("oz"),
            Some("lb"),
            Some("st"),
        ],
        F::MassVolumeConcentration => &[Some("μg/m³"), Some("mg/m³"), Some("g/m³")],
        F::NitrogenDioxideConcentration => &[Some("ppb"), Some("ppm"), Some("μg/m³")],
        F::NitrogenMonoxideConcentration => &[Some("ppb"), Some("μg/m³")],
        F::OzoneConcentration => &[Some("ppb"), Some("ppm"), Some("μg/m³")],
        F::Power => &[
            Some("mW"),
            Some("W"),
            Some("kW"),
            Some("MW"),
            Some("GW"),
            Some("TW"),
            Some("BTU/h"),
        ],
        F::Pressure => &[
            Some("mPa"),
            Some("Pa"),
            Some("hPa"),
            Some("kPa"),
            Some("bar"),
            Some("cbar"),
            Some("mbar"),
            Some("inHg"),
            Some("inH₂O"),
            Some("psi"),
            Some("mmHg"),
            Some("atm"),
        ],
        F::RadiationConcentration => &[Some("Bq/m³"), Some("pCi/L")],
        F::ReactiveEnergy => &[Some("varh"), Some("kvarh")],
        F::ReactivePower => &[Some("mvar"), Some("var"), Some("kvar")],
        F::Speed => &[
            Some("in/d"),
            Some("in/h"),
            Some("mm/d"),
            Some("mm/h"),
            Some("ft/s"),
            Some("in/s"),
            Some("km/h"),
            Some("kn"),
            Some("m/min"),
            Some("m/s"),
            Some("mm/s"),
            Some("mph"),
            Some("Beaufort"),
        ],
        F::SulphurDioxideConcentration => &[Some("ppb"), Some("μg/m³")],
        F::Temperature | F::TemperatureDelta => &[Some("°C"), Some("°F"), Some("K")],
        F::UnitlessRatio => &[None, Some("ppb"), Some("ppm"), Some("%")],
        F::Volume => &[
            Some("L"),
            Some("mL"),
            Some("gal"),
            Some("fl. oz."),
            Some("m³"),
            Some("ft³"),
            Some("CCF"),
            Some("MCF"),
        ],
        F::VolumeFlowRate => &[
            Some("m³/h"),
            Some("m³/min"),
            Some("m³/s"),
            Some("ft³/min"),
            Some("L/h"),
            Some("L/min"),
            Some("L/s"),
            Some("gal/h"),
            Some("gal/min"),
            Some("gal/d"),
            Some("mL/s"),
        ],
    }
}

/// Test a unit identifier against a converter family's pinned valid-unit set.
pub fn is_valid_unit(family: UnitFamily, unit: Option<&str>) -> bool {
    supported_units(family).contains(&unit)
}

/// Convert an f64 between units in one pinned family.
///
/// Equal source and target identifiers return the input unchanged before unit
/// validation. For other pairs, invalid source and target units produce
/// distinct errors. None is a unit identifier only for unitless ratios.
pub fn convert(
    family: UnitFamily,
    value: f64,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<f64, ConversionError> {
    apply_plan(plan(family, from, to)?, value, false)?
        .ok_or(ConversionError::DivisionByZero { family })
}

/// Convert an optional value using the source allow-None converter behavior.
///
/// Unit validation still happens when the value is None. Reciprocal
/// energy-distance conversions return None for a zero denominator, matching
/// Home Assistant's allow-None converter; convert reports DivisionByZero for
/// that same reciprocal conversion.
pub fn convert_optional(
    family: UnitFamily,
    value: Option<f64>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Option<f64>, ConversionError> {
    let conversion = plan(family, from, to)?;
    let Some(value) = value else {
        return Ok(None);
    };
    apply_plan(conversion, value, true)
}

#[derive(Clone, Copy, Debug)]
enum Plan {
    Identity,
    Linear {
        from_ratio: f64,
        to_ratio: f64,
        inverse: bool,
    },
    Temperature(TemperatureConversion),
    BeaufortFrom {
        to_ratio: f64,
    },
    BeaufortTo {
        from_ratio: f64,
    },
}

#[derive(Clone, Copy, Debug)]
enum TemperatureConversion {
    CelsiusToFahrenheit,
    CelsiusToKelvin,
    FahrenheitToCelsius,
    FahrenheitToKelvin,
    KelvinToCelsius,
    KelvinToFahrenheit,
}

fn plan(family: UnitFamily, from: Option<&str>, to: Option<&str>) -> Result<Plan, ConversionError> {
    if from == to {
        return Ok(Plan::Identity);
    }
    if family == UnitFamily::Temperature {
        return temperature_plan(from, to);
    }
    if family == UnitFamily::Speed {
        let from_valid = is_valid_unit(family, from);
        let to_valid = is_valid_unit(family, to);
        if !from_valid || !to_valid {
            let (side, unit) = if !from_valid {
                (UnitSide::From, from)
            } else {
                (UnitSide::To, to)
            };
            return Err(unrecognized_unit(family, side, unit, to));
        }
        if from == Some("Beaufort") {
            let to_ratio = factor(family, to).expect("validated speed unit");
            return Ok(Plan::BeaufortFrom { to_ratio });
        }
        if to == Some("Beaufort") {
            let from_ratio = factor(family, from).expect("validated speed unit");
            return Ok(Plan::BeaufortTo { from_ratio });
        }
    }

    let from_ratio = factor(family, from)
        .ok_or_else(|| unrecognized_unit(family, UnitSide::From, from, from))?;
    let to_ratio =
        factor(family, to).ok_or_else(|| unrecognized_unit(family, UnitSide::To, to, to))?;
    let inverse = family == UnitFamily::EnergyDistance
        && is_inverse_energy_distance(from) != is_inverse_energy_distance(to);
    Ok(Plan::Linear {
        from_ratio,
        to_ratio,
        inverse,
    })
}

fn temperature_plan(from: Option<&str>, to: Option<&str>) -> Result<Plan, ConversionError> {
    let conversion = match from {
        Some("°C") => match to {
            Some("°F") => TemperatureConversion::CelsiusToFahrenheit,
            Some("K") => TemperatureConversion::CelsiusToKelvin,
            _ => {
                return Err(unrecognized_unit(
                    UnitFamily::Temperature,
                    UnitSide::To,
                    to,
                    to,
                ));
            }
        },
        Some("°F") => match to {
            Some("°C") => TemperatureConversion::FahrenheitToCelsius,
            Some("K") => TemperatureConversion::FahrenheitToKelvin,
            _ => {
                return Err(unrecognized_unit(
                    UnitFamily::Temperature,
                    UnitSide::To,
                    to,
                    to,
                ));
            }
        },
        Some("K") => match to {
            Some("°C") => TemperatureConversion::KelvinToCelsius,
            Some("°F") => TemperatureConversion::KelvinToFahrenheit,
            _ => {
                return Err(unrecognized_unit(
                    UnitFamily::Temperature,
                    UnitSide::To,
                    to,
                    to,
                ));
            }
        },
        _ => {
            return Err(unrecognized_unit(
                UnitFamily::Temperature,
                UnitSide::From,
                from,
                from,
            ));
        }
    };
    Ok(Plan::Temperature(conversion))
}

fn unrecognized_unit(
    family: UnitFamily,
    side: UnitSide,
    unit: Option<&str>,
    reported_unit: Option<&str>,
) -> ConversionError {
    ConversionError::UnrecognizedUnit {
        family,
        side,
        unit: unit.map(str::to_owned),
        reported_unit: reported_unit.unwrap_or("None").to_owned(),
    }
}

fn apply_plan(plan: Plan, value: f64, allow_none: bool) -> Result<Option<f64>, ConversionError> {
    let result = match plan {
        Plan::Identity => value,
        Plan::Linear {
            from_ratio,
            to_ratio,
            inverse: false,
        } => value / from_ratio * to_ratio,
        Plan::Linear {
            from_ratio,
            to_ratio,
            inverse: true,
        } => {
            let denominator = value / from_ratio;
            if denominator == 0.0 {
                if allow_none && value == 0.0 {
                    return Ok(None);
                }
                return Err(ConversionError::DivisionByZero {
                    family: UnitFamily::EnergyDistance,
                });
            }
            to_ratio / denominator
        }
        Plan::Temperature(conversion) => match conversion {
            TemperatureConversion::CelsiusToFahrenheit => value * 1.8 + 32.0,
            TemperatureConversion::CelsiusToKelvin => value + 273.15,
            TemperatureConversion::FahrenheitToCelsius => (value - 32.0) / 1.8,
            TemperatureConversion::FahrenheitToKelvin => 273.15 + (value - 32.0) / 1.8,
            TemperatureConversion::KelvinToCelsius => value - 273.15,
            TemperatureConversion::KelvinToFahrenheit => (value - 273.15) * 1.8 + 32.0,
        },
        Plan::BeaufortFrom { to_ratio } => {
            if value.is_finite() && value < 0.0 {
                return Err(ConversionError::NonRealResult {
                    family: UnitFamily::Speed,
                });
            }
            let normalized = beaufort_to_meters_per_second(value);
            if value.is_finite() && normalized.is_infinite() {
                return Err(ConversionError::ArithmeticOverflow {
                    family: UnitFamily::Speed,
                });
            }
            normalized * to_ratio
        }
        Plan::BeaufortTo { from_ratio } => meters_per_second_to_beaufort(value / from_ratio)?,
    };
    Ok(Some(result))
}

fn meters_per_second_to_beaufort(meters_per_second: f64) -> Result<f64, ConversionError> {
    let speed_ratio = meters_per_second / 0.836;
    let squared = speed_ratio.powf(2.0);
    if speed_ratio.is_finite() && squared.is_infinite() {
        return Err(ConversionError::ArithmeticOverflow {
            family: UnitFamily::Speed,
        });
    }
    let unrounded = squared.powf(1.0 / 3.0);
    if unrounded.is_nan() {
        return Err(ConversionError::CannotRoundNaN {
            family: UnitFamily::Speed,
        });
    }
    if unrounded.is_infinite() {
        return Err(ConversionError::CannotRoundInfinity {
            family: UnitFamily::Speed,
        });
    }
    Ok(unrounded.round_ties_even())
}

fn beaufort_to_meters_per_second(beaufort: f64) -> f64 {
    0.836 * beaufort.powf(3.0 / 2.0)
}

fn is_inverse_energy_distance(unit: Option<&str>) -> bool {
    matches!(unit, Some("mi/kWh" | "km/kWh"))
}

fn factor(family: UnitFamily, unit: Option<&str>) -> Option<f64> {
    use UnitFamily as F;
    let value = match (family, unit) {
        (F::ApparentPower, Some("mVA")) => 1.0 * 1000.0,
        (F::ApparentPower, Some("VA")) => 1.0,
        (F::ApparentPower, Some("kVA")) => 1.0 / 1000.0,

        (F::Area, Some("m²")) => 1.0,
        (F::Area, Some("cm²")) => 1.0 / (0.01_f64.powf(2.0)),
        (F::Area, Some("mm²")) => 1.0 / (0.001_f64.powf(2.0)),
        (F::Area, Some("km²")) => 1.0 / (1000.0_f64.powf(2.0)),
        (F::Area, Some("in²")) => 1.0 / (0.0254_f64.powf(2.0)),
        (F::Area, Some("ft²")) => 1.0 / ((0.0254_f64 * 12.0).powf(2.0)),
        (F::Area, Some("yd²")) => 1.0 / ((0.0254_f64 * 12.0 * 3.0).powf(2.0)),
        (F::Area, Some("mi²")) => 1.0 / ((0.0254_f64 * 12.0 * 3.0 * 1760.0).powf(2.0)),
        (F::Area, Some("ac")) => {
            let foot_to_m = 0.0254_f64 * 12.0;
            1.0 / (66.0 * 660.0 * foot_to_m.powf(2.0))
        }
        (F::Area, Some("ha")) => 1.0 / (100.0 * 100.0),

        (F::BloodGlucoseConcentration, Some("mg/dL")) => 180.16 / 10.0,
        (F::BloodGlucoseConcentration, Some("mmol/L")) => 1.0,

        (F::CarbonMonoxideConcentration, Some("ppb")) => 1_000_000_000.0,
        (F::CarbonMonoxideConcentration, Some("ppm")) => 1_000_000.0,
        (F::CarbonMonoxideConcentration, Some("mg/m³")) => {
            gas_concentration_factor(28.01, 1_000.0)
        }
        (F::CarbonMonoxideConcentration, Some("μg/m³")) => {
            gas_concentration_factor(28.01, 1_000_000.0)
        }

        (F::Conductivity, Some("μS/cm")) => 1.0,
        (F::Conductivity, Some("mS/cm")) => 0.001,
        (F::Conductivity, Some("S/cm")) => 0.000001,

        (F::DataRate, Some("bit/s")) => 1.0,
        (F::DataRate, Some("kbit/s")) => 1.0 / 1000.0,
        (F::DataRate, Some("Mbit/s")) => 1.0 / 1_000_000.0,
        (F::DataRate, Some("Gbit/s")) => 1.0 / 1_000_000_000.0,
        (F::DataRate, Some("B/s")) => 1.0 / 8.0,
        (F::DataRate, Some("kB/s")) => 1.0 / 8_000.0,
        (F::DataRate, Some("MB/s")) => 1.0 / 8_000_000.0,
        (F::DataRate, Some("GB/s")) => 1.0 / 8_000_000_000.0,
        (F::DataRate, Some("KiB/s")) => 1.0 / 2.0_f64.powf(13.0),
        (F::DataRate, Some("MiB/s")) => 1.0 / 2.0_f64.powf(23.0),
        (F::DataRate, Some("GiB/s")) => 1.0 / 2.0_f64.powf(33.0),

        (F::Distance, Some("m")) => 1.0,
        (F::Distance, Some("mm")) => 1.0 / 0.001,
        (F::Distance, Some("cm")) => 1.0 / 0.01,
        (F::Distance, Some("km")) => 1.0 / 1000.0,
        (F::Distance, Some("in")) => 1.0 / 0.0254,
        (F::Distance, Some("ft")) => 1.0 / (0.0254 * 12.0),
        (F::Distance, Some("yd")) => 1.0 / (0.0254 * 12.0 * 3.0),
        (F::Distance, Some("mi")) => 1.0 / (0.0254 * 12.0 * 3.0 * 1760.0),
        (F::Distance, Some("nmi")) => 1.0 / 1852.0,

        (F::Duration, Some("μs")) => 1.0 * 1_000_000.0,
        (F::Duration, Some("ms")) => 1.0 * 1000.0,
        (F::Duration, Some("s")) => 1.0,
        (F::Duration, Some("min")) => 1.0 / 60.0,
        (F::Duration, Some("h")) => 1.0 / (60.0 * 60.0),
        (F::Duration, Some("d")) => 1.0 / (24.0 * 60.0 * 60.0),
        (F::Duration, Some("w")) => 1.0 / (7.0 * 24.0 * 60.0 * 60.0),

        (F::ElectricCurrent, Some("A")) => 1.0,
        (F::ElectricCurrent, Some("mA")) => 1000.0,
        (F::ElectricCurrent, Some("μA")) => 1_000_000.0,

        (F::Frequency, Some("mHz")) => 1000.0,
        (F::Frequency, Some("Hz")) => 1.0,
        (F::Frequency, Some("kHz")) => 1.0 / 1000.0,
        (F::Frequency, Some("MHz")) => 1.0 / 1_000_000.0,
        (F::Frequency, Some("GHz")) => 1.0 / 1_000_000_000.0,

        (F::ElectricPotential, Some("V")) => 1.0,
        (F::ElectricPotential, Some("mV")) => 1000.0,
        (F::ElectricPotential, Some("μV")) => 1_000_000.0,
        (F::ElectricPotential, Some("kV")) => 1.0 / 1000.0,
        (F::ElectricPotential, Some("MV")) => 1.0 / 1_000_000.0,

        (F::Energy, Some("J")) => 3600.0 * 1000.0,
        (F::Energy, Some("kJ")) => 3600.0,
        (F::Energy, Some("MJ")) => 3600.0 / 1000.0,
        (F::Energy, Some("GJ")) => 3600.0 / 1_000_000.0,
        (F::Energy, Some("mWh")) => 1_000_000.0,
        (F::Energy, Some("Wh")) => 1000.0,
        (F::Energy, Some("kWh")) => 1.0,
        (F::Energy, Some("MWh")) => 1.0 / 1000.0,
        (F::Energy, Some("GWh")) => 1.0 / 1_000_000.0,
        (F::Energy, Some("TWh")) => 1.0 / 1_000_000_000.0,
        (F::Energy, Some("cal")) => (3600.0 / 4.184) * 1000.0,
        (F::Energy, Some("kcal")) => 3600.0 / 4.184,
        (F::Energy, Some("Mcal")) => (3600.0 / 4.184) / 1000.0,
        (F::Energy, Some("Gcal")) => (3600.0 / 4.184) / 1_000_000.0,
        (F::Energy, Some("thm")) => (3600.0 / 105_480_400.0) * 1000.0,

        (F::EnergyDistance, Some("kWh/100km")) => 1.0,
        (F::EnergyDistance, Some("Wh/km")) => 10.0,
        (F::EnergyDistance, Some("mi/kWh")) => 100.0 * 1000.0 / (0.0254 * 12.0 * 3.0 * 1760.0),
        (F::EnergyDistance, Some("km/kWh")) => 100.0,

        (F::Information, Some("bit")) => 1.0,
        (F::Information, Some("kbit")) => 1.0 / 1000.0,
        (F::Information, Some("Mbit")) => 1.0 / 1_000_000.0,
        (F::Information, Some("Gbit")) => 1.0 / 1_000_000_000.0,
        (F::Information, Some("B")) => 1.0 / 8.0,
        (F::Information, Some("kB")) => 1.0 / 8_000.0,
        (F::Information, Some("MB")) => 1.0 / 8_000_000.0,
        (F::Information, Some("GB")) => 1.0 / 8_000_000_000.0,
        (F::Information, Some("TB")) => 1.0 / 8_000_000_000_000.0,
        (F::Information, Some("PB")) => 1.0 / 8_000_000_000_000_000.0,
        (F::Information, Some("EB")) => 1.0 / 8.0e18,
        (F::Information, Some("ZB")) => 1.0 / 8.0e21,
        (F::Information, Some("YB")) => 1.0 / 8.0e24,
        (F::Information, Some("KiB")) => 1.0 / 2.0_f64.powf(13.0),
        (F::Information, Some("MiB")) => 1.0 / 2.0_f64.powf(23.0),
        (F::Information, Some("GiB")) => 1.0 / 2.0_f64.powf(33.0),
        (F::Information, Some("TiB")) => 1.0 / 2.0_f64.powf(43.0),
        (F::Information, Some("PiB")) => 1.0 / 2.0_f64.powf(53.0),
        (F::Information, Some("EiB")) => 1.0 / 2.0_f64.powf(63.0),
        (F::Information, Some("ZiB")) => 1.0 / 2.0_f64.powf(73.0),
        (F::Information, Some("YiB")) => 1.0 / 2.0_f64.powf(83.0),

        (F::Mass, Some("μg")) => 1.0 * 1000.0 * 1000.0,
        (F::Mass, Some("mg")) => 1.0 * 1000.0,
        (F::Mass, Some("g")) => 1.0,
        (F::Mass, Some("kg")) => 1.0 / 1000.0,
        (F::Mass, Some("oz")) => 1.0 / (453.59237 / 16.0),
        (F::Mass, Some("lb")) => 1.0 / 453.59237,
        (F::Mass, Some("st")) => 1.0 / (453.59237 * 14.0),

        (F::MassVolumeConcentration, Some("μg/m³")) => 1_000_000.0,
        (F::MassVolumeConcentration, Some("mg/m³")) => 1000.0,
        (F::MassVolumeConcentration, Some("g/m³")) => 1.0,

        (F::NitrogenDioxideConcentration, Some("ppb")) => 1_000_000_000.0,
        (F::NitrogenDioxideConcentration, Some("ppm")) => 1_000_000.0,
        (F::NitrogenDioxideConcentration, Some("μg/m³")) => {
            gas_concentration_factor(46.0055, 1_000_000.0)
        }
        (F::NitrogenMonoxideConcentration, Some("ppb")) => 1_000_000_000.0,
        (F::NitrogenMonoxideConcentration, Some("μg/m³")) => {
            gas_concentration_factor(30.0061, 1_000_000.0)
        }
        (F::OzoneConcentration, Some("ppb")) => 1_000_000_000.0,
        (F::OzoneConcentration, Some("ppm")) => 1_000_000.0,
        (F::OzoneConcentration, Some("μg/m³")) => gas_concentration_factor(48.0, 1_000_000.0),

        (F::Power, Some("mW")) => 1.0 * 1000.0,
        (F::Power, Some("W")) => 1.0,
        (F::Power, Some("kW")) => 1.0 / 1000.0,
        (F::Power, Some("MW")) => 1.0 / 1_000_000.0,
        (F::Power, Some("GW")) => 1.0 / 1_000_000_000.0,
        (F::Power, Some("TW")) => 1.0 / 1_000_000_000_000.0,
        (F::Power, Some("BTU/h")) => 1.0 / 0.29307107,

        (F::Pressure, Some("mPa")) => 1.0 * 1000.0,
        (F::Pressure, Some("Pa")) => 1.0,
        (F::Pressure, Some("hPa")) => 1.0 / 100.0,
        (F::Pressure, Some("kPa")) => 1.0 / 1000.0,
        (F::Pressure, Some("bar")) => 1.0 / 100_000.0,
        (F::Pressure, Some("cbar")) => 1.0 / 1000.0,
        (F::Pressure, Some("mbar")) => 1.0 / 100.0,
        (F::Pressure, Some("inHg")) => 1.0 / (0.0254 * 1000.0 * 9.80665 * 13.5951),
        (F::Pressure, Some("inH₂O")) => 1.0 / 249.0889083333348,
        (F::Pressure, Some("psi")) => 1.0 / 6894.757,
        (F::Pressure, Some("mmHg")) => 1.0 / (0.001 * 1000.0 * 9.80665 * 13.5951),
        (F::Pressure, Some("atm")) => 1.0 / 101_325.0,

        (F::RadiationConcentration, Some("Bq/m³")) => 1.0,
        (F::RadiationConcentration, Some("pCi/L")) => 1.0 / 37.0,
        (F::ReactiveEnergy, Some("varh")) => 1.0,
        (F::ReactiveEnergy, Some("kvarh")) => 1.0 / 1000.0,
        (F::ReactivePower, Some("mvar")) => 1.0 * 1000.0,
        (F::ReactivePower, Some("var")) => 1.0,
        (F::ReactivePower, Some("kvar")) => 1.0 / 1000.0,

        (F::Speed, Some("in/d")) => (24.0 * 60.0 * 60.0) / 0.0254,
        (F::Speed, Some("in/h")) => (60.0 * 60.0) / 0.0254,
        (F::Speed, Some("mm/d")) => (24.0 * 60.0 * 60.0) / 0.001,
        (F::Speed, Some("mm/h")) => (60.0 * 60.0) / 0.001,
        (F::Speed, Some("ft/s")) => 1.0 / (0.0254 * 12.0),
        (F::Speed, Some("in/s")) => 1.0 / 0.0254,
        (F::Speed, Some("km/h")) => (60.0 * 60.0) / 1000.0,
        (F::Speed, Some("kn")) => (60.0 * 60.0) / 1852.0,
        (F::Speed, Some("m/min")) => 60.0,
        (F::Speed, Some("m/s" | "Beaufort")) => 1.0,
        (F::Speed, Some("mm/s")) => 1.0 / 0.001,
        (F::Speed, Some("mph")) => (60.0 * 60.0) / (0.0254 * 12.0 * 3.0 * 1760.0),

        (F::SulphurDioxideConcentration, Some("ppb")) => 1_000_000_000.0,
        (F::SulphurDioxideConcentration, Some("μg/m³")) => {
            gas_concentration_factor(64.066, 1_000_000.0)
        }

        (F::Temperature | F::TemperatureDelta, Some("°C" | "K")) => 1.0,
        (F::Temperature | F::TemperatureDelta, Some("°F")) => 1.8,
        (F::UnitlessRatio, None) => 1.0,
        (F::UnitlessRatio, Some("ppb")) => 1_000_000_000.0,
        (F::UnitlessRatio, Some("ppm")) => 1_000_000.0,
        (F::UnitlessRatio, Some("%")) => 100.0,

        (F::Volume, Some("L")) => 1.0 / 0.001,
        (F::Volume, Some("mL")) => 1.0 / (0.001 * 0.001),
        (F::Volume, Some("gal")) => {
            let inch_to_m = 0.0254_f64;
            1.0 / (231.0 * inch_to_m.powf(3.0))
        }
        (F::Volume, Some("fl. oz.")) => {
            let inch_to_m = 0.0254_f64;
            let gallon = 231.0 * inch_to_m.powf(3.0);
            1.0 / (gallon / 128.0)
        }
        (F::Volume, Some("m³")) => 1.0,
        (F::Volume, Some("ft³")) => {
            let foot_to_m = 0.0254_f64 * 12.0;
            1.0 / foot_to_m.powf(3.0)
        }
        (F::Volume, Some("CCF")) => {
            let foot_to_m = 0.0254_f64 * 12.0;
            1.0 / (100.0 * foot_to_m.powf(3.0))
        }
        (F::Volume, Some("MCF")) => {
            let foot_to_m = 0.0254_f64 * 12.0;
            1.0 / (1000.0 * foot_to_m.powf(3.0))
        }

        (F::VolumeFlowRate, Some("m³/h")) => 1.0,
        (F::VolumeFlowRate, Some("m³/min")) => 1.0 / 60.0,
        (F::VolumeFlowRate, Some("m³/s")) => 1.0 / (60.0 * 60.0),
        (F::VolumeFlowRate, Some("ft³/min")) => {
            let foot_to_m = 0.0254_f64 * 12.0;
            1.0 / (60.0 * foot_to_m.powf(3.0))
        }
        (F::VolumeFlowRate, Some("L/h")) => 1.0 / 0.001,
        (F::VolumeFlowRate, Some("L/min")) => 1.0 / (60.0 * 0.001),
        (F::VolumeFlowRate, Some("L/s")) => 1.0 / (60.0 * 60.0 * 0.001),
        (F::VolumeFlowRate, Some("gal/h")) => {
            let inch_to_m = 0.0254_f64;
            1.0 / (231.0 * inch_to_m.powf(3.0))
        }
        (F::VolumeFlowRate, Some("gal/min")) => {
            let inch_to_m = 0.0254_f64;
            1.0 / (60.0 * 231.0 * inch_to_m.powf(3.0))
        }
        (F::VolumeFlowRate, Some("gal/d")) => {
            let inch_to_m = 0.0254_f64;
            24.0 / (231.0 * inch_to_m.powf(3.0))
        }
        (F::VolumeFlowRate, Some("mL/s")) => 1.0 / (60.0 * 60.0 * 0.001 * 0.001),

        _ => return None,
    };
    Some(value)
}

fn gas_concentration_factor(molar_mass: f64, multiplier: f64) -> f64 {
    let ideal_gas_constant = 8.31446261815324;
    let ambient_temperature = 293.15;
    let ambient_pressure = 101_325.0;
    let ambient_molar_volume = ideal_gas_constant * ambient_temperature / ambient_pressure;
    molar_mass / ambient_molar_volume * multiplier
}
