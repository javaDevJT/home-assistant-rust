use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate};
use hass_core::{MemoryStore, Runtime};
use hass_entity::{EntityLifecycle, EntitySpec, EntityStatus};
use hass_registry::{EntityUpdate, Registries};
use hass_sensor::{
    SensorDescriptor, SensorDeviceClass, SensorErrorKind, SensorProjectionOptions, SensorReading,
    SensorStateClass, SensorUnit, SensorValue, SensorWarning,
};
use hass_units::{ConversionError, UnitFamily, UnitSide};
use serde_json::json;

fn spec(provider_id: &str, provider_key: &str) -> EntitySpec {
    EntitySpec::new("sensor", "fixture", provider_id, provider_key)
        .with_unique_id(format!("{provider_id}-{provider_key}"))
}

fn descriptor(
    provider_id: &str,
    provider_key: &str,
    device_class: Option<SensorDeviceClass>,
    state_class: Option<SensorStateClass>,
    native_unit: Option<&str>,
) -> SensorDescriptor {
    let mut descriptor = SensorDescriptor::new(spec(provider_id, provider_key)).unwrap();
    if let Some(device_class) = device_class {
        descriptor = descriptor.with_device_class(device_class);
    }
    if let Some(state_class) = state_class {
        descriptor = descriptor.with_state_class(state_class);
    }
    if let Some(unit) = native_unit {
        descriptor = descriptor.with_native_unit(unit);
    }
    descriptor
}

fn options(unit: &str) -> SensorProjectionOptions {
    SensorProjectionOptions {
        user_unit: Some(SensorUnit::from(unit)),
        temperature_unit: None,
    }
}

fn datetime(value: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(value).unwrap()
}

#[test]
fn text_date_timestamp_unknown_and_unavailable_remain_distinct() {
    let text = descriptor("text", "status", None, None, None)
        .project(SensorReading::new("ready"))
        .unwrap();
    assert_eq!(text.snapshot.state.as_deref(), Some("ready"));
    assert!(text.snapshot.available);

    let date = descriptor(
        "calendar",
        "date",
        Some(SensorDeviceClass::Date),
        None,
        None,
    )
    .project(SensorReading::new(SensorValue::Date(
        NaiveDate::from_ymd_opt(2026, 11, 7).unwrap(),
    )))
    .unwrap();
    assert_eq!(date.snapshot.state.as_deref(), Some("2026-11-07"));

    let timestamp = descriptor(
        "clock",
        "timestamp",
        Some(SensorDeviceClass::Timestamp),
        None,
        None,
    )
    .project(SensorReading::new(SensorValue::DateTime(datetime(
        "2026-11-07T12:34:56-05:00",
    ))))
    .unwrap();
    assert_eq!(
        timestamp.snapshot.state.as_deref(),
        Some("2026-11-07T17:34:56+00:00")
    );
    assert!(matches!(
        descriptor(
            "clock",
            "naive",
            Some(SensorDeviceClass::Uptime),
            None,
            None,
        )
        .project(SensorReading::new(SensorValue::NaiveDateTime(
            NaiveDate::from_ymd_opt(2026, 11, 7)
                .unwrap()
                .and_hms_opt(12, 34, 56)
                .unwrap(),
        )))
        .unwrap_err()
        .kind,
        SensorErrorKind::InvalidTimestampValue {
            device_class: SensorDeviceClass::Uptime
        }
    ));

    let unknown = descriptor("text", "missing", None, None, None)
        .project(SensorReading::without_value())
        .unwrap();
    assert_eq!(unknown.snapshot.state, None);
    assert!(unknown.snapshot.available);

    let unavailable = descriptor("text", "offline", None, None, None)
        .project(SensorReading::new("stale").with_availability(false))
        .unwrap();
    assert_eq!(unavailable.snapshot.state, None);
    assert!(!unavailable.snapshot.available);
}

#[test]
fn pinned_enum_optional_options_and_unknown_numeric_validation_order() {
    let enumeration = descriptor("fixture", "enum", Some(SensorDeviceClass::Enum), None, None);
    assert_eq!(
        enumeration
            .project(SensorReading::new("running"))
            .unwrap()
            .snapshot
            .state
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        enumeration
            .clone()
            .with_options(Vec::<String>::new())
            .project(SensorReading::new("running"))
            .unwrap()
            .snapshot
            .state
            .as_deref(),
        Some("running")
    );
    assert!(
        enumeration
            .with_options(["running"])
            .project(SensorReading::new(1_i64))
            .is_err()
    );

    let unknown_temperature = descriptor(
        "fixture",
        "temperature",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("Celsius"),
    )
    .project(SensorReading::without_value())
    .unwrap();
    assert_eq!(unknown_temperature.snapshot.state, None);
    assert!(unknown_temperature.warnings.is_empty());
    let angle = descriptor(
        "fixture",
        "angle",
        None,
        Some(SensorStateClass::MeasurementAngle),
        None,
    );
    assert_eq!(
        angle
            .project(SensorReading::without_value())
            .unwrap()
            .snapshot
            .state,
        None
    );
    assert!(angle.project(SensorReading::new(1.0_f64)).is_err());

    let measurement = descriptor(
        "fixture",
        "measurement",
        None,
        Some(SensorStateClass::Measurement),
        None,
    );
    assert_eq!(
        measurement
            .project(SensorReading::new("1"))
            .unwrap()
            .snapshot
            .state
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        measurement
            .project(SensorReading::new(1.0_f64))
            .unwrap()
            .snapshot
            .state
            .as_deref(),
        Some("1.0")
    );
}

#[test]
fn integer_counters_and_booleans_keep_source_native_state_strings() {
    let measurement = descriptor(
        "fixture",
        "measurement",
        None,
        Some(SensorStateClass::Measurement),
        None,
    );
    assert_eq!(
        measurement
            .project(SensorReading::new(u64::MAX))
            .unwrap()
            .snapshot
            .state
            .as_deref(),
        Some("18446744073709551615")
    );
    for (input, expected) in [
        ("18446744073709551617", "18446744073709551617"),
        ("+00012", "12"),
        ("-000", "0"),
        ("1_000", "1000"),
    ] {
        assert_eq!(
            measurement
                .project(SensorReading::new(input))
                .unwrap()
                .snapshot
                .state
                .as_deref(),
            Some(expected)
        );
    }
    for input in ["1_", "_1", "1__0"] {
        assert!(measurement.project(SensorReading::new(input)).is_err());
    }
    for (value, expected) in [(true, "True"), (false, "False")] {
        assert_eq!(
            measurement
                .project(SensorReading::new(value))
                .unwrap()
                .snapshot
                .state
                .as_deref(),
            Some(expected)
        );
        assert_eq!(
            descriptor("fixture", "plain", None, None, None)
                .project(SensorReading::new(value))
                .unwrap()
                .snapshot
                .state
                .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn float_state_format_matches_builtin_python_golden_values() {
    // Binary64 bit patterns and expected strings were generated with built-in
    // Python str(float), without importing Home Assistant or its dependencies.
    let plain = descriptor("fixture", "float", None, None, None);
    for (bits, expected) in [
        (0x8000000000000000, "-0.0"),
        (0x0000000000000000, "0.0"),
        (0x3ff0000000000000, "1.0"),
        (0x3ee4f8b588e368f1, "1e-05"),
        (0x3f1a36e2eb1c432d, "0.0001"),
        (0x3e7ad7f29abcaf48, "1e-07"),
        (0x4341c37937e08000, "1e+16"),
        (0x430c6bf526340000, "1000000000000000.0"),
        (0x4415af1d78b58c40, "1e+20"),
        (0x444b1ae4d6e2ef50, "1e+21"),
        (0x3ee9e409302678ba, "1.2345678901234568e-05"),
        (0x0000000000000001, "5e-324"),
        (0x7fefffffffffffff, "1.7976931348623157e+308"),
        (0x7ff0000000000000, "inf"),
        (0xfff0000000000000, "-inf"),
        (0x7ff8000000000000, "nan"),
    ] {
        assert_eq!(
            plain
                .project(SensorReading::new(f64::from_bits(bits)))
                .unwrap()
                .snapshot
                .state
                .as_deref(),
            Some(expected),
            "bits={bits:x}"
        );
    }
}

#[test]
fn full_native_unit_rules_apply_to_projection_including_explicit_no_unit() {
    for (class, unit) in [
        (SensorDeviceClass::Humidity, Some("%")),
        (SensorDeviceClass::Ph, None),
        (SensorDeviceClass::Aqi, None),
        (SensorDeviceClass::PowerFactor, None),
        (SensorDeviceClass::PowerFactor, Some("%")),
        (SensorDeviceClass::Monetary, Some("USD")),
    ] {
        let projected = descriptor("fixture", "units", Some(class), None, unit)
            .project(SensorReading::new(1.0_f64))
            .unwrap();
        assert!(
            projected.warnings.is_empty(),
            "{class:?}/{unit:?}: {:?}",
            projected.warnings
        );
        assert_eq!(projected.snapshot.state.as_deref(), Some("1.0"));
    }
    let invalid = descriptor(
        "fixture",
        "humidity",
        Some(SensorDeviceClass::Humidity),
        None,
        Some("W"),
    )
    .project(SensorReading::new(1.0_f64))
    .unwrap();
    assert!(matches!(
        invalid.warnings.as_slice(),
        [SensorWarning::InvalidDeviceClassUnit {
            device_class: SensorDeviceClass::Humidity,
            ..
        }]
    ));
    assert_eq!(invalid.snapshot.state.as_deref(), Some("1.0"));
}

#[test]
fn native_unit_aliases_keep_raw_metadata_and_publish_compatible_spelling() {
    for (raw, expected) in [
        ("µSv/h", "μSv/h"),
        ("µS/cm", "μS/cm"),
        ("µV", "μV"),
        ("µg/ft³", "μg/ft³"),
        ("µg/m³", "μg/m³"),
        ("µmol/s⋅m²", "μmol/s⋅m²"),
        ("µg", "μg"),
        ("µs", "μs"),
        ("mVAr", "mvar"),
        ("VAr", "var"),
        ("kVAr", "kvar"),
        ("VArh", "varh"),
        ("kVArh", "kvarh"),
    ] {
        let mut identity = spec("fixture", "alias");
        identity.info.unit_of_measurement = Some(raw.to_owned());
        let from_identity = SensorDescriptor::new(identity).unwrap();
        let from_builder = descriptor("fixture", "alias", None, None, Some(raw));
        for descriptor in [from_identity, from_builder] {
            assert_eq!(descriptor.native_unit().unwrap().as_str(), raw);
            assert_eq!(
                descriptor.entity_spec().info.unit_of_measurement.as_deref(),
                Some(expected)
            );
            let projected = descriptor.project(SensorReading::new(1.0_f64)).unwrap();
            assert_eq!(
                projected.snapshot.attributes.get("unit_of_measurement"),
                Some(&json!(expected))
            );
        }
    }
    let particulate = descriptor(
        "fixture",
        "pm25",
        Some(SensorDeviceClass::Pm25),
        None,
        Some("µg/m³"),
    )
    .project(SensorReading::new(1.0_f64))
    .unwrap();
    assert!(particulate.warnings.is_empty());
}

#[test]
fn unit_selection_obeys_priority_and_converts_only_matching_classes() {
    let celsius = descriptor(
        "weather",
        "temperature",
        Some(SensorDeviceClass::Temperature),
        Some(SensorStateClass::Measurement),
        Some("°C"),
    )
    .with_suggested_unit("°F")
    .with_suggested_display_precision(1);
    let suggested = celsius.project(SensorReading::new(20.0_f64)).unwrap();
    let suggested_value: f64 = suggested
        .snapshot
        .state
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    assert!((suggested_value - 68.0).abs() < 0.000_001);
    assert_eq!(
        suggested.snapshot.attributes.get("unit_of_measurement"),
        Some(&json!("°F"))
    );
    assert_eq!(
        suggested
            .snapshot
            .attributes
            .get("suggested_display_precision"),
        Some(&json!(1))
    );
    assert_eq!(
        celsius.entity_spec().info.unit_of_measurement.as_deref(),
        Some("°F")
    );
    assert_eq!(celsius.native_unit().unwrap().as_str(), "°C");
    assert_eq!(celsius.suggested_display_precision(), Some(1));

    let numeric_string = celsius.project(SensorReading::new("21")).unwrap();
    let numeric_string_value: f64 = numeric_string.snapshot.state.unwrap().parse().unwrap();
    assert!((numeric_string_value - 69.8).abs() < 0.000_001);

    let user_unit_wins = celsius
        .project_with_options(SensorReading::new(20.0_f64), options("°C"))
        .unwrap();
    let user_unit_value: f64 = user_unit_wins
        .snapshot
        .state
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    assert!((user_unit_value - 20.0).abs() < 0.000_001);
    assert_eq!(
        user_unit_wins
            .snapshot
            .attributes
            .get("unit_of_measurement"),
        Some(&json!("°C"))
    );

    let automatic_temperature = descriptor(
        "weather",
        "automatic-temperature",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("°C"),
    )
    .project_with_options(
        SensorReading::new(100_i64),
        SensorProjectionOptions {
            user_unit: None,
            temperature_unit: Some(SensorUnit::from("°F")),
        },
    )
    .unwrap();
    let automatic_value: f64 = automatic_temperature
        .snapshot
        .state
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    assert!((automatic_value - 212.0).abs() < 0.000_001);

    let wrong_class = descriptor(
        "weather",
        "wrong-class",
        Some(SensorDeviceClass::Pressure),
        None,
        Some("°C"),
    )
    .project_with_options(SensorReading::new(20.0_f64), options("°F"));
    assert!(matches!(
        wrong_class.unwrap_err().kind,
        SensorErrorKind::UnitConversion {
            error: ConversionError::UnrecognizedUnit {
                family: UnitFamily::Pressure,
                side: UnitSide::From,
                unit: Some(ref unit),
                ..
            }
        } if unit == "°C"
    ));
}

#[test]
fn common_pressure_length_energy_and_power_conversions_are_class_gated() {
    let cases = [
        (
            "pressure",
            SensorDeviceClass::Pressure,
            SensorStateClass::Measurement,
            "hPa",
            "inHg",
            1000.0,
            29.529_98,
        ),
        (
            "distance",
            SensorDeviceClass::Distance,
            SensorStateClass::Measurement,
            "ft",
            "m",
            1.0,
            0.3048,
        ),
        (
            "energy",
            SensorDeviceClass::Energy,
            SensorStateClass::Total,
            "kWh",
            "Wh",
            1.0,
            1000.0,
        ),
        (
            "power",
            SensorDeviceClass::Power,
            SensorStateClass::Measurement,
            "kW",
            "W",
            1.0,
            1000.0,
        ),
    ];

    for (name, class, state_class, from, to, value, expected) in cases {
        let projected = descriptor("units", name, Some(class), Some(state_class), Some(from))
            .project_with_options(SensorReading::new(value), options(to))
            .unwrap();
        let actual: f64 = projected.snapshot.state.unwrap().parse().unwrap();
        assert!((actual - expected).abs() < 0.000_02, "{name}: {actual}");
    }
}

#[test]
fn pinned_sensor_device_classes_use_their_complete_converter_families() {
    let cases = [
        (
            SensorDeviceClass::ApparentPower,
            Some("mVA"),
            "VA",
            1000.0,
            1.0,
        ),
        (
            SensorDeviceClass::AbsoluteHumidity,
            Some("mg/m³"),
            "g/m³",
            1000.0,
            1.0,
        ),
        (SensorDeviceClass::Area, Some("m²"), "cm²", 1.0, 10_000.0),
        (
            SensorDeviceClass::AtmosphericPressure,
            Some("hPa"),
            "Pa",
            1.0,
            100.0,
        ),
        (
            SensorDeviceClass::BloodGlucoseConcentration,
            Some("mg/dL"),
            "mmol/L",
            18.016,
            1.0,
        ),
        (
            SensorDeviceClass::CarbonMonoxide,
            Some("ppb"),
            "ppm",
            1000.0,
            1.0,
        ),
        (
            SensorDeviceClass::Conductivity,
            Some("mS/cm"),
            "μS/cm",
            1.0,
            1000.0,
        ),
        (SensorDeviceClass::Current, Some("mA"), "A", 1000.0, 1.0),
        (
            SensorDeviceClass::DataRate,
            Some("Mbit/s"),
            "kbit/s",
            1.0,
            1000.0,
        ),
        (SensorDeviceClass::DataSize, Some("kB"), "B", 1.0, 1000.0),
        (SensorDeviceClass::Distance, Some("m"), "cm", 1.0, 100.0),
        (SensorDeviceClass::Duration, Some("h"), "min", 1.0, 60.0),
        (SensorDeviceClass::Energy, Some("kWh"), "Wh", 1.0, 1000.0),
        (
            SensorDeviceClass::EnergyDistance,
            Some("kWh/100km"),
            "km/kWh",
            5.0,
            20.0,
        ),
        (
            SensorDeviceClass::EnergyStorage,
            Some("kWh"),
            "Wh",
            1.0,
            1000.0,
        ),
        (SensorDeviceClass::Frequency, Some("kHz"), "Hz", 1.0, 1000.0),
        (SensorDeviceClass::Gas, Some("m³"), "L", 1.0, 1000.0),
        (
            SensorDeviceClass::NitrogenDioxide,
            Some("ppb"),
            "ppm",
            1000.0,
            1.0,
        ),
        (
            SensorDeviceClass::NitrogenMonoxide,
            Some("ppb"),
            "μg/m³",
            1.0,
            1.247_389_491_656_190_4,
        ),
        (SensorDeviceClass::Ozone, Some("ppb"), "ppm", 1000.0, 1.0),
        (SensorDeviceClass::Power, Some("kW"), "W", 1.0, 1000.0),
        (SensorDeviceClass::PowerFactor, None, "%", 0.5, 50.0),
        (
            SensorDeviceClass::Precipitation,
            Some("m"),
            "cm",
            1.0,
            100.0,
        ),
        (
            SensorDeviceClass::PrecipitationIntensity,
            Some("mm/h"),
            "mm/d",
            1.0,
            24.0,
        ),
        (SensorDeviceClass::Pressure, Some("hPa"), "Pa", 1.0, 100.0),
        (SensorDeviceClass::Radon, Some("pCi/L"), "Bq/m³", 1.0, 37.0),
        (
            SensorDeviceClass::ReactiveEnergy,
            Some("kvarh"),
            "varh",
            1.0,
            1000.0,
        ),
        (
            SensorDeviceClass::ReactivePower,
            Some("kvar"),
            "var",
            1.0,
            1000.0,
        ),
        (
            SensorDeviceClass::SulphurDioxide,
            Some("ppb"),
            "μg/m³",
            1.0,
            2.663_300_301_353_574,
        ),
        (SensorDeviceClass::Speed, Some("km/h"), "m/s", 3.6, 1.0),
        (SensorDeviceClass::Temperature, Some("°C"), "°F", 0.0, 32.0),
        (
            SensorDeviceClass::TemperatureDelta,
            Some("°C"),
            "°F",
            1.0,
            1.8,
        ),
        (
            SensorDeviceClass::VolatileOrganicCompounds,
            Some("mg/m³"),
            "μg/m³",
            1.0,
            1000.0,
        ),
        (
            SensorDeviceClass::VolatileOrganicCompoundsParts,
            Some("ppb"),
            "ppm",
            1000.0,
            1.0,
        ),
        (SensorDeviceClass::Voltage, Some("kV"), "V", 1.0, 1000.0),
        (SensorDeviceClass::Volume, Some("m³"), "L", 1.0, 1000.0),
        (
            SensorDeviceClass::VolumeStorage,
            Some("m³"),
            "L",
            1.0,
            1000.0,
        ),
        (
            SensorDeviceClass::VolumeFlowRate,
            Some("m³/h"),
            "L/min",
            1.0,
            16.666_666_666_666_668,
        ),
        (SensorDeviceClass::Water, Some("m³"), "L", 1.0, 1000.0),
        (SensorDeviceClass::Weight, Some("kg"), "g", 1.0, 1000.0),
        (SensorDeviceClass::WindSpeed, Some("km/h"), "m/s", 3.6, 1.0),
    ];

    assert_eq!(
        cases.len(),
        41,
        "one golden for every pinned converter mapping"
    );
    for (class, from, to, input, expected) in cases {
        let projected = descriptor("units", class.as_str(), Some(class), None, from)
            .project_with_options(SensorReading::new(input), options(to))
            .unwrap_or_else(|error| panic!("{class:?} {from:?} -> {to}: {error}"));
        let actual: f64 = projected
            .snapshot
            .state
            .as_deref()
            .unwrap_or_else(|| panic!("{class:?} unexpectedly projected unknown"))
            .parse()
            .unwrap();
        assert!(
            (actual - expected).abs() <= expected.abs() * 1e-8 + 1e-8,
            "{class:?} {from:?} -> {to}: expected {expected}, got {actual}"
        );
        assert_eq!(
            projected.snapshot.attributes.get("unit_of_measurement"),
            Some(&json!(to)),
            "{class:?} must publish the selected unit"
        );
    }
}

#[test]
fn projection_preserves_converter_identity_errors_and_optional_results() {
    let unknown_identity = descriptor(
        "units",
        "unknown-identity",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("unknown-temperature-unit"),
    )
    .project(SensorReading::new(9_007_199_254_740_993_u64))
    .unwrap();
    assert_eq!(
        unknown_identity.snapshot.state.as_deref(),
        Some("9007199254740993")
    );
    assert!(matches!(
        unknown_identity.warnings.as_slice(),
        [SensorWarning::InvalidDeviceClassUnit {
            device_class: SensorDeviceClass::Temperature,
            ..
        }]
    ));

    let invalid_source = descriptor(
        "units",
        "invalid-source",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("Celsius"),
    )
    .project_with_options(SensorReading::new(20.0_f64), options("°F"))
    .unwrap_err();
    assert!(matches!(
        invalid_source.kind,
        SensorErrorKind::UnitConversion {
            error: ConversionError::UnrecognizedUnit {
                family: UnitFamily::Temperature,
                side: UnitSide::From,
                unit: Some(ref unit),
                ref reported_unit,
            }
        } if unit == "Celsius" && reported_unit == "Celsius"
    ));

    let invalid_target = descriptor(
        "units",
        "invalid-target",
        Some(SensorDeviceClass::Power),
        None,
        Some("W"),
    )
    .project_with_options(SensorReading::new(1.0_f64), options("widgets"))
    .unwrap_err();
    assert!(matches!(
        invalid_target.kind,
        SensorErrorKind::UnitConversion {
            error: ConversionError::UnrecognizedUnit {
                family: UnitFamily::Power,
                side: UnitSide::To,
                unit: Some(ref unit),
                ref reported_unit,
            }
        } if unit == "widgets" && reported_unit == "widgets"
    ));

    let missing_value = descriptor(
        "units",
        "missing-invalid-source",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("Celsius"),
    )
    .project_with_options(SensorReading::without_value(), options("°F"))
    .unwrap();
    assert_eq!(missing_value.snapshot.state, None);
    assert!(missing_value.warnings.is_empty());

    let reciprocal_zero = descriptor(
        "units",
        "reciprocal-zero",
        Some(SensorDeviceClass::EnergyDistance),
        None,
        Some("kWh/100km"),
    )
    .project_with_options(SensorReading::new(0.0_f64), options("km/kWh"))
    .unwrap();
    assert_eq!(reciprocal_zero.snapshot.state, None);
    assert_eq!(
        reciprocal_zero
            .snapshot
            .attributes
            .get("unit_of_measurement"),
        Some(&json!("km/kWh"))
    );

    let negative_beaufort = descriptor(
        "units",
        "negative-beaufort",
        Some(SensorDeviceClass::WindSpeed),
        None,
        Some("Beaufort"),
    )
    .project_with_options(SensorReading::new(-1.0_f64), options("m/s"))
    .unwrap_err();
    assert!(matches!(
        negative_beaufort.kind,
        SensorErrorKind::UnitConversion {
            error: ConversionError::NonRealResult {
                family: UnitFamily::Speed
            }
        }
    ));
}

#[test]
fn suggested_and_unitless_conversions_use_effective_display_units() {
    let apparent_power = descriptor(
        "units",
        "suggested-apparent-power",
        Some(SensorDeviceClass::ApparentPower),
        None,
        Some("VA"),
    )
    .with_suggested_unit("kVA");
    let suggested = apparent_power
        .project(SensorReading::new(1000.0_f64))
        .unwrap();
    assert_eq!(suggested.snapshot.state.as_deref(), Some("1.0"));
    assert_eq!(
        suggested.snapshot.attributes.get("unit_of_measurement"),
        Some(&json!("kVA"))
    );
    let user_selected = apparent_power
        .project_with_options(SensorReading::new(1000.0_f64), options("mVA"))
        .unwrap();
    assert_eq!(user_selected.snapshot.state.as_deref(), Some("1000000.0"));

    let unitless = descriptor(
        "units",
        "unitless-power-factor",
        Some(SensorDeviceClass::PowerFactor),
        None,
        None,
    )
    .project_with_options(SensorReading::new(0.5_f64), options("%"))
    .unwrap();
    assert_eq!(unitless.snapshot.state.as_deref(), Some("50.0"));
    assert!(unitless.warnings.is_empty());
}

#[test]
fn warning_only_mismatches_and_value_errors_are_separate() {
    let invalid_class_pair = descriptor(
        "validation",
        "class-warning",
        Some(SensorDeviceClass::Temperature),
        Some(SensorStateClass::Total),
        Some("°C"),
    )
    .project(SensorReading::new(2.0_f64))
    .unwrap();
    assert!(matches!(
        invalid_class_pair.warnings.as_slice(),
        [SensorWarning::InvalidDeviceStateClass { .. }]
    ));
    assert_eq!(invalid_class_pair.snapshot.state.as_deref(), Some("2.0"));

    let invalid_device_unit = descriptor(
        "validation",
        "unit-warning",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("Celsius"),
    )
    .project(SensorReading::new(2.0_f64))
    .unwrap();
    assert!(matches!(
        invalid_device_unit.warnings.as_slice(),
        [SensorWarning::InvalidDeviceClassUnit { .. }]
    ));

    let missing_temperature_unit = descriptor(
        "validation",
        "missing-unit-warning",
        Some(SensorDeviceClass::Temperature),
        None,
        None,
    )
    .project(SensorReading::new(2.0_f64))
    .unwrap();
    assert!(matches!(
        missing_temperature_unit.warnings.as_slice(),
        [SensorWarning::InvalidDeviceClassUnit { unit: None, .. }]
    ));

    let bad_angle_unit = descriptor(
        "validation",
        "angle-error",
        None,
        Some(SensorStateClass::MeasurementAngle),
        Some("rad"),
    )
    .project(SensorReading::new(2.0_f64))
    .unwrap_err();
    assert!(matches!(
        bad_angle_unit.kind,
        SensorErrorKind::InvalidStateClassUnit {
            state_class: SensorStateClass::MeasurementAngle,
            ..
        }
    ));

    let unit_without_native = descriptor("validation", "unit-without-native", None, None, None)
        .project_with_options(SensorReading::new(2.0_f64), options("widgets"))
        .unwrap_err();
    assert!(matches!(
        unit_without_native.kind,
        SensorErrorKind::UnsupportedUnitConversion { from: None, .. }
    ));

    let non_numeric_unit = descriptor(
        "validation",
        "date-with-unit",
        Some(SensorDeviceClass::Date),
        None,
        Some("°C"),
    )
    .project(SensorReading::without_value())
    .unwrap_err();
    assert!(matches!(
        non_numeric_unit.kind,
        SensorErrorKind::NonNumericDeviceClassHasUnit {
            device_class: SensorDeviceClass::Date,
            ..
        }
    ));

    let invalid_number = descriptor(
        "validation",
        "numeric-error",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("°C"),
    )
    .project(SensorReading::new("not a number"))
    .unwrap_err();
    assert!(matches!(
        invalid_number.kind,
        SensorErrorKind::NumericValueRequired { .. }
    ));

    let non_finite = descriptor(
        "validation",
        "finite-error",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("°C"),
    )
    .project(SensorReading::new(f64::NAN))
    .unwrap_err();
    assert!(matches!(
        non_finite.kind,
        SensorErrorKind::NonFiniteValue { .. }
    ));

    let precision_requires_numeric = SensorDescriptor::new(spec("validation", "precision"))
        .unwrap()
        .with_suggested_display_precision(2)
        .project(SensorReading::new("not numeric"))
        .unwrap_err();
    assert!(matches!(
        precision_requires_numeric.kind,
        SensorErrorKind::NumericValueRequired { .. }
    ));

    let decimal = descriptor(
        "validation",
        "decimal",
        None,
        Some(SensorStateClass::Measurement),
        None,
    )
    .project(SensorReading::new(SensorValue::Decimal("1.230".to_owned())))
    .unwrap();
    assert_eq!(decimal.snapshot.state.as_deref(), Some("1.230"));

    let invalid_reset = descriptor(
        "validation",
        "reset-error",
        None,
        Some(SensorStateClass::Measurement),
        None,
    )
    .project(SensorReading::new(2.0_f64).with_last_reset(datetime("2026-11-07T00:00:00Z")))
    .unwrap_err();
    assert!(matches!(
        invalid_reset.kind,
        SensorErrorKind::LastResetRequiresTotal {
            state_class: Some(SensorStateClass::Measurement)
        }
    ));

    let reset = descriptor(
        "validation",
        "reset-valid",
        Some(SensorDeviceClass::Energy),
        Some(SensorStateClass::Total),
        Some("kWh"),
    )
    .project(SensorReading::new(2.0_f64).with_last_reset(datetime("2026-11-07T00:00:00Z")))
    .unwrap();
    assert_eq!(
        reset.snapshot.attributes.get("last_reset"),
        Some(&json!("2026-11-07T00:00:00+00:00"))
    );
}

#[test]
fn enum_options_are_validated_only_for_enum_class() {
    let enum_sensor = descriptor("enum", "state", Some(SensorDeviceClass::Enum), None, None)
        .with_options(["idle", "running"]);
    let good = enum_sensor.project(SensorReading::new("running")).unwrap();
    assert_eq!(good.snapshot.state.as_deref(), Some("running"));
    assert_eq!(
        good.snapshot.attributes.get("options"),
        Some(&json!(["idle", "running"]))
    );
    assert!(matches!(
        enum_sensor
            .project(SensorReading::new("stopped"))
            .unwrap_err()
            .kind,
        SensorErrorKind::EnumValueNotInOptions { .. }
    ));

    let options_without_enum = descriptor(
        "enum",
        "wrong-class",
        Some(SensorDeviceClass::Temperature),
        None,
        Some("°C"),
    )
    .with_options(["idle"])
    .project(SensorReading::new(2.0_f64))
    .unwrap_err();
    assert!(matches!(
        options_without_enum.kind,
        SensorErrorKind::OptionsRequireEnum { .. }
    ));
}

#[tokio::test]
async fn lifecycle_publishes_projected_values_across_rename_disable_and_unload() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let descriptor = SensorDescriptor::new(spec("lifecycle", "temperature"))
        .unwrap()
        .with_name("Outside temperature")
        .with_device_class(SensorDeviceClass::Temperature)
        .with_state_class(SensorStateClass::Measurement)
        .with_native_unit("°C")
        .with_suggested_unit("°F");

    let handle = lifecycle
        .register_provider_batch("lifecycle", vec![descriptor.entity_spec()])
        .unwrap()
        .remove(0);
    let old_id = handle.current_entity_id().unwrap();
    let initial = descriptor.project(SensorReading::new(20.0_f64)).unwrap();
    handle.publish(initial.snapshot).unwrap().unwrap();
    let state = runtime.get_state(&old_id).unwrap();
    assert_eq!(state.state, "68.0");
    assert_eq!(
        state.attributes.get("device_class"),
        Some(&json!("temperature"))
    );
    assert_eq!(
        state.attributes.get("state_class"),
        Some(&json!("measurement"))
    );
    assert_eq!(
        state.attributes.get("unit_of_measurement"),
        Some(&json!("°F"))
    );

    let new_id = "sensor.renamed_outside_temperature";
    registries
        .update_entity(
            &old_id,
            EntityUpdate {
                new_entity_id: Some(new_id.to_owned()),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| runtime.get_state(&old_id).is_none() && runtime.get_state(new_id).is_some())
        .await;
    handle
        .publish(
            descriptor
                .project(SensorReading::new(21.0_f64))
                .unwrap()
                .snapshot,
        )
        .unwrap()
        .unwrap();
    let renamed_value: f64 = runtime.get_state(new_id).unwrap().state.parse().unwrap();
    assert!((renamed_value - 69.8).abs() < 0.000_001);

    registries
        .update_entity(
            new_id,
            EntityUpdate {
                disabled_by: Some(Some("user".to_owned())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state(new_id).is_none()
            && matches!(handle.status(), EntityStatus::Disabled { .. })
    })
    .await;
    assert!(
        handle
            .publish(
                descriptor
                    .project(SensorReading::new(22.0_f64))
                    .unwrap()
                    .snapshot
            )
            .unwrap()
            .is_none()
    );

    registries
        .update_entity(
            new_id,
            EntityUpdate {
                disabled_by: Some(None),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| matches!(handle.status(), EntityStatus::Active { .. })).await;
    handle
        .publish(
            descriptor
                .project(SensorReading::new(22.0_f64))
                .unwrap()
                .snapshot,
        )
        .unwrap()
        .unwrap();
    let reenabled_value: f64 = runtime.get_state(new_id).unwrap().state.parse().unwrap();
    assert!((reenabled_value - 71.6).abs() < 0.000_001);

    lifecycle
        .register_provider_batch("lifecycle", Vec::new())
        .unwrap();
    assert_eq!(runtime.get_state(new_id).unwrap().state, "unavailable");
    lifecycle.shutdown().await.unwrap();
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if condition() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("entity lifecycle transition did not complete");
}
