//! Generated from Home Assistant `homeassistant/generated/usb.py`.
//! Source commit: cbf2c02c9bff331537c19bff69da8dc11c3fbfbf. Do not hand-edit.
use crate::matcher::{USBMatcher, USBMatcherSpec};
pub const CORE_USB_MATCHER_SPECS: &[USBMatcherSpec] = &[
    USBMatcherSpec {
        domain: "enocean",
        vid: Some("0403"),
        pid: Some("6001"),
        serial_number: None,
        manufacturer: Some("*enocean*"),
        description: Some("*usb 300*"),
    },
    USBMatcherSpec {
        domain: "homeassistant_connect_zbt2",
        vid: Some("303A"),
        pid: Some("4001"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zbt-2*"),
    },
    USBMatcherSpec {
        domain: "homeassistant_connect_zbt2",
        vid: Some("303A"),
        pid: Some("831A"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zbt-2*"),
    },
    USBMatcherSpec {
        domain: "homeassistant_sky_connect",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*skyconnect v1.0*"),
    },
    USBMatcherSpec {
        domain: "homeassistant_sky_connect",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*home assistant connect zbt-1*"),
    },
    USBMatcherSpec {
        domain: "insteon",
        vid: Some("10BF"),
        pid: None,
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "modem_callerid",
        vid: Some("0572"),
        pid: Some("1340"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "rainforest_raven",
        vid: Some("0403"),
        pid: Some("8A28"),
        serial_number: None,
        manufacturer: Some("*rainforest*"),
        description: Some("*raven*"),
    },
    USBMatcherSpec {
        domain: "rainforest_raven",
        vid: Some("04B4"),
        pid: Some("0003"),
        serial_number: None,
        manufacturer: Some("*rainforest*"),
        description: Some("*emu-2*"),
    },
    USBMatcherSpec {
        domain: "velbus",
        vid: Some("10CF"),
        pid: Some("0B1B"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "velbus",
        vid: Some("10CF"),
        pid: Some("0516"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "velbus",
        vid: Some("10CF"),
        pid: Some("0517"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "velbus",
        vid: Some("10CF"),
        pid: Some("0518"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*2652*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*slzb-07*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("1A86"),
        pid: Some("55D4"),
        serial_number: None,
        manufacturer: None,
        description: Some("*sonoff*plus*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*sonoff*plus*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*tubeszb*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("1A86"),
        pid: Some("7523"),
        serial_number: None,
        manufacturer: None,
        description: Some("*tubeszb*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("1A86"),
        pid: Some("7523"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zigstar*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("1CF1"),
        pid: Some("0030"),
        serial_number: None,
        manufacturer: None,
        description: Some("*conbee*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("0403"),
        pid: Some("6015"),
        serial_number: None,
        manufacturer: None,
        description: Some("*conbee*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("8A2A"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zigbee*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("0403"),
        pid: Some("6015"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zigate*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*zigate*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("8B34"),
        serial_number: None,
        manufacturer: None,
        description: Some("*bv 2010/10*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*sonoff*max*"),
    },
    USBMatcherSpec {
        domain: "zha",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: None,
        description: Some("*sonoff*lite*mg21*"),
    },
    USBMatcherSpec {
        domain: "zwave_js",
        vid: Some("0658"),
        pid: Some("0200"),
        serial_number: None,
        manufacturer: None,
        description: None,
    },
    USBMatcherSpec {
        domain: "zwave_js",
        vid: Some("10C4"),
        pid: Some("8A2A"),
        serial_number: None,
        manufacturer: None,
        description: Some("*z-wave*"),
    },
    USBMatcherSpec {
        domain: "zwave_js",
        vid: Some("303A"),
        pid: Some("4001"),
        serial_number: None,
        manufacturer: Some("nabu casa"),
        description: Some("*nabu casa zwa-2*"),
    },
    USBMatcherSpec {
        domain: "zwave_js",
        vid: Some("10C4"),
        pid: Some("EA60"),
        serial_number: None,
        manufacturer: Some("sonoff"),
        description: Some("*sonoff*zwave*"),
    },
];
pub const SERIAL_PORT_SELECTOR_INTEGRATIONS: &[&str] = &[
    "aerosmart",
    "denon_rs232",
    "dsmr",
    "edl21",
    "flexit",
    "kaco_rs485",
    "landisgyr_heat_meter",
    "lg_tv_rs232",
    "marantz_rs232",
    "monoprice",
    "neopool",
    "russound_rio",
    "samsung_exlink",
    "sofar",
    "solaredge_modbus",
    "tonewinner",
    "upb",
    "velbus",
    "zha",
];

/// Core integrations selected by the pinned config-entry consumer metadata:
/// manifest dependency/after-dependency on `usb`, plus the source's four
/// explicit non-USB serial domains.
///
/// Source commit: cbf2c02c9bff331537c19bff69da8dc11c3fbfbf.
pub const SERIAL_CONFIG_ENTRY_DOMAINS: &[&str] = &[
    "aerosmart",
    "alarmdecoder",
    "aurora_abb_powerone",
    "bluetooth",
    "bryant_evolution",
    "default_config",
    "denon_rs232",
    "dsmr",
    "edl21",
    "elkm1",
    "enocean",
    "esphome",
    "flexit",
    "homeassistant_connect_zbt2",
    "homeassistant_hardware",
    "homeassistant_sky_connect",
    "homeassistant_yellow",
    "insteon",
    "kaco_rs485",
    "landisgyr_heat_meter",
    "lg_tv_rs232",
    "marantz_rs232",
    "modem_callerid",
    "monoprice",
    "mysensors",
    "neopool",
    "rainforest_raven",
    "rfxtrx",
    "route_b_smart_meter",
    "russound_rio",
    "samsung_exlink",
    "sofar",
    "solaredge_modbus",
    "teleinfo",
    "tonewinner",
    "upb",
    "velbus",
    "zha",
    "zwave_js",
];

/// Explicit serial integrations that do not declare a `usb` dependency.
pub const NON_USB_SERIAL_DOMAINS: &[&str] =
    &["alarmdecoder", "bryant_evolution", "elkm1", "mysensors"];

/// Whether a pinned core domain can be configured with a serial port.
pub fn config_entry_uses_serial_port(domain: &str) -> bool {
    SERIAL_CONFIG_ENTRY_DOMAINS.binary_search(&domain).is_ok()
}

/// Apply the same rule to a runtime/custom integration's manifest metadata.
pub fn integration_uses_serial_port(
    domain: &str,
    dependencies: &[&str],
    after_dependencies: &[&str],
) -> bool {
    NON_USB_SERIAL_DOMAINS.contains(&domain)
        || dependencies.contains(&"usb")
        || after_dependencies.contains(&"usb")
}
pub fn pinned_usb_matchers() -> Vec<USBMatcher> {
    CORE_USB_MATCHER_SPECS
        .iter()
        .copied()
        .map(USBMatcherSpec::to_matcher)
        .collect()
}
