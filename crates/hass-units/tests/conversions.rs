use hass_units::{
    ConversionError, UnitFamily, UnitSide, convert, convert_optional, is_valid_unit,
    supported_units,
};

const FAMILIES: [UnitFamily; 31] = [
    UnitFamily::ApparentPower,
    UnitFamily::Area,
    UnitFamily::BloodGlucoseConcentration,
    UnitFamily::CarbonMonoxideConcentration,
    UnitFamily::Conductivity,
    UnitFamily::DataRate,
    UnitFamily::Distance,
    UnitFamily::Duration,
    UnitFamily::ElectricCurrent,
    UnitFamily::Frequency,
    UnitFamily::ElectricPotential,
    UnitFamily::Energy,
    UnitFamily::EnergyDistance,
    UnitFamily::Information,
    UnitFamily::Mass,
    UnitFamily::MassVolumeConcentration,
    UnitFamily::NitrogenDioxideConcentration,
    UnitFamily::NitrogenMonoxideConcentration,
    UnitFamily::OzoneConcentration,
    UnitFamily::Power,
    UnitFamily::Pressure,
    UnitFamily::RadiationConcentration,
    UnitFamily::ReactiveEnergy,
    UnitFamily::ReactivePower,
    UnitFamily::Speed,
    UnitFamily::SulphurDioxideConcentration,
    UnitFamily::Temperature,
    UnitFamily::TemperatureDelta,
    UnitFamily::UnitlessRatio,
    UnitFamily::Volume,
    UnitFamily::VolumeFlowRate,
];

fn close(actual: f64, expected: f64) {
    let tolerance = 1.0e-10 * expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual:.17e} did not match expected {expected:.17e} within {tolerance:.3e}"
    );
}

#[test]
fn supported_unit_counts_match_the_pinned_converter_tables() {
    let expected_counts = [
        3, 10, 2, 4, 3, 11, 9, 7, 3, 5, 5, 15, 4, 21, 7, 3, 3, 2, 3, 7, 12, 2, 2, 3, 13, 2, 3, 3,
        4, 8, 11,
    ];
    assert_eq!(expected_counts.iter().sum::<usize>(), 190);
    for (family, count) in FAMILIES.into_iter().zip(expected_counts) {
        assert_eq!(supported_units(family).len(), count, "{family:?}");
        for unit in supported_units(family) {
            assert!(is_valid_unit(family, *unit), "{family:?} {unit:?}");
        }
        assert!(
            !is_valid_unit(family, Some("__unsupported__")),
            "{family:?}"
        );
    }
}

#[test]
fn golden_conversions_cover_every_converter_family() {
    let cases = [
        (
            UnitFamily::ApparentPower,
            2_000.0,
            Some("mVA"),
            Some("VA"),
            2.0,
        ),
        (UnitFamily::Area, 1.0, Some("ha"), Some("m²"), 10_000.0),
        (
            UnitFamily::BloodGlucoseConcentration,
            180.16,
            Some("mg/dL"),
            Some("mmol/L"),
            10.0,
        ),
        (
            UnitFamily::CarbonMonoxideConcentration,
            1.0,
            Some("ppm"),
            Some("mg/m³"),
            1.1644092255004779,
        ),
        (
            UnitFamily::Conductivity,
            1.0,
            Some("S/cm"),
            Some("μS/cm"),
            1_000_000.0,
        ),
        (
            UnitFamily::DataRate,
            1.0,
            Some("MiB/s"),
            Some("bit/s"),
            8_388_608.0,
        ),
        (UnitFamily::Distance, 1.0, Some("mi"), Some("m"), 1609.344),
        (UnitFamily::Duration, 1.0, Some("w"), Some("s"), 604_800.0),
        (
            UnitFamily::ElectricCurrent,
            1.0,
            Some("A"),
            Some("mA"),
            1000.0,
        ),
        (
            UnitFamily::Frequency,
            1.0,
            Some("MHz"),
            Some("Hz"),
            1_000_000.0,
        ),
        (
            UnitFamily::ElectricPotential,
            1.0,
            Some("kV"),
            Some("V"),
            1000.0,
        ),
        (UnitFamily::Energy, 1.0, Some("kWh"), Some("J"), 3_600_000.0),
        (
            UnitFamily::EnergyDistance,
            1.0,
            Some("mi/kWh"),
            Some("kWh/100km"),
            62.137_119_223_733_41,
        ),
        (UnitFamily::Information, 1.0, Some("KiB"), Some("B"), 1024.0),
        (UnitFamily::Mass, 1.0, Some("lb"), Some("g"), 453.59237),
        (
            UnitFamily::MassVolumeConcentration,
            1.0,
            Some("g/m³"),
            Some("mg/m³"),
            1000.0,
        ),
        (
            UnitFamily::NitrogenDioxideConcentration,
            1.0,
            Some("ppb"),
            Some("μg/m³"),
            1.9125036995273914,
        ),
        (
            UnitFamily::NitrogenMonoxideConcentration,
            1.0,
            Some("ppb"),
            Some("μg/m³"),
            1.2473894916561903,
        ),
        (
            UnitFamily::OzoneConcentration,
            1.0,
            Some("ppm"),
            Some("μg/m³"),
            1_995.417_451_768_045,
        ),
        (UnitFamily::Power, 1.0, Some("BTU/h"), Some("W"), 0.29307107),
        (
            UnitFamily::Pressure,
            1.0,
            Some("inHg"),
            Some("Pa"),
            3386.3886403409997,
        ),
        (
            UnitFamily::RadiationConcentration,
            1.0,
            Some("pCi/L"),
            Some("Bq/m³"),
            37.0,
        ),
        (
            UnitFamily::ReactiveEnergy,
            1.0,
            Some("kvarh"),
            Some("varh"),
            1000.0,
        ),
        (
            UnitFamily::ReactivePower,
            1.0,
            Some("kvar"),
            Some("var"),
            1000.0,
        ),
        (UnitFamily::Speed, 1.0, Some("Beaufort"), Some("m/s"), 0.836),
        (
            UnitFamily::SulphurDioxideConcentration,
            1.0,
            Some("ppb"),
            Some("μg/m³"),
            2.663_300_301_353_574,
        ),
        (UnitFamily::Temperature, 0.0, Some("°C"), Some("°F"), 32.0),
        (
            UnitFamily::TemperatureDelta,
            10.0,
            Some("°C"),
            Some("°F"),
            18.0,
        ),
        (UnitFamily::UnitlessRatio, 0.5, None, Some("%"), 50.0),
        (UnitFamily::Volume, 1.0, Some("gal"), Some("L"), 3.785411784),
        (
            UnitFamily::VolumeFlowRate,
            1.0,
            Some("m³/s"),
            Some("m³/h"),
            3600.0,
        ),
    ];

    assert_eq!(cases.len(), FAMILIES.len());
    for (family, value, from, to, expected) in cases {
        close(convert(family, value, from, to).unwrap(), expected);
    }
}

#[test]
fn supported_conversions_compose_for_all_families_except_rounded_beaufort() {
    for family in FAMILIES {
        if family == UnitFamily::Speed {
            continue;
        }
        for from in supported_units(family) {
            for middle in supported_units(family) {
                for to in supported_units(family) {
                    let value = 7.25;
                    let first = convert(family, value, *from, *middle).unwrap();
                    let chained = convert(family, first, *middle, *to).unwrap();
                    let direct = convert(family, value, *from, *to).unwrap();
                    close(chained, direct);
                }
            }
        }
    }
}

#[test]
fn temperature_is_affine_but_delta_is_a_scale_only_conversion() {
    close(
        convert(UnitFamily::Temperature, 20.0, Some("°C"), Some("°F")).unwrap(),
        68.0,
    );
    close(
        convert(UnitFamily::TemperatureDelta, 10.0, Some("°C"), Some("°F")).unwrap(),
        18.0,
    );
    close(
        convert(UnitFamily::TemperatureDelta, 32.0, Some("°F"), Some("°C")).unwrap(),
        17.77777777777778,
    );
}

#[test]
fn identity_and_none_boundaries_match_the_pinned_wrappers() {
    assert!(!is_valid_unit(UnitFamily::Distance, None));
    assert!(is_valid_unit(UnitFamily::UnitlessRatio, None));
    assert_eq!(convert(UnitFamily::Distance, 12.5, None, None), Ok(12.5));
    assert_eq!(
        convert(UnitFamily::Distance, 12.5, Some("unknown"), Some("unknown")),
        Ok(12.5)
    );
    assert_eq!(
        convert_optional(UnitFamily::Distance, None, Some("m"), Some("km")),
        Ok(None)
    );
    assert!(matches!(
        convert_optional(UnitFamily::Distance, None, None, Some("m")),
        Err(ConversionError::UnrecognizedUnit {
            side: UnitSide::From,
            ..
        })
    ));
}

#[test]
fn invalid_units_preserve_side_and_original_message_unit() {
    let from_error = convert(UnitFamily::Distance, 1.0, Some("furlong"), Some("m")).unwrap_err();
    assert_eq!(
        from_error,
        ConversionError::UnrecognizedUnit {
            family: UnitFamily::Distance,
            side: UnitSide::From,
            unit: Some("furlong".to_owned()),
            reported_unit: "furlong".to_owned(),
        }
    );
    assert_eq!(
        from_error.to_string(),
        "furlong is not a recognized distance unit."
    );

    let to_error = convert(UnitFamily::Distance, 1.0, Some("m"), Some("furlong")).unwrap_err();
    assert!(matches!(
        to_error,
        ConversionError::UnrecognizedUnit {
            side: UnitSide::To,
            ..
        }
    ));

    let temperature_from =
        convert(UnitFamily::Temperature, 1.0, Some("unknown"), Some("°C")).unwrap_err();
    assert!(matches!(
        temperature_from,
        ConversionError::UnrecognizedUnit {
            side: UnitSide::From,
            ..
        }
    ));
    let temperature_to =
        convert(UnitFamily::Temperature, 1.0, Some("°C"), Some("unknown")).unwrap_err();
    assert!(matches!(
        temperature_to,
        ConversionError::UnrecognizedUnit {
            side: UnitSide::To,
            ..
        }
    ));

    let speed_error = convert(UnitFamily::Speed, 1.0, Some("unknown"), Some("m/s")).unwrap_err();
    assert!(matches!(
        &speed_error,
        ConversionError::UnrecognizedUnit {
            side: UnitSide::From,
            unit: Some(unit),
            reported_unit,
            ..
        } if unit == "unknown" && reported_unit == "m/s"
    ));
    assert_eq!(
        speed_error.to_string(),
        "m/s is not a recognized speed unit."
    );
}

#[test]
fn reciprocal_energy_distance_zero_matches_both_source_entry_points() {
    assert_eq!(
        convert(
            UnitFamily::EnergyDistance,
            0.0,
            Some("mi/kWh"),
            Some("kWh/100km")
        ),
        Err(ConversionError::DivisionByZero {
            family: UnitFamily::EnergyDistance
        })
    );
    assert_eq!(
        convert_optional(
            UnitFamily::EnergyDistance,
            Some(0.0),
            Some("mi/kWh"),
            Some("kWh/100km")
        ),
        Ok(None)
    );
}

#[test]
fn beaufort_rounding_uses_ties_to_even_and_rejects_non_real_f64_results() {
    close(
        convert(UnitFamily::Speed, 0.836, Some("m/s"), Some("Beaufort")).unwrap(),
        1.0,
    );
    assert_eq!(
        convert(
            UnitFamily::Speed,
            3.304_580_154_875_956,
            Some("m/s"),
            Some("Beaufort")
        ),
        Ok(2.0)
    );
    assert_eq!(
        convert(
            UnitFamily::Speed,
            5.474_044_756_850_276,
            Some("m/s"),
            Some("Beaufort")
        ),
        Ok(3.0)
    );
    assert_eq!(
        convert(UnitFamily::Speed, -1.0, Some("Beaufort"), Some("m/s")),
        Err(ConversionError::NonRealResult {
            family: UnitFamily::Speed
        })
    );
}

#[test]
fn reciprocal_nonzero_underflow_is_not_an_optional_zero_reading() {
    for value in [f64::from_bits(1), -f64::from_bits(1)] {
        assert_eq!(
            convert_optional(
                UnitFamily::EnergyDistance,
                Some(value),
                Some("km/kWh"),
                Some("kWh/100km")
            ),
            Err(ConversionError::DivisionByZero {
                family: UnitFamily::EnergyDistance
            })
        );
    }
}

#[test]
fn beaufort_exceptional_values_preserve_rounding_and_power_errors() {
    assert_eq!(
        convert(UnitFamily::Speed, f64::NAN, Some("m/s"), Some("Beaufort")),
        Err(ConversionError::CannotRoundNaN {
            family: UnitFamily::Speed
        })
    );
    for value in [f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            convert(UnitFamily::Speed, value, Some("m/s"), Some("Beaufort")),
            Err(ConversionError::CannotRoundInfinity {
                family: UnitFamily::Speed
            })
        );
        let converted = convert(UnitFamily::Speed, value, Some("Beaufort"), Some("m/s")).unwrap();
        assert!(converted.is_infinite() && converted.is_sign_positive());
    }
    assert!(
        convert(UnitFamily::Speed, f64::NAN, Some("Beaufort"), Some("m/s"))
            .unwrap()
            .is_nan()
    );
    for (value, from, to) in [(1e200, "m/s", "Beaufort"), (1e308, "Beaufort", "m/s")] {
        assert_eq!(
            convert(UnitFamily::Speed, value, Some(from), Some(to)),
            Err(ConversionError::ArithmeticOverflow {
                family: UnitFamily::Speed
            })
        );
    }
}
