use hass_climate::attr;
use hass_climate::{
    CURRENT_HVAC_ACTIONS, ClimateCommand, ClimateDescriptor, ClimateEffect, ClimateError,
    ClimateFeature, ClimateFeatures, ClimateService, ClimateSnapshot, ClimateTemperatureUnit,
    ClimateUnitContext, HVAC_MODES, HvacAction, HvacMode,
};
use hass_core::{Context, MemoryStore, Runtime, ServiceCall};
use hass_entity::{EntityLifecycle, EntityStatus};
use hass_registry::{EntityUpdate, Registries};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

fn all_features() -> ClimateFeatures {
    ClimateFeatures::TARGET_TEMPERATURE
        | ClimateFeatures::TARGET_TEMPERATURE_RANGE
        | ClimateFeatures::TARGET_HUMIDITY
        | ClimateFeatures::FAN_MODE
        | ClimateFeatures::PRESET_MODE
        | ClimateFeatures::SWING_MODE
        | ClimateFeatures::TURN_OFF
        | ClimateFeatures::TURN_ON
        | ClimateFeatures::SWING_HORIZONTAL_MODE
}

fn descriptor() -> ClimateDescriptor {
    ClimateDescriptor::new("fixture", "climate-fixture", "thermostat")
        .with_unique_id("thermostat-1")
        .with_name("Provider thermostat")
        .with_supported_features(all_features())
        .with_hvac_modes([
            HvacMode::Off,
            HvacMode::Heat,
            HvacMode::Cool,
            HvacMode::Auto,
        ])
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_temperature_limits(10.0, 30.0)
        .with_humidity_limits(35.0, 80.0)
        .with_target_temperature_step(Some(0.5))
        .with_target_humidity_step(Some(5))
        .with_fan_modes(Some(vec!["auto".into(), "low".into(), "high".into()]))
        .with_preset_modes(Some(vec!["home".into(), "away".into(), "eco".into()]))
        .with_swing_modes(Some(vec!["off".into(), "on".into()]))
        .with_swing_horizontal_modes(Some(vec!["off".into(), "on".into()]))
}

fn context() -> Context {
    Context {
        id: "climate-state-1".into(),
        parent_id: Some("climate-command-1".into()),
        user_id: Some("climate-user-1".into()),
    }
}

fn call(service: &str, data: Map<String, Value>) -> ServiceCall {
    let target = if ["entity_id", "device_id", "area_id", "floor_id", "label_id"]
        .iter()
        .any(|field| data.contains_key(*field))
    {
        Map::new()
    } else {
        Map::from_iter([("entity_id".to_owned(), json!("climate.fixture"))])
    };
    ServiceCall {
        domain: "climate".into(),
        service: service.into(),
        data,
        target,
        context: context(),
    }
}

fn data(fields: &[(&str, Value)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn plan(service: &str, fields: &[(&str, Value)]) -> Result<Vec<ClimateEffect>, ClimateError> {
    let request = ClimateCommand::parse_service_call(&call(service, data(fields)))?;
    Ok(request
        .resolve(&descriptor(), ClimateUnitContext::default())?
        .effects)
}

#[test]
fn pinned_modes_features_and_service_names_are_exposed() {
    assert_eq!(
        HVAC_MODES
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>(),
        [
            "off",
            "heat",
            "cool",
            "heat_cool",
            "auto",
            "dry",
            "fan_only"
        ]
    );
    assert_eq!(
        CURRENT_HVAC_ACTIONS
            .iter()
            .map(|action| action.as_str())
            .collect::<Vec<_>>(),
        [
            "cooling",
            "defrosting",
            "drying",
            "fan",
            "heating",
            "idle",
            "off",
            "preheating"
        ]
    );
    assert_eq!(HvacMode::from_str("heat_cool").unwrap(), HvacMode::HeatCool);
    assert_eq!(
        HvacAction::from_str("preheating").unwrap(),
        HvacAction::Preheating
    );
    assert_eq!(ClimateFeatures::from_bits_retain(64).bits(), 64);
    assert_eq!(ClimateFeatures::TURN_ON.bits(), 256);
    assert_eq!(ClimateFeatures::SWING_HORIZONTAL_MODE.bits(), 512);
    assert_eq!(
        ClimateService::ALL.map(ClimateService::as_str),
        [
            "set_fan_mode",
            "set_humidity",
            "set_hvac_mode",
            "set_preset_mode",
            "set_swing_horizontal_mode",
            "set_swing_mode",
            "set_temperature",
            "toggle",
            "turn_off",
            "turn_on"
        ]
    );
}

#[test]
fn each_registered_service_family_resolves_to_its_provider_effect() {
    let cases = [
        ("turn_on", vec![], ClimateEffect::TurnOn),
        ("turn_off", vec![], ClimateEffect::TurnOff),
        ("toggle", vec![], ClimateEffect::Toggle),
        (
            "set_hvac_mode",
            vec![("hvac_mode", json!("heat"))],
            ClimateEffect::SetHvacMode {
                hvac_mode: HvacMode::Heat,
            },
        ),
        (
            "set_temperature",
            vec![("temperature", json!(22.5)), ("hvac_mode", json!("cool"))],
            ClimateEffect::SetTemperature {
                temperature: Some(22.5),
                target_temp_high: None,
                target_temp_low: None,
                hvac_mode: Some(HvacMode::Cool),
            },
        ),
        (
            "set_temperature",
            vec![
                ("target_temp_low", json!(18)),
                ("target_temp_high", json!(24)),
            ],
            ClimateEffect::SetTemperature {
                temperature: None,
                target_temp_high: Some(24.0),
                target_temp_low: Some(18.0),
                hvac_mode: None,
            },
        ),
        (
            "set_humidity",
            vec![("humidity", json!(50))],
            ClimateEffect::SetHumidity { humidity: 50 },
        ),
        (
            "set_fan_mode",
            vec![("fan_mode", json!("auto"))],
            ClimateEffect::SetFanMode {
                fan_mode: "auto".into(),
            },
        ),
        (
            "set_preset_mode",
            vec![("preset_mode", json!("home"))],
            ClimateEffect::SetPresetMode {
                preset_mode: "home".into(),
            },
        ),
        (
            "set_swing_mode",
            vec![("swing_mode", json!("off"))],
            ClimateEffect::SetSwingMode {
                swing_mode: "off".into(),
            },
        ),
        (
            "set_swing_horizontal_mode",
            vec![("swing_horizontal_mode", json!("on"))],
            ClimateEffect::SetSwingHorizontalMode {
                swing_horizontal_mode: "on".into(),
            },
        ),
    ];
    for (service, fields, expected) in cases {
        assert_eq!(
            plan(service, &fields).unwrap(),
            [expected],
            "service {service}"
        );
    }
}

#[test]
fn parsed_request_carries_selectors_and_causal_context_to_the_plan() {
    let mut call = call("set_hvac_mode", data(&[("hvac_mode", json!("heat"))]));
    call.target
        .insert("entity_id".into(), json!("climate.upstairs"));
    let request = ClimateCommand::parse_service_call(&call).unwrap();
    let plan = request
        .resolve(&descriptor(), ClimateUnitContext::default())
        .unwrap();
    assert_eq!(
        plan.target.get("entity_id"),
        Some(&json!("climate.upstairs"))
    );
    assert_eq!(plan.context, context());
}

#[test]
fn targets_require_positive_selectors_and_preserve_exclusions_without_frontend_metadata() {
    let mut request_call = call("turn_on", Map::new());
    request_call.target.clear();
    assert!(ClimateCommand::parse_service_call(&request_call).is_err());
    request_call.target.insert(
        "exclude".into(),
        json!({"entity_id": ["climate.downstairs"]}),
    );
    assert!(ClimateCommand::parse_service_call(&request_call).is_err());
    request_call.target.insert("area_id".into(), json!([]));
    assert!(ClimateCommand::parse_service_call(&request_call).is_ok());
    request_call
        .target
        .insert("area_id".into(), json!("upstairs"));
    request_call
        .data
        .insert("metadata".into(), json!({"ui": true}));
    let parsed = ClimateCommand::parse_service_call(&request_call).unwrap();
    let plan = parsed
        .resolve(&descriptor(), ClimateUnitContext::default())
        .unwrap();
    assert_eq!(plan.target, request_call.target);
    assert_eq!(plan.context, request_call.context);
    request_call
        .target
        .insert("exclude".into(), json!({"exclude": {}}));
    assert!(matches!(
        ClimateCommand::parse_service_call(&request_call),
        Err(ClimateError::UnknownTargetField(field)) if field == "exclude.exclude"
    ));
    request_call.target.remove("exclude");
    request_call.data.insert("metadata".into(), json!(false));
    assert!(ClimateCommand::parse_service_call(&request_call).is_err());
}

#[test]
fn provider_mode_fields_coerce_json_scalars_with_python_string_spelling() {
    let scalars = [
        (json!(3), "3"),
        (json!(true), "True"),
        (json!(false), "False"),
        (json!(1.0), "1.0"),
        (json!(0.00001), "1e-05"),
        (json!(1e20), "1e+20"),
        (json!(-0.0), "-0.0"),
    ];
    let modes = scalars
        .iter()
        .map(|(_, text)| (*text).to_owned())
        .collect::<Vec<_>>();
    let provider = descriptor()
        .with_fan_modes(Some(modes.clone()))
        .with_preset_modes(Some(modes.clone()))
        .with_swing_modes(Some(modes.clone()))
        .with_swing_horizontal_modes(Some(modes));
    for (service, field) in [
        ("set_fan_mode", "fan_mode"),
        ("set_preset_mode", "preset_mode"),
        ("set_swing_mode", "swing_mode"),
        ("set_swing_horizontal_mode", "swing_horizontal_mode"),
    ] {
        for (value, expected) in &scalars {
            let parsed =
                ClimateCommand::parse_service_call(&call(service, data(&[(field, value.clone())])))
                    .unwrap();
            let plan = parsed
                .resolve(&provider, ClimateUnitContext::default())
                .unwrap();
            let actual = match &plan.effects[0] {
                ClimateEffect::SetFanMode { fan_mode } => fan_mode,
                ClimateEffect::SetPresetMode { preset_mode } => preset_mode,
                ClimateEffect::SetSwingMode { swing_mode } => swing_mode,
                ClimateEffect::SetSwingHorizontalMode {
                    swing_horizontal_mode,
                } => swing_horizontal_mode,
                effect => panic!("unexpected effect: {effect:?}"),
            };
            assert_eq!(actual, expected, "{service}: {value}");
        }
        for value in [Value::Null, json!([]), json!({})] {
            assert!(
                ClimateCommand::parse_service_call(&call(service, data(&[(field, value)])))
                    .is_err()
            );
        }
    }
}

#[test]
fn zero_low_temperature_preserves_source_falsey_range_validation() {
    let provider = descriptor().with_temperature_limits(-10.0, 30.0);
    let parsed = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[
            ("target_temp_low", json!(0.0)),
            ("target_temp_high", json!(-1.0)),
        ]),
    ))
    .unwrap();
    assert!(
        parsed
            .resolve(&provider, ClimateUnitContext::default())
            .is_ok()
    );
    assert!(
        ClimateCommand::parse_service_call(&call(
            "set_temperature",
            data(&[
                ("target_temp_low", json!(1.0)),
                ("target_temp_high", json!(-1.0))
            ]),
        ))
        .unwrap()
        .resolve(&provider, ClimateUnitContext::default())
        .is_err()
    );
}

#[test]
fn entity_spec_retains_registry_identity_features_and_global_unit_capabilities() {
    let descriptor = descriptor();
    let spec = descriptor.try_entity_spec().unwrap();
    assert_eq!(spec.domain, "climate");
    assert_eq!(spec.platform, "fixture");
    assert_eq!(spec.unique_id.as_deref(), Some("thermostat-1"));
    assert_eq!(
        spec.info.original_name.as_deref(),
        Some("Provider thermostat")
    );
    assert_eq!(spec.info.supported_features, all_features().bits());

    let fahrenheit_spec = descriptor
        .entity_spec_with_units(ClimateUnitContext::new(ClimateTemperatureUnit::Fahrenheit))
        .unwrap();
    let capabilities = fahrenheit_spec.info.capabilities.unwrap();
    assert_eq!(capabilities.get(attr::MIN_TEMP), Some(&json!(50.0)));
    assert_eq!(capabilities.get(attr::MAX_TEMP), Some(&json!(86.0)));
    assert_eq!(
        capabilities.get(attr::FAN_MODES),
        Some(&json!(["auto", "low", "high"]))
    );

    let missing_modes = ClimateDescriptor::new("fixture", "climate-fixture", "missing-modes")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .entity_spec_with_units(ClimateUnitContext::default())
        .unwrap();
    assert_eq!(
        missing_modes
            .info
            .capabilities
            .as_ref()
            .unwrap()
            .get(attr::HVAC_MODES),
        Some(&Value::Null)
    );
    let empty_modes = ClimateDescriptor::new("fixture", "climate-fixture", "empty-modes")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_hvac_modes([])
        .entity_spec_with_units(ClimateUnitContext::default())
        .unwrap();
    assert_eq!(
        empty_modes
            .info
            .capabilities
            .as_ref()
            .unwrap()
            .get(attr::HVAC_MODES),
        Some(&json!([]))
    );
}

#[test]
fn schema_rejects_unknown_mixed_missing_and_duplicate_fields_before_planning() {
    let mut mixed = call(
        "set_temperature",
        data(&[
            ("temperature", json!(20)),
            ("target_temp_low", json!(15)),
            ("target_temp_high", json!(25)),
        ]),
    );
    assert!(matches!(
        ClimateCommand::parse_service_call(&mixed),
        Err(ClimateError::InvalidArgument { .. })
    ));

    mixed.data = data(&[("target_temp_low", json!(15))]);
    assert!(matches!(
        ClimateCommand::parse_service_call(&mixed),
        Err(ClimateError::InvalidArgument { .. })
    ));

    mixed.data.clear();
    assert!(matches!(
        ClimateCommand::parse_service_call(&mixed),
        Err(ClimateError::MissingArgument(_))
    ));

    mixed.data = data(&[("temperature", json!(20)), ("effect", json!("heat"))]);
    assert!(matches!(
        ClimateCommand::parse_service_call(&mixed),
        Err(ClimateError::UnknownServiceField(field)) if field == "effect"
    ));

    let mut duplicate_entity = call(
        "turn_on",
        Map::from_iter([("entity_id".into(), json!("climate.one"))]),
    );
    duplicate_entity
        .target
        .insert("entity_id".into(), json!("climate.two"));
    assert_eq!(
        ClimateCommand::parse_service_call(&duplicate_entity).unwrap_err(),
        ClimateError::EntityIdInDataAndTarget
    );

    let mut duplicate_area = call(
        "turn_on",
        Map::from_iter([("area_id".into(), json!("first"))]),
    );
    duplicate_area
        .target
        .insert("area_id".into(), json!("second"));
    assert!(matches!(
        ClimateCommand::parse_service_call(&duplicate_area),
        Err(ClimateError::DuplicateTargetField(field)) if field == "area_id"
    ));

    let mut unknown_target = call("turn_on", Map::new());
    unknown_target
        .target
        .insert("zone_id".into(), json!("zone"));
    assert!(matches!(
        ClimateCommand::parse_service_call(&unknown_target),
        Err(ClimateError::UnknownTargetField(field)) if field == "zone_id"
    ));
}

#[test]
fn feature_gates_and_mode_lists_reject_commands_before_effects() {
    let no_features = ClimateDescriptor::new("fixture", "climate-fixture", "no-features")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_hvac_modes([HvacMode::Heat]);
    let cases = [
        ("turn_on", Map::new(), ClimateFeatures::TURN_ON),
        ("turn_off", Map::new(), ClimateFeatures::TURN_OFF),
        (
            "toggle",
            Map::new(),
            ClimateFeatures::TURN_ON | ClimateFeatures::TURN_OFF,
        ),
        (
            "set_temperature",
            data(&[("temperature", json!(20))]),
            ClimateFeatures::TARGET_TEMPERATURE,
        ),
        (
            "set_temperature",
            data(&[
                ("target_temp_low", json!(18)),
                ("target_temp_high", json!(22)),
            ]),
            ClimateFeatures::TARGET_TEMPERATURE_RANGE,
        ),
        (
            "set_humidity",
            data(&[("humidity", json!(50))]),
            ClimateFeatures::TARGET_HUMIDITY,
        ),
        (
            "set_fan_mode",
            data(&[("fan_mode", json!("auto"))]),
            ClimateFeatures::FAN_MODE,
        ),
        (
            "set_preset_mode",
            data(&[("preset_mode", json!("home"))]),
            ClimateFeatures::PRESET_MODE,
        ),
        (
            "set_swing_mode",
            data(&[("swing_mode", json!("off"))]),
            ClimateFeatures::SWING_MODE,
        ),
        (
            "set_swing_horizontal_mode",
            data(&[("swing_horizontal_mode", json!("off"))]),
            ClimateFeatures::SWING_HORIZONTAL_MODE,
        ),
    ];
    for (service, fields, required) in cases {
        let request = ClimateCommand::parse_service_call(&call(service, fields)).unwrap();
        assert!(matches!(
            request.resolve(&no_features, ClimateUnitContext::default()),
            Err(ClimateError::UnsupportedFeature { required: bits, supported: 0 }) if bits == required.bits()
        ));
    }

    let request = ClimateCommand::parse_service_call(&call(
        "set_hvac_mode",
        data(&[("hvac_mode", json!("cool"))]),
    ))
    .unwrap();
    assert!(matches!(
        request.resolve(&no_features, ClimateUnitContext::default()),
        Err(ClimateError::InvalidMode { family: "hvac", .. })
    ));

    let invalid_preset = ClimateCommand::parse_service_call(&call(
        "set_preset_mode",
        data(&[("preset_mode", json!("sleep"))]),
    ))
    .unwrap();
    let with_preset = no_features
        .clone()
        .with_feature(ClimateFeature::PresetMode)
        .with_preset_modes(Some(vec!["home".into(), "away".into()]));
    assert!(matches!(
        invalid_preset.resolve(&with_preset, ClimateUnitContext::default()),
        Err(ClimateError::InvalidMode {
            family: "preset",
            ..
        })
    ));

    let malformed = ClimateCommand::SetTemperature {
        temperature: Some(20.0),
        target_temp_high: Some(22.0),
        target_temp_low: Some(18.0),
        hvac_mode: None,
    };
    assert!(matches!(
        malformed.resolve(&descriptor(), ClimateUnitContext::default()),
        Err(ClimateError::InvalidArgument { .. })
    ));
}

#[test]
fn each_provider_mode_list_rejects_unadvertised_values() {
    let cases = [
        (
            "set_fan_mode",
            "fan_mode",
            "turbo",
            ClimateFeature::FanMode,
            "fan",
        ),
        (
            "set_preset_mode",
            "preset_mode",
            "sleep",
            ClimateFeature::PresetMode,
            "preset",
        ),
        (
            "set_swing_mode",
            "swing_mode",
            "vertical",
            ClimateFeature::SwingMode,
            "swing",
        ),
        (
            "set_swing_horizontal_mode",
            "swing_horizontal_mode",
            "horizontal",
            ClimateFeature::SwingHorizontalMode,
            "horizontal_swing",
        ),
    ];
    for (service, field, value, feature, family) in cases {
        let mut provider = ClimateDescriptor::new("fixture", "climate-fixture", "list-validation")
            .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
            .with_feature(feature);
        provider = match feature {
            ClimateFeature::FanMode => provider.with_fan_modes(Some(vec!["auto".into()])),
            ClimateFeature::PresetMode => provider.with_preset_modes(Some(vec!["home".into()])),
            ClimateFeature::SwingMode => provider.with_swing_modes(Some(vec!["off".into()])),
            ClimateFeature::SwingHorizontalMode => {
                provider.with_swing_horizontal_modes(Some(vec!["off".into()]))
            }
            _ => unreachable!(),
        };
        let request =
            ClimateCommand::parse_service_call(&call(service, data(&[(field, json!(value))])))
                .unwrap();
        assert!(matches!(
            request.resolve(&provider, ClimateUnitContext::default()),
            Err(ClimateError::InvalidMode { family: actual, .. }) if actual == family
        ));
    }
}

#[test]
fn temperatures_use_shared_conversion_and_source_range_clamping() {
    let descriptor = ClimateDescriptor::new("fixture", "climate-fixture", "fahrenheit-input")
        .with_feature(ClimateFeature::TargetTemperature)
        .with_feature(ClimateFeature::TargetTemperatureRange)
        .with_hvac_modes([HvacMode::Heat])
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_temperature_limits(10.04, 30.0);
    let units = ClimateUnitContext::new(ClimateTemperatureUnit::Fahrenheit);
    let at_shown_min = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(50.0))]),
    ))
    .unwrap()
    .resolve(&descriptor, units)
    .unwrap();
    assert_eq!(
        at_shown_min.effects,
        [ClimateEffect::SetTemperature {
            temperature: Some(10.04),
            target_temp_high: None,
            target_temp_low: None,
            hvac_mode: None,
        }]
    );

    let below_shown_min = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(49.9))]),
    ))
    .unwrap();
    assert!(matches!(
        below_shown_min.resolve(&descriptor, units),
        Err(ClimateError::TemperatureOutOfRange { field, .. }) if field == "temperature"
    ));

    let range = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[
            ("target_temp_low", json!(59.0)),
            ("target_temp_high", json!(77.0)),
        ]),
    ))
    .unwrap()
    .resolve(&descriptor, units)
    .unwrap();
    assert_eq!(
        range.effects,
        [ClimateEffect::SetTemperature {
            temperature: None,
            target_temp_high: Some(25.0),
            target_temp_low: Some(15.0),
            hvac_mode: None,
        }]
    );

    let inverted = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[
            ("target_temp_low", json!(25)),
            ("target_temp_high", json!(20)),
        ]),
    ))
    .unwrap();
    assert_eq!(
        inverted.resolve(&descriptor, units).unwrap_err(),
        ClimateError::InvalidTemperatureRange
    );

    let kelvin = ClimateDescriptor::new("fixture", "climate-fixture", "kelvin")
        .with_feature(ClimateFeature::TargetTemperature)
        .with_native_temperature_unit(ClimateTemperatureUnit::Kelvin)
        .with_temperature_limits(260.0, 310.0);
    let in_kelvin = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(80.0))]),
    ))
    .unwrap()
    .resolve(&kelvin, units)
    .unwrap();
    let ClimateEffect::SetTemperature {
        temperature: Some(value),
        ..
    } = in_kelvin.effects[0]
    else {
        panic!("expected temperature effect")
    };
    assert!((value - 299.81666666666666).abs() < 1e-9);
}

#[test]
fn native_unit_is_required_and_default_bounds_follow_explicit_provider_units() {
    let descriptor = ClimateDescriptor::new("fixture", "climate-fixture", "default-unit")
        .with_feature(ClimateFeature::TargetTemperature);
    let units = ClimateUnitContext::new(ClimateTemperatureUnit::Fahrenheit);
    assert!(matches!(
        descriptor.entity_spec_with_units(units),
        Err(ClimateError::InvalidDescriptor(
            "native_temperature_unit is required"
        ))
    ));
    assert!(matches!(
        descriptor.publication(&ClimateSnapshot::unknown(), units),
        Err(ClimateError::InvalidDescriptor(
            "native_temperature_unit is required"
        ))
    ));
    let request = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(45.0))]),
    ))
    .unwrap();
    assert!(matches!(
        request.resolve(&descriptor, units),
        Err(ClimateError::InvalidDescriptor(
            "native_temperature_unit is required"
        ))
    ));
    let descriptor = descriptor.with_native_temperature_unit(ClimateTemperatureUnit::Fahrenheit);
    let accepted = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(45.0))]),
    ))
    .unwrap()
    .resolve(&descriptor, units)
    .unwrap();
    assert_eq!(
        accepted.effects,
        [ClimateEffect::SetTemperature {
            temperature: Some(45.0),
            target_temp_high: None,
            target_temp_low: None,
            hvac_mode: None,
        }]
    );
    let rejected = ClimateCommand::parse_service_call(&call(
        "set_temperature",
        data(&[("temperature", json!(44.0))]),
    ))
    .unwrap();
    assert!(matches!(
        rejected.resolve(&descriptor, units),
        Err(ClimateError::TemperatureOutOfRange { .. })
    ));

    let capabilities = descriptor
        .publication(&ClimateSnapshot::unknown(), units)
        .unwrap();
    assert_eq!(
        capabilities.snapshot.attributes.get(attr::TEMPERATURE_UNIT),
        Some(&json!("°F"))
    );
}

#[test]
fn humidity_uses_provider_bounds_and_accepts_inclusive_endpoints() {
    for humidity in [35, 80] {
        assert_eq!(
            plan("set_humidity", &[("humidity", json!(humidity))]).unwrap(),
            [ClimateEffect::SetHumidity { humidity }]
        );
    }
    for humidity in [34, 81] {
        let request = ClimateCommand::parse_service_call(&call(
            "set_humidity",
            data(&[("humidity", json!(humidity))]),
        ))
        .unwrap();
        assert!(matches!(
            request.resolve(&descriptor(), ClimateUnitContext::default()),
            Err(ClimateError::HumidityOutOfRange { value, .. }) if value == humidity
        ));
    }
}

#[test]
fn projection_keeps_unknown_off_nullable_and_feature_gated_states_distinct() {
    let units = ClimateUnitContext::default();
    let descriptor = descriptor();
    let unknown = ClimateSnapshot::unknown()
        .with_current_temperature(Some(21.26))
        .with_current_humidity(None)
        .with_target_humidity(None)
        .with_fan_mode(None::<String>)
        .with_hvac_action(None)
        .with_context(context())
        .project(&descriptor, units)
        .unwrap();
    assert_eq!(unknown.snapshot.state, None);
    assert!(unknown.snapshot.available);
    assert_eq!(unknown.snapshot.context, context());
    assert_eq!(
        unknown.snapshot.attributes.get(attr::CURRENT_TEMPERATURE),
        Some(&json!(21.3))
    );
    assert_eq!(
        unknown.snapshot.attributes.get(attr::TEMPERATURE_UNIT),
        Some(&json!("°C"))
    );
    assert_eq!(
        unknown.snapshot.attributes.get(attr::TARGET_HUMIDITY),
        Some(&Value::Null)
    );
    assert_eq!(
        unknown.snapshot.attributes.get(attr::FAN_MODE),
        Some(&Value::Null)
    );
    assert!(
        !unknown
            .snapshot
            .attributes
            .contains_key(attr::CURRENT_HUMIDITY)
    );
    assert!(!unknown.snapshot.attributes.contains_key(attr::HVAC_ACTION));

    let no_current_temperature = ClimateSnapshot::unknown()
        .project(&descriptor, units)
        .unwrap();
    assert_eq!(
        no_current_temperature
            .snapshot
            .attributes
            .get(attr::CURRENT_TEMPERATURE),
        Some(&Value::Null)
    );
    let ties_to_even = ClimateSnapshot::unknown()
        .with_current_temperature(Some(20.25))
        .project(&descriptor, units)
        .unwrap();
    assert_eq!(
        ties_to_even
            .snapshot
            .attributes
            .get(attr::CURRENT_TEMPERATURE),
        Some(&json!(20.2))
    );

    let off = ClimateSnapshot::new(Some(HvacMode::Off))
        .with_hvac_action(Some(HvacAction::Off))
        .project(&descriptor, units)
        .unwrap();
    assert_eq!(off.snapshot.state.as_deref(), Some("off"));
    assert_eq!(
        off.snapshot.attributes.get(attr::HVAC_ACTION),
        Some(&json!("off"))
    );

    let unavailable = ClimateSnapshot::unavailable()
        .with_extra_attribute("provider_diagnostic", Some(json!("transient")))
        .project(&descriptor, units)
        .unwrap();
    assert!(!unavailable.snapshot.available);
    assert_eq!(unavailable.snapshot.state, None);
    assert_eq!(
        unavailable.snapshot.attributes.get("provider_diagnostic"),
        Some(&json!("transient"))
    );

    let no_features = ClimateDescriptor::new("fixture", "climate-fixture", "basic")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_hvac_modes([HvacMode::Heat]);
    let basic = ClimateSnapshot::new(Some(HvacMode::Heat))
        .with_target_temperature(Some(20.0))
        .with_target_humidity(Some(50.0))
        .with_fan_mode(Some("auto"))
        .with_extra_attribute("device_note", Some(json!("ordinary")))
        .project(&no_features, units)
        .unwrap();
    assert_eq!(basic.snapshot.state.as_deref(), Some("heat"));
    assert!(
        !basic
            .snapshot
            .attributes
            .contains_key(attr::TARGET_TEMPERATURE)
    );
    assert!(
        !basic
            .snapshot
            .attributes
            .contains_key(attr::TARGET_HUMIDITY)
    );
    assert!(!basic.snapshot.attributes.contains_key(attr::FAN_MODE));
    assert_eq!(
        basic.snapshot.attributes.get("device_note"),
        Some(&json!("ordinary"))
    );
}

#[tokio::test]
async fn publication_retains_live_capabilities_and_registry_lifecycle_context() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();

    let initial_descriptor = ClimateDescriptor::new("fixture", "climate-fixture", "upstairs")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_unique_id("upstairs-thermostat")
        .with_name("Provider name")
        .with_icon("mdi:thermostat")
        .with_supported_features(ClimateFeatures::FAN_MODE | ClimateFeatures::PRESET_MODE)
        .with_hvac_modes([HvacMode::Off, HvacMode::Heat])
        .with_fan_modes(Some(vec!["auto".into(), "quiet".into()]))
        .with_preset_modes(Some(vec!["home".into(), "away".into()]));
    let handle = lifecycle
        .register_provider_batch(
            "climate-fixture",
            vec![initial_descriptor.try_entity_spec().unwrap()],
        )
        .unwrap()
        .remove(0);
    let first_id = handle.current_entity_id().unwrap();
    let first = ClimateSnapshot::new(Some(HvacMode::Off))
        .with_fan_mode(Some("auto"))
        .with_extra_attribute("ordinary_extra", Some(json!("only while available")))
        .with_context(context());
    handle
        .publish_publication(
            initial_descriptor
                .publication(&first, ClimateUnitContext::default())
                .unwrap(),
        )
        .unwrap();
    let state = runtime.get_state(&first_id).unwrap();
    assert_eq!(state.state, "off");
    assert_eq!(state.context, context());
    assert_eq!(
        state.attributes.get(attr::FAN_MODES),
        Some(&json!(["auto", "quiet"]))
    );
    assert_eq!(
        state.attributes.get("ordinary_extra"),
        Some(&json!("only while available"))
    );

    registries
        .update_entity(
            &first_id,
            EntityUpdate {
                new_entity_id: Some("climate.guest_room".into()),
                name: Some(Some("Registry name".into())),
                icon: Some(Some("mdi:account-thermostat".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state("climate.guest_room").is_some()
            && matches!(handle.status(), EntityStatus::Active { entity_id } if entity_id == "climate.guest_room")
    })
    .await;
    handle
        .publish_publication(
            initial_descriptor
                .publication(
                    &ClimateSnapshot::unavailable().with_context(context()),
                    ClimateUnitContext::default(),
                )
                .unwrap(),
        )
        .unwrap();
    let offline = runtime.get_state("climate.guest_room").unwrap();
    assert_eq!(offline.state, "unavailable");
    assert_eq!(
        offline.attributes.get("friendly_name"),
        Some(&json!("Registry name"))
    );
    assert_eq!(
        offline.attributes.get("icon"),
        Some(&json!("mdi:account-thermostat"))
    );
    assert!(!offline.attributes.contains_key("ordinary_extra"));
    assert_eq!(
        offline.attributes.get(attr::SUPPORTED_FEATURES),
        Some(&json!(24))
    );
    assert_eq!(
        offline.attributes.get(attr::FAN_MODES),
        Some(&json!(["auto", "quiet"]))
    );

    registries
        .update_entity(
            "climate.guest_room",
            EntityUpdate {
                disabled_by: Some(Some("user".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state("climate.guest_room").is_none()
            && matches!(handle.status(), EntityStatus::Disabled { .. })
    })
    .await;
    assert!(
        handle
            .publish_publication(
                initial_descriptor
                    .publication(
                        &ClimateSnapshot::unavailable().with_context(context()),
                        ClimateUnitContext::default()
                    )
                    .unwrap(),
            )
            .unwrap()
            .is_none()
    );

    registries
        .update_entity(
            "climate.guest_room",
            EntityUpdate {
                disabled_by: Some(None),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state("climate.guest_room").is_some()
            && matches!(handle.status(), EntityStatus::Active { .. })
    })
    .await;
    let reenabled = runtime.get_state("climate.guest_room").unwrap();
    assert_eq!(reenabled.state, "unavailable");
    assert_eq!(reenabled.context, context());
    assert_eq!(
        reenabled.attributes.get("friendly_name"),
        Some(&json!("Registry name"))
    );
    assert_eq!(
        reenabled.attributes.get("icon"),
        Some(&json!("mdi:account-thermostat"))
    );
    assert!(!reenabled.attributes.contains_key("ordinary_extra"));
    assert_eq!(
        reenabled.attributes.get(attr::FAN_MODES),
        Some(&json!(["auto", "quiet"]))
    );

    handle.release().unwrap();
    wait_until(|| matches!(handle.status(), EntityStatus::Released { .. })).await;
    let released = runtime.get_state("climate.guest_room").unwrap();
    assert_eq!(released.state, "unavailable");
    assert_eq!(released.context, context());
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn unavailable_publication_removes_disabled_live_capability_but_not_user_metadata() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let with_fan = ClimateDescriptor::new("fixture", "climate-removal", "fan")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_unique_id("fan-capability")
        .with_name("Provider climate")
        .with_feature(ClimateFeature::FanMode)
        .with_hvac_modes([HvacMode::Heat])
        .with_fan_modes(Some(vec!["auto".into()]));
    let handle = lifecycle
        .register_provider_batch("climate-removal", vec![with_fan.try_entity_spec().unwrap()])
        .unwrap()
        .remove(0);
    let entity_id = handle.current_entity_id().unwrap();
    handle
        .publish_publication(
            with_fan
                .publication(
                    &ClimateSnapshot::new(Some(HvacMode::Heat))
                        .with_extra_attribute("diagnostic", Some(json!("online"))),
                    ClimateUnitContext::default(),
                )
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        runtime
            .get_state(&entity_id)
            .unwrap()
            .attributes
            .get(attr::FAN_MODES),
        Some(&json!(["auto"]))
    );

    registries
        .update_entity(
            &entity_id,
            EntityUpdate {
                name: Some(Some("Registry override".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    let without_fan = ClimateDescriptor::new("fixture", "climate-removal", "fan")
        .with_native_temperature_unit(ClimateTemperatureUnit::Celsius)
        .with_unique_id("fan-capability")
        .with_hvac_modes([HvacMode::Heat]);
    handle
        .publish_publication(
            without_fan
                .publication(
                    &ClimateSnapshot::unavailable().with_context(context()),
                    ClimateUnitContext::default(),
                )
                .unwrap(),
        )
        .unwrap();
    let unavailable = runtime.get_state(&entity_id).unwrap();
    assert_eq!(unavailable.state, "unavailable");
    assert_eq!(unavailable.context, context());
    assert!(!unavailable.attributes.contains_key(attr::FAN_MODES));
    assert!(!unavailable.attributes.contains_key("diagnostic"));
    assert!(
        !unavailable
            .attributes
            .contains_key(attr::SUPPORTED_FEATURES)
    );
    assert_eq!(
        unavailable.attributes.get("friendly_name"),
        Some(&json!("Registry override"))
    );

    handle.release().unwrap();
    lifecycle.shutdown().await.unwrap();
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not become true within the bounded wait");
}
