//! Native sensor unit validation data from the pinned Home Assistant Core source.
//!
//! A returned slice is the exact `DEVICE_CLASS_UNITS` set for that class. The
//! outer `None` means the source defines no native-unit validation rule for the
//! class; an inner `None` means a unitless value is explicitly allowed.

/// Return the exact pinned native units for a Home Assistant sensor class.
///
/// Classes with no entry in Home Assistant's `DEVICE_CLASS_UNITS` mapping
/// return `None`. A `None` inside a returned slice is an explicitly accepted
/// null unit, not the absence of a rule.
pub(crate) fn native_units(device_class: &str) -> Option<&'static [Option<&'static str>]> {
    match device_class {
        "absolute_humidity" => Some(&[Some("g/m³"), Some("mg/m³")]),
        "apparent_power" => Some(&[Some("VA"), Some("kVA"), Some("mVA")]),
        "aqi" => Some(&[None]),
        "area" => Some(&[
            Some("ac"),
            Some("cm²"),
            Some("ft²"),
            Some("ha"),
            Some("in²"),
            Some("km²"),
            Some("mi²"),
            Some("mm²"),
            Some("m²"),
            Some("yd²"),
        ]),
        "atmospheric_pressure" => Some(&[
            Some("Pa"),
            Some("atm"),
            Some("bar"),
            Some("cbar"),
            Some("hPa"),
            Some("inHg"),
            Some("inH₂O"),
            Some("kPa"),
            Some("mPa"),
            Some("mbar"),
            Some("mmHg"),
            Some("psi"),
        ]),
        "battery" => Some(&[Some("%")]),
        "blood_glucose_concentration" => Some(&[Some("mg/dL"), Some("mmol/L")]),
        "carbon_monoxide" => Some(&[Some("mg/m³"), Some("ppb"), Some("ppm"), Some("μg/m³")]),
        "carbon_dioxide" => Some(&[Some("ppm")]),
        "conductivity" => Some(&[Some("S/cm"), Some("mS/cm"), Some("μS/cm")]),
        "current" => Some(&[Some("A"), Some("mA"), Some("μA")]),
        "data_rate" => Some(&[
            Some("B/s"),
            Some("GB/s"),
            Some("Gbit/s"),
            Some("GiB/s"),
            Some("KiB/s"),
            Some("MB/s"),
            Some("Mbit/s"),
            Some("MiB/s"),
            Some("bit/s"),
            Some("kB/s"),
            Some("kbit/s"),
        ]),
        "data_size" => Some(&[
            Some("B"),
            Some("EB"),
            Some("EiB"),
            Some("GB"),
            Some("Gbit"),
            Some("GiB"),
            Some("KiB"),
            Some("MB"),
            Some("Mbit"),
            Some("MiB"),
            Some("PB"),
            Some("PiB"),
            Some("TB"),
            Some("TiB"),
            Some("YB"),
            Some("YiB"),
            Some("ZB"),
            Some("ZiB"),
            Some("bit"),
            Some("kB"),
            Some("kbit"),
        ]),
        "distance" => Some(&[
            Some("cm"),
            Some("ft"),
            Some("in"),
            Some("km"),
            Some("m"),
            Some("mi"),
            Some("mm"),
            Some("nmi"),
            Some("yd"),
        ]),
        "duration" => Some(&[
            Some("d"),
            Some("h"),
            Some("min"),
            Some("ms"),
            Some("s"),
            Some("μs"),
        ]),
        "energy" => Some(&[
            Some("GJ"),
            Some("GWh"),
            Some("Gcal"),
            Some("J"),
            Some("MJ"),
            Some("MWh"),
            Some("Mcal"),
            Some("TWh"),
            Some("Wh"),
            Some("cal"),
            Some("kJ"),
            Some("kWh"),
            Some("kcal"),
            Some("mWh"),
            Some("thm"),
        ]),
        "energy_distance" => Some(&[
            Some("Wh/km"),
            Some("kWh/100km"),
            Some("km/kWh"),
            Some("mi/kWh"),
        ]),
        "energy_storage" => Some(&[
            Some("GJ"),
            Some("GWh"),
            Some("Gcal"),
            Some("J"),
            Some("MJ"),
            Some("MWh"),
            Some("Mcal"),
            Some("TWh"),
            Some("Wh"),
            Some("cal"),
            Some("kJ"),
            Some("kWh"),
            Some("kcal"),
            Some("mWh"),
            Some("thm"),
        ]),
        "frequency" => Some(&[
            Some("GHz"),
            Some("Hz"),
            Some("MHz"),
            Some("kHz"),
            Some("mHz"),
        ]),
        "gas" => Some(&[Some("CCF"), Some("L"), Some("MCF"), Some("ft³"), Some("m³")]),
        "humidity" => Some(&[Some("%")]),
        "illuminance" => Some(&[Some("lx")]),
        "irradiance" => Some(&[Some("BTU/(h⋅ft²)"), Some("W/m²")]),
        "moisture" => Some(&[Some("%")]),
        "nitrogen_dioxide" => Some(&[Some("ppb"), Some("ppm"), Some("μg/m³")]),
        "nitrogen_monoxide" => Some(&[Some("ppb"), Some("μg/m³")]),
        "nitrous_oxide" => Some(&[Some("μg/m³")]),
        "ozone" => Some(&[Some("ppb"), Some("ppm"), Some("μg/m³")]),
        "ph" => Some(&[None]),
        "pm1" => Some(&[Some("μg/m³")]),
        "pm10" => Some(&[Some("μg/m³")]),
        "pm25" => Some(&[Some("μg/m³")]),
        "pm4" => Some(&[Some("μg/m³")]),
        "power_factor" => Some(&[None, Some("%")]),
        "power" => Some(&[
            Some("GW"),
            Some("MW"),
            Some("TW"),
            Some("W"),
            Some("kW"),
            Some("mW"),
        ]),
        "precipitation" => Some(&[Some("cm"), Some("in"), Some("mm")]),
        "precipitation_intensity" => {
            Some(&[Some("in/d"), Some("in/h"), Some("mm/d"), Some("mm/h")])
        }
        "pressure" => Some(&[
            Some("Pa"),
            Some("atm"),
            Some("bar"),
            Some("cbar"),
            Some("hPa"),
            Some("inHg"),
            Some("inH₂O"),
            Some("kPa"),
            Some("mPa"),
            Some("mbar"),
            Some("mmHg"),
            Some("psi"),
        ]),
        "radon" => Some(&[Some("Bq/m³"), Some("pCi/L")]),
        "reactive_energy" => Some(&[Some("kvarh"), Some("varh")]),
        "reactive_power" => Some(&[Some("kvar"), Some("mvar"), Some("var")]),
        "signal_strength" => Some(&[Some("dB"), Some("dBm")]),
        "sound_pressure" => Some(&[Some("dB"), Some("dBA")]),
        "speed" => Some(&[
            Some("Beaufort"),
            Some("ft/s"),
            Some("in/d"),
            Some("in/h"),
            Some("in/s"),
            Some("km/h"),
            Some("kn"),
            Some("m/min"),
            Some("m/s"),
            Some("mm/d"),
            Some("mm/h"),
            Some("mm/s"),
            Some("mph"),
        ]),
        "sulphur_dioxide" => Some(&[Some("ppb"), Some("μg/m³")]),
        "temperature" => Some(&[Some("K"), Some("°C"), Some("°F")]),
        "temperature_delta" => Some(&[Some("K"), Some("°C"), Some("°F")]),
        "volatile_organic_compounds" => Some(&[Some("mg/m³"), Some("μg/m³")]),
        "volatile_organic_compounds_parts" => Some(&[Some("ppb"), Some("ppm")]),
        "voltage" => Some(&[Some("MV"), Some("V"), Some("kV"), Some("mV"), Some("μV")]),
        "volume" => Some(&[
            Some("CCF"),
            Some("L"),
            Some("MCF"),
            Some("fl. oz."),
            Some("ft³"),
            Some("gal"),
            Some("mL"),
            Some("m³"),
        ]),
        "volume_flow_rate" => Some(&[
            Some("L/h"),
            Some("L/min"),
            Some("L/s"),
            Some("ft³/min"),
            Some("gal/d"),
            Some("gal/h"),
            Some("gal/min"),
            Some("mL/s"),
            Some("m³/h"),
            Some("m³/min"),
            Some("m³/s"),
        ]),
        "volume_storage" => Some(&[
            Some("CCF"),
            Some("L"),
            Some("MCF"),
            Some("fl. oz."),
            Some("ft³"),
            Some("gal"),
            Some("mL"),
            Some("m³"),
        ]),
        "water" => Some(&[
            Some("CCF"),
            Some("L"),
            Some("MCF"),
            Some("ft³"),
            Some("gal"),
            Some("m³"),
        ]),
        "weight" => Some(&[
            Some("g"),
            Some("kg"),
            Some("lb"),
            Some("mg"),
            Some("oz"),
            Some("st"),
            Some("μg"),
        ]),
        "wind_direction" => Some(&[Some("°")]),
        "wind_speed" => Some(&[
            Some("Beaufort"),
            Some("ft/s"),
            Some("in/s"),
            Some("km/h"),
            Some("kn"),
            Some("m/min"),
            Some("m/s"),
            Some("mm/s"),
            Some("mph"),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::native_units;

    // All SensorDeviceClass values at the source pin, in enum declaration order.
    const SOURCE_DEVICE_CLASSES: &[&str] = &[
        "date",
        "enum",
        "timestamp",
        "uptime",
        "absolute_humidity",
        "apparent_power",
        "aqi",
        "area",
        "atmospheric_pressure",
        "battery",
        "blood_glucose_concentration",
        "carbon_monoxide",
        "carbon_dioxide",
        "conductivity",
        "current",
        "data_rate",
        "data_size",
        "distance",
        "duration",
        "energy",
        "energy_distance",
        "energy_storage",
        "frequency",
        "gas",
        "humidity",
        "illuminance",
        "irradiance",
        "moisture",
        "monetary",
        "nitrogen_dioxide",
        "nitrogen_monoxide",
        "nitrous_oxide",
        "ozone",
        "ph",
        "pm1",
        "pm10",
        "pm25",
        "pm4",
        "power_factor",
        "power",
        "precipitation",
        "precipitation_intensity",
        "pressure",
        "radon",
        "reactive_energy",
        "reactive_power",
        "signal_strength",
        "sound_pressure",
        "speed",
        "sulphur_dioxide",
        "temperature",
        "temperature_delta",
        "volatile_organic_compounds",
        "volatile_organic_compounds_parts",
        "voltage",
        "volume",
        "volume_storage",
        "volume_flow_rate",
        "water",
        "weight",
        "wind_direction",
        "wind_speed",
    ];

    const SOURCE_CLASSES_WITHOUT_A_NATIVE_UNIT_RULE: &[&str] =
        &["date", "enum", "timestamp", "uptime", "monetary"];

    fn assert_units(device_class: &str, expected: &[Option<&str>]) {
        assert_eq!(native_units(device_class), Some(expected));
    }

    #[test]
    fn covers_every_pinned_sensor_device_class_and_only_source_unruled_classes() {
        assert_eq!(SOURCE_DEVICE_CLASSES.len(), 62);
        let mut mapped = Vec::new();
        let mut unruled = Vec::new();
        let mut unit_count = 0;
        let mut null_allowed_count = 0;

        for device_class in SOURCE_DEVICE_CLASSES {
            match native_units(device_class) {
                Some(units) => {
                    mapped.push(*device_class);
                    unit_count += units.len();
                    if units.contains(&None) {
                        null_allowed_count += 1;
                    }
                    assert!(!units.is_empty(), "empty rule for {device_class}");
                    assert!(
                        units.windows(2).all(|pair| pair[0] < pair[1]),
                        "units for {device_class} are not unique and canonically sorted"
                    );
                }
                None => unruled.push(*device_class),
            }
        }

        assert_eq!(mapped.len(), 57);
        assert_eq!(unit_count, 270);
        assert_eq!(null_allowed_count, 3);
        assert_eq!(unruled, SOURCE_CLASSES_WITHOUT_A_NATIVE_UNIT_RULE);
        assert_eq!(native_units("not_a_sensor_class"), None);
    }

    #[test]
    fn preserves_explicit_null_rules_and_native_units_distinct_from_converter_units() {
        assert_units("aqi", &[None]);
        assert_units("ph", &[None]);
        assert_units("power_factor", &[None, Some("%")]);

        // Converter VALID_UNITS contains additional options for these classes;
        // native DEVICE_CLASS_UNITS deliberately does not.
        assert_units("absolute_humidity", &[Some("g/m³"), Some("mg/m³")]);
        assert_units(
            "duration",
            &[
                Some("d"),
                Some("h"),
                Some("min"),
                Some("ms"),
                Some("s"),
                Some("μs"),
            ],
        );
        assert_units(
            "power",
            &[
                Some("GW"),
                Some("MW"),
                Some("TW"),
                Some("W"),
                Some("kW"),
                Some("mW"),
            ],
        );
        assert_units("signal_strength", &[Some("dB"), Some("dBm")]);
        assert_units(
            "pressure",
            &[
                Some("Pa"),
                Some("atm"),
                Some("bar"),
                Some("cbar"),
                Some("hPa"),
                Some("inHg"),
                Some("inH₂O"),
                Some("kPa"),
                Some("mPa"),
                Some("mbar"),
                Some("mmHg"),
                Some("psi"),
            ],
        );
    }
}
