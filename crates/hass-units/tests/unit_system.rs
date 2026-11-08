use hass_units::{
    AreaUnit, ConversionError, IMPERIAL_SYSTEM, LengthUnit, METRIC_SYSTEM, METRIC_UNITS, MassUnit,
    PressureUnit, TemperatureUnit, US_CUSTOMARY_SYSTEM, US_CUSTOMARY_UNITS, UnitFamily, UnitSide,
    UnitSystem, UnitSystemConversion, UnitSystemError, UnitSystemKind, UnitSystemProperty,
    UnitSystemUnitNames, UnitSystemUnits, VolumeUnit, WindSpeedUnit, configurable_units,
    get_unit_system, get_unit_system_optional, validate_unit_system,
};
use std::collections::BTreeMap;

fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

fn names(units: UnitSystemUnits) -> UnitSystemUnitNames<'static> {
    UnitSystemUnitNames {
        accumulated_precipitation: units.accumulated_precipitation.as_str(),
        area: units.area.as_str(),
        length: units.length.as_str(),
        mass: units.mass.as_str(),
        pressure: units.pressure.as_str(),
        temperature: units.temperature.as_str(),
        volume: units.volume.as_str(),
        wind_speed: units.wind_speed.as_str(),
    }
}

fn names_with(
    mut selections: UnitSystemUnitNames<'static>,
    property: UnitSystemProperty,
    unit: &'static str,
) -> UnitSystemUnitNames<'static> {
    match property {
        UnitSystemProperty::Length => selections.length = unit,
        UnitSystemProperty::AccumulatedPrecipitation => selections.accumulated_precipitation = unit,
        UnitSystemProperty::Area => selections.area = unit,
        UnitSystemProperty::Mass => selections.mass = unit,
        UnitSystemProperty::Pressure => selections.pressure = unit,
        UnitSystemProperty::Temperature => selections.temperature = unit,
        UnitSystemProperty::Volume => selections.volume = unit,
        UnitSystemProperty::WindSpeed => selections.wind_speed = unit,
    }
    selections
}

#[test]
fn canonical_defaults_and_dictionary_shape_match_the_pinned_source() {
    assert_eq!(METRIC_SYSTEM.name(), "metric");
    assert_eq!(METRIC_SYSTEM.units(), METRIC_UNITS);
    assert_eq!(METRIC_SYSTEM.length_unit(), LengthUnit::Kilometers);
    assert_eq!(
        METRIC_SYSTEM.accumulated_precipitation_unit(),
        LengthUnit::Millimeters
    );
    assert_eq!(METRIC_SYSTEM.area_unit(), AreaUnit::SquareMeters);
    assert_eq!(METRIC_SYSTEM.mass_unit(), MassUnit::Grams);
    assert_eq!(METRIC_SYSTEM.pressure_unit(), PressureUnit::Pascals);
    assert_eq!(METRIC_SYSTEM.temperature_unit(), TemperatureUnit::Celsius);
    assert_eq!(METRIC_SYSTEM.volume_unit(), VolumeUnit::Liters);
    assert_eq!(
        METRIC_SYSTEM.wind_speed_unit(),
        WindSpeedUnit::MetersPerSecond
    );

    assert_eq!(US_CUSTOMARY_SYSTEM.name(), "us_customary");
    assert_eq!(US_CUSTOMARY_SYSTEM.units(), US_CUSTOMARY_UNITS);
    assert_eq!(US_CUSTOMARY_SYSTEM.length_unit(), LengthUnit::Miles);
    assert_eq!(
        US_CUSTOMARY_SYSTEM.accumulated_precipitation_unit(),
        LengthUnit::Inches
    );
    assert_eq!(US_CUSTOMARY_SYSTEM.area_unit(), AreaUnit::SquareFeet);
    assert_eq!(US_CUSTOMARY_SYSTEM.mass_unit(), MassUnit::Pounds);
    assert_eq!(
        US_CUSTOMARY_SYSTEM.pressure_unit(),
        PressureUnit::PoundsPerSquareInch
    );
    assert_eq!(
        US_CUSTOMARY_SYSTEM.temperature_unit(),
        TemperatureUnit::Fahrenheit
    );
    assert_eq!(US_CUSTOMARY_SYSTEM.volume_unit(), VolumeUnit::Gallons);
    assert_eq!(
        US_CUSTOMARY_SYSTEM.wind_speed_unit(),
        WindSpeedUnit::MilesPerHour
    );
    assert!(std::ptr::eq(IMPERIAL_SYSTEM, &US_CUSTOMARY_SYSTEM));

    let expected_metric = BTreeMap::from([
        ("length", "km"),
        ("accumulated_precipitation", "mm"),
        ("area", "m²"),
        ("mass", "g"),
        ("pressure", "Pa"),
        ("temperature", "°C"),
        ("volume", "L"),
        ("wind_speed", "m/s"),
    ]);
    let expected_us_customary = BTreeMap::from([
        ("length", "mi"),
        ("accumulated_precipitation", "in"),
        ("area", "ft²"),
        ("mass", "lb"),
        ("pressure", "psi"),
        ("temperature", "°F"),
        ("volume", "gal"),
        ("wind_speed", "mph"),
    ]);
    assert_eq!(METRIC_SYSTEM.as_dict(), expected_metric);
    assert_eq!(US_CUSTOMARY_SYSTEM.as_dict(), expected_us_customary);
    assert_eq!(METRIC_SYSTEM.as_dict().len(), 8);
}

#[test]
fn system_lookup_and_validation_keep_source_key_rules_distinct() {
    assert!(std::ptr::eq(
        get_unit_system("metric").unwrap(),
        &METRIC_SYSTEM
    ));
    assert!(std::ptr::eq(
        get_unit_system("us_customary").unwrap(),
        &US_CUSTOMARY_SYSTEM
    ));
    assert_eq!(
        get_unit_system_optional(None).unwrap_err().to_string(),
        "`None` is not a valid unit system key"
    );
    for key in ["", "Metric", "imperial", "invalid_custom"] {
        assert_eq!(
            get_unit_system(key).unwrap_err().to_string(),
            format!("`{key}` is not a valid unit system key")
        );
    }

    assert_eq!(
        validate_unit_system("metric").unwrap(),
        UnitSystemKind::Metric
    );
    assert_eq!(
        validate_unit_system("METRIC").unwrap(),
        UnitSystemKind::Metric
    );
    assert_eq!(
        validate_unit_system("IMPERIAL").unwrap(),
        UnitSystemKind::UsCustomary
    );
    assert_eq!(
        validate_unit_system("US_CUSTOMARY").unwrap(),
        UnitSystemKind::UsCustomary
    );
    assert_eq!(UnitSystemKind::Metric.unit_system(), &METRIC_SYSTEM);
    assert_eq!(
        validate_unit_system("unknown").unwrap_err(),
        UnitSystemError::InvalidSystemValue {
            value: "unknown".to_owned()
        }
    );
}

#[test]
fn every_pinned_selection_domain_is_accepted_and_keeps_its_source_boundary() {
    let properties = [
        UnitSystemProperty::Length,
        UnitSystemProperty::AccumulatedPrecipitation,
        UnitSystemProperty::Area,
        UnitSystemProperty::Mass,
        UnitSystemProperty::Pressure,
        UnitSystemProperty::Temperature,
        UnitSystemProperty::Volume,
        UnitSystemProperty::WindSpeed,
    ];
    let canonical_names = names(METRIC_UNITS);

    for property in properties {
        let accepted = configurable_units(property);
        assert!(
            !accepted.is_empty(),
            "{} had no selectable units",
            property.as_str()
        );
        for unit in accepted {
            let candidate = names_with(canonical_names, property, unit);
            UnitSystem::try_new("custom", candidate, &[]).unwrap_or_else(|error| {
                panic!(
                    "{} rejected supported unit {unit}: {error}",
                    property.as_str()
                )
            });
        }
    }

    assert_eq!(configurable_units(UnitSystemProperty::Length).len(), 9);
    assert_eq!(
        configurable_units(UnitSystemProperty::AccumulatedPrecipitation),
        configurable_units(UnitSystemProperty::Length)
    );
    assert_eq!(configurable_units(UnitSystemProperty::Area).len(), 10);
    assert_eq!(configurable_units(UnitSystemProperty::Mass).len(), 4);
    assert_eq!(configurable_units(UnitSystemProperty::Pressure).len(), 12);
    assert_eq!(
        configurable_units(UnitSystemProperty::Temperature),
        &["°C", "°F"]
    );
    assert_eq!(configurable_units(UnitSystemProperty::Volume).len(), 8);
    assert_eq!(configurable_units(UnitSystemProperty::WindSpeed).len(), 13);

    // Kelvin and stone are converter units, but not valid UnitSystem choices.
    assert!(hass_units::supported_units(UnitFamily::Temperature).contains(&Some("K")));
    assert!(hass_units::supported_units(UnitFamily::Mass).contains(&Some("st")));
    assert!(!configurable_units(UnitSystemProperty::Temperature).contains(&"K"));
    assert!(!configurable_units(UnitSystemProperty::Mass).contains(&"st"));
}

#[test]
fn custom_systems_validate_selections_and_preserve_joined_source_errors() {
    let invalid = UnitSystem::try_new(
        "bad",
        UnitSystemUnitNames {
            accumulated_precipitation: "bad-depth",
            area: "bad-area",
            length: "bad-length",
            mass: "st",
            pressure: "bad-pressure",
            temperature: "K",
            volume: "bad-volume",
            wind_speed: "bad-speed",
        },
        &[],
    )
    .unwrap_err();
    assert_eq!(
        invalid.to_string(),
        "bad-depth is not a recognized accumulated_precipitation unit., \
         bad-area is not a recognized area unit., K is not a recognized temperature unit., \
         bad-length is not a recognized length unit., bad-speed is not a recognized wind_speed unit., \
         bad-volume is not a recognized volume unit., st is not a recognized mass unit., \
         bad-pressure is not a recognized pressure unit."
            .replace("\n         ", "")
    );
    let UnitSystemError::InvalidUnitSelections(selections) = invalid else {
        panic!("unexpected error kind: {invalid:?}");
    };
    assert_eq!(selections.len(), 8);
    assert_eq!(
        selections[0].property,
        UnitSystemProperty::AccumulatedPrecipitation
    );
    assert_eq!(selections[7].property, UnitSystemProperty::Pressure);

    // Precipitation uses the length selection domain; temperature uses the
    // narrower two-unit UnitSystem domain, even though the converter handles K.
    let custom = UnitSystem::try_new(
        "long-distance-rain",
        UnitSystemUnitNames {
            accumulated_precipitation: "mi",
            area: "m²",
            length: "km",
            mass: "g",
            pressure: "Pa",
            temperature: "°C",
            volume: "L",
            wind_speed: "m/s",
        },
        &[],
    )
    .unwrap();
    assert_eq!(custom.accumulated_precipitation_unit(), LengthUnit::Miles);
    assert_eq!(
        custom.get_converted_unit(Some("distance"), Some("ft")),
        None
    );
}

#[test]
fn custom_conversion_table_replaces_builtins_and_preserves_none_keys() {
    let conversions = [
        UnitSystemConversion::new(Some("distance"), Some("ft"), "furlong"),
        UnitSystemConversion::new(Some("distance"), Some("ft"), "last-entry"),
        UnitSystemConversion::new(None, None, "missing-key-result"),
        UnitSystemConversion::new(Some("distance"), None, "missing-unit-result"),
    ];
    let custom = UnitSystem::from_typed("custom", METRIC_UNITS, &conversions);
    assert_eq!(
        custom.get_converted_unit(Some("distance"), Some("ft")),
        Some("last-entry")
    );
    assert_eq!(
        custom.get_converted_unit(None, None),
        Some("missing-key-result")
    );
    assert_eq!(
        custom.get_converted_unit(Some("distance"), None),
        Some("missing-unit-result")
    );
    assert_eq!(
        custom.get_converted_unit(Some("distance"), Some("mi")),
        None
    );
    assert_eq!(custom.get_converted_unit(Some("unknown"), Some("ft")), None);
}

#[test]
fn canonical_conversion_tables_cover_source_device_classes_and_exceptions() {
    let metric_cases = [
        ("atmospheric_pressure", "psi", "hPa"),
        ("area", "ac", "ha"),
        ("distance", "nmi", "km"),
        ("gas", "CCF", "m³"),
        ("precipitation", "in", "mm"),
        ("precipitation_intensity", "in/h", "mm/h"),
        ("pressure", "inHg", "hPa"),
        ("radon", "pCi/L", "Bq/m³"),
        ("speed", "ft/s", "km/h"),
        ("volume", "gal", "L"),
        ("volume_flow_rate", "gal/d", "L/h"),
        ("water", "MCF", "m³"),
        ("wind_speed", "Beaufort", "km/h"),
    ];
    let us_customary_cases = [
        ("atmospheric_pressure", "hPa", "inHg"),
        ("area", "ha", "ac"),
        ("distance", "cm", "in"),
        ("gas", "L", "ft³"),
        ("precipitation", "mm", "in"),
        ("precipitation_intensity", "mm/h", "in/h"),
        ("pressure", "atm", "psi"),
        ("radon", "Bq/m³", "pCi/L"),
        ("speed", "mm/d", "in/d"),
        ("volume", "mL", "fl. oz."),
        ("volume_flow_rate", "m³/s", "ft³/min"),
        ("water", "L", "gal"),
        ("wind_speed", "m/s", "mph"),
    ];

    for (system, cases) in [
        (&METRIC_SYSTEM, &metric_cases[..]),
        (&US_CUSTOMARY_SYSTEM, &us_customary_cases[..]),
    ] {
        for (device_class, original_unit, expected) in cases {
            assert_eq!(
                system.get_converted_unit(Some(device_class), Some(original_unit)),
                Some(*expected),
                "{device_class} {original_unit} in {}",
                system.name()
            );
        }
    }

    assert_eq!(
        METRIC_SYSTEM.get_converted_unit(Some("wind_speed"), Some("kn")),
        None
    );
    assert_eq!(
        US_CUSTOMARY_SYSTEM.get_converted_unit(Some("wind_speed"), Some("kn")),
        None
    );
    assert_eq!(METRIC_SYSTEM.get_converted_unit(None, None), None);
    assert_eq!(
        METRIC_SYSTEM.get_converted_unit(Some("distance"), None),
        None
    );
    assert_eq!(
        METRIC_SYSTEM.get_converted_unit(Some("distance"), Some("unknown")),
        None
    );
}

#[test]
fn canonical_tables_match_every_explicit_source_mapping() {
    let metric_cases = [
        ("area", "in²", "cm²"),
        ("area", "ft²", "m²"),
        ("area", "mi²", "km²"),
        ("area", "yd²", "m²"),
        ("area", "ac", "ha"),
        ("distance", "ft", "m"),
        ("distance", "in", "mm"),
        ("distance", "mi", "km"),
        ("distance", "nmi", "km"),
        ("distance", "yd", "m"),
        ("gas", "CCF", "m³"),
        ("gas", "ft³", "m³"),
        ("gas", "MCF", "m³"),
        ("precipitation", "in", "mm"),
        ("precipitation_intensity", "in/d", "mm/d"),
        ("precipitation_intensity", "in/h", "mm/h"),
        ("pressure", "psi", "kPa"),
        ("pressure", "inHg", "hPa"),
        ("pressure", "inH₂O", "kPa"),
        ("radon", "pCi/L", "Bq/m³"),
        ("speed", "ft/s", "km/h"),
        ("speed", "in/s", "mm/s"),
        ("speed", "mph", "km/h"),
        ("speed", "in/d", "mm/d"),
        ("speed", "in/h", "mm/h"),
        ("volume", "CCF", "m³"),
        ("volume", "ft³", "m³"),
        ("volume", "fl. oz.", "mL"),
        ("volume", "gal", "L"),
        ("volume", "MCF", "m³"),
        ("volume_flow_rate", "ft³/min", "m³/min"),
        ("volume_flow_rate", "gal/min", "L/min"),
        ("volume_flow_rate", "gal/h", "L/h"),
        ("volume_flow_rate", "gal/d", "L/h"),
        ("water", "CCF", "m³"),
        ("water", "ft³", "m³"),
        ("water", "gal", "L"),
        ("water", "MCF", "m³"),
    ];
    let us_customary_cases = [
        ("area", "m²", "ft²"),
        ("area", "cm²", "in²"),
        ("area", "mm²", "in²"),
        ("area", "km²", "mi²"),
        ("area", "ha", "ac"),
        ("distance", "cm", "in"),
        ("distance", "km", "mi"),
        ("distance", "m", "ft"),
        ("distance", "mm", "in"),
        ("gas", "m³", "ft³"),
        ("gas", "L", "ft³"),
        ("precipitation", "cm", "in"),
        ("precipitation", "mm", "in"),
        ("precipitation_intensity", "mm/d", "in/d"),
        ("precipitation_intensity", "mm/h", "in/h"),
        ("pressure", "mbar", "psi"),
        ("pressure", "cbar", "psi"),
        ("pressure", "bar", "psi"),
        ("pressure", "mPa", "psi"),
        ("pressure", "Pa", "psi"),
        ("pressure", "hPa", "psi"),
        ("pressure", "kPa", "psi"),
        ("pressure", "mmHg", "inHg"),
        ("pressure", "inH₂O", "psi"),
        ("pressure", "atm", "psi"),
        ("radon", "Bq/m³", "pCi/L"),
        ("speed", "m/min", "in/s"),
        ("speed", "m/s", "mph"),
        ("speed", "mm/s", "in/s"),
        ("speed", "km/h", "mph"),
        ("speed", "mm/d", "in/d"),
        ("speed", "mm/h", "in/h"),
        ("volume", "m³", "ft³"),
        ("volume", "L", "gal"),
        ("volume", "mL", "fl. oz."),
        ("volume_flow_rate", "m³/min", "ft³/min"),
        ("volume_flow_rate", "m³/h", "ft³/min"),
        ("volume_flow_rate", "m³/s", "ft³/min"),
        ("volume_flow_rate", "L/min", "gal/min"),
        ("volume_flow_rate", "L/h", "gal/h"),
        ("volume_flow_rate", "L/s", "gal/min"),
        ("volume_flow_rate", "mL/s", "gal/min"),
        ("water", "m³", "ft³"),
        ("water", "L", "gal"),
    ];

    for (system, cases) in [
        (&METRIC_SYSTEM, &metric_cases[..]),
        (&US_CUSTOMARY_SYSTEM, &us_customary_cases[..]),
    ] {
        for (device_class, original_unit, expected) in cases {
            assert_eq!(
                system.get_converted_unit(Some(device_class), Some(original_unit)),
                Some(*expected),
                "{device_class} {original_unit} in {}",
                system.name()
            );
        }
    }

    let pressure_units = configurable_units(UnitSystemProperty::Pressure);
    for unit in pressure_units {
        let metric = (unit != &"hPa").then_some("hPa");
        let us_customary = (unit != &"inHg").then_some("inHg");
        assert_eq!(
            METRIC_SYSTEM.get_converted_unit(Some("atmospheric_pressure"), Some(unit)),
            metric
        );
        assert_eq!(
            US_CUSTOMARY_SYSTEM.get_converted_unit(Some("atmospheric_pressure"), Some(unit)),
            us_customary
        );
    }
    for unit in [
        "Beaufort", "ft/s", "in/s", "m/min", "m/s", "km/h", "kn", "mph", "mm/s",
    ] {
        let metric = (!matches!(unit, "km/h" | "kn")).then_some("km/h");
        let us_customary = (!matches!(unit, "mph" | "kn")).then_some("mph");
        assert_eq!(
            METRIC_SYSTEM.get_converted_unit(Some("wind_speed"), Some(unit)),
            metric
        );
        assert_eq!(
            US_CUSTOMARY_SYSTEM.get_converted_unit(Some("wind_speed"), Some(unit)),
            us_customary
        );
    }
}

#[test]
fn all_source_conversion_entry_points_use_their_pinned_converter_families() {
    // Metric target choices: Celsius, kilometers, millimeters of rain, square
    // meters, pascals, meters per second, and liters.
    close(METRIC_SYSTEM.temperature(32.0, Some("°F")).unwrap(), 0.0);
    close(METRIC_SYSTEM.length(1.0, Some("mi")).unwrap(), 1.609_344);
    close(
        METRIC_SYSTEM
            .accumulated_precipitation(1.0, Some("in"))
            .unwrap(),
        25.4,
    );
    close(
        METRIC_SYSTEM.area(1.0, Some("ac")).unwrap(),
        4_046.856_422_4,
    );
    close(METRIC_SYSTEM.pressure(1.0, Some("psi")).unwrap(), 6894.757);
    close(
        METRIC_SYSTEM.wind_speed(1.0, Some("mph")).unwrap(),
        0.447_04,
    );
    close(
        METRIC_SYSTEM.volume(1.0, Some("gal")).unwrap(),
        3.785_411_784,
    );

    // US customary target choices: Fahrenheit, miles, inches of rain, square
    // feet, psi, miles per hour, and US gallons.
    close(
        US_CUSTOMARY_SYSTEM.temperature(0.0, Some("°C")).unwrap(),
        32.0,
    );
    close(
        US_CUSTOMARY_SYSTEM.length(1.0, Some("km")).unwrap(),
        0.621_371_192_237_334,
    );
    close(
        US_CUSTOMARY_SYSTEM
            .accumulated_precipitation(25.4, Some("mm"))
            .unwrap(),
        1.0,
    );
    close(
        US_CUSTOMARY_SYSTEM.area(1.0, Some("m²")).unwrap(),
        10.763_910_416_709_722,
    );
    close(
        US_CUSTOMARY_SYSTEM.pressure(101_325.0, Some("Pa")).unwrap(),
        14.695_949_400_392_212,
    );
    close(
        US_CUSTOMARY_SYSTEM.wind_speed(1.0, Some("m/s")).unwrap(),
        2.236_936_292_054_4,
    );
    close(
        US_CUSTOMARY_SYSTEM.volume(1.0, Some("L")).unwrap(),
        0.264_172_052_358_148_4,
    );

    // The source reuses DistanceConverter for accumulated precipitation and
    // applies offsets only for absolute temperatures, not temperature deltas.
    close(
        hass_units::convert(UnitFamily::TemperatureDelta, 32.0, Some("°F"), Some("°C")).unwrap(),
        17.777_777_777_777_78,
    );
    assert_eq!(METRIC_SYSTEM.temperature(17.0, Some("°C")).unwrap(), 17.0);
    assert_eq!(
        METRIC_SYSTEM.length(1.0, Some("km")).unwrap(),
        1.0,
        "same-unit identity bypasses conversion"
    );
}

#[test]
fn missing_source_units_and_converter_errors_remain_distinguishable() {
    let missing = METRIC_SYSTEM.length(1.0, None).unwrap_err();
    assert!(matches!(
        missing,
        ConversionError::UnrecognizedUnit {
            family: UnitFamily::Distance,
            side: UnitSide::From,
            unit: None,
            ..
        }
    ));
    let invalid = METRIC_SYSTEM.length(1.0, Some("lightyear")).unwrap_err();
    assert!(matches!(
        invalid,
        ConversionError::UnrecognizedUnit {
            family: UnitFamily::Distance,
            side: UnitSide::From,
            unit: Some(ref unit),
            ..
        } if unit == "lightyear"
    ));
    assert_ne!(missing, invalid);
}
