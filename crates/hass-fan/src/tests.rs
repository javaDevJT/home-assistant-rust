use super::*;
use hass_core::{Context, Runtime, ServiceCall};
use hass_entity::EntityLifecycle;
use hass_registry::{EntityUpdate, Registries};
use hass_storage::SqliteStore;
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn service_call(domain: &str, service: &str) -> ServiceCall {
    ServiceCall {
        domain: domain.into(),
        service: service.into(),
        data: Map::new(),
        target: Map::new(),
        context: Context::default(),
    }
}

fn descriptor_with_all_features() -> FanDescriptor {
    FanDescriptor::new("fixture", "fan-provider", "ceiling")
        .with_features(FanFeatures::from_bits_retain(63))
        .with_speed_count(4)
        .with_preset_modes(Some(vec!["auto".into(), "eco".into()]))
}

#[test]
fn pinned_feature_bits_services_and_entity_defaults_are_preserved() {
    assert_eq!(FanFeatures::SET_SPEED.bits(), 1);
    assert_eq!(FanFeatures::OSCILLATE.bits(), 2);
    assert_eq!(FanFeatures::DIRECTION.bits(), 4);
    assert_eq!(FanFeatures::PRESET_MODE.bits(), 8);
    assert_eq!(FanFeatures::TURN_OFF.bits(), 16);
    assert_eq!(FanFeatures::TURN_ON.bits(), 32);
    assert!("set_speed".parse::<FanService>().is_err());
    assert_eq!(FAN_DOMAIN, "fan");

    let descriptor = FanDescriptor::new("fixture", "fan-provider", "default");
    let spec = descriptor.entity_spec();
    assert_eq!(spec.domain, FAN_DOMAIN);
    assert_eq!(spec.info.supported_features, 0);
    assert_eq!(spec.info.capabilities, None);
    assert_eq!(descriptor.speed_count(), 100);
    assert_eq!(descriptor.percentage_step().unwrap(), 1.0);

    let snapshot = FanSnapshot::default();
    assert_eq!(snapshot.percentage(), Some(0));
    assert_eq!(snapshot.state(), FanState::Off);
    assert!(snapshot.state_attributes(&descriptor).unwrap().is_empty());
    assert_eq!(
        snapshot
            .entity_snapshot(&descriptor)
            .unwrap()
            .state
            .as_deref(),
        Some("off")
    );
}

#[test]
fn attributes_keep_null_zero_and_capability_gating_distinct() {
    let descriptor = FanDescriptor::new("fixture", "fan-provider", "features")
        .with_features(
            FanFeatures::SET_SPEED
                | FanFeatures::OSCILLATE
                | FanFeatures::DIRECTION
                | FanFeatures::PRESET_MODE,
        )
        .with_speed_count(4)
        .with_preset_modes(Some(vec!["auto".into(), "eco".into()]));
    let snapshot = FanSnapshot::default()
        .with_percentage(None)
        .with_current_direction(None)
        .with_oscillating(None)
        .with_preset_mode(None)
        .with_extra_attribute("provider_extra", Some(json!(1)))
        .with_extra_attribute("explicit_null", Some(Value::Null))
        .with_extra_attribute("removed", None);
    let attrs = snapshot.state_attributes(&descriptor).unwrap();
    assert_eq!(attrs.get(FAN_ATTR_PERCENTAGE), Some(&Value::Null));
    assert_eq!(attrs.get(FAN_ATTR_PERCENTAGE_STEP), Some(&json!(25.0)));
    assert_eq!(attrs.get(FAN_ATTR_DIRECTION), Some(&Value::Null));
    assert_eq!(attrs.get(FAN_ATTR_OSCILLATING), Some(&Value::Null));
    assert_eq!(attrs.get(FAN_ATTR_PRESET_MODE), Some(&Value::Null));
    assert_eq!(attrs.get("provider_extra"), Some(&json!(1)));
    assert_eq!(attrs.get("explicit_null"), Some(&Value::Null));
    assert!(!attrs.contains_key("removed"));

    assert_eq!(FanSnapshot::default().state(), FanState::Off);
    assert_eq!(
        FanSnapshot::default().with_percentage(Some(1)).state(),
        FanState::On
    );
    assert_eq!(
        FanSnapshot::default()
            .with_percentage(Some(0))
            .with_preset_mode(Some("auto".into()))
            .state(),
        FanState::On
    );
    assert_eq!(
        FanSnapshot::default()
            .with_percentage(None)
            .with_preset_mode(None)
            .state(),
        FanState::Off
    );

    let descriptor_without_speed = FanDescriptor::new("fixture", "fan-provider", "direction")
        .with_feature(FanFeatures::DIRECTION);
    let direction_only = FanSnapshot::default()
        .with_current_direction(Some("reverse".into()))
        .state_attributes(&descriptor_without_speed)
        .unwrap();
    assert_eq!(
        direction_only,
        json!({"direction": "reverse"}).as_object().unwrap().clone()
    );
}

#[test]
fn descriptor_publication_carries_provider_capabilities_as_live_metadata() {
    let descriptor = descriptor_with_all_features()
        .with_unique_id("ceiling-17")
        .with_config_entry_id("entry-1")
        .with_name("Ceiling fan")
        .with_icon("mdi:fan")
        .with_suggested_object_id("ceiling_fan");
    let spec = descriptor.entity_spec();
    assert_eq!(spec.unique_id.as_deref(), Some("ceiling-17"));
    assert_eq!(spec.config_entry_id.as_deref(), Some("entry-1"));
    assert_eq!(spec.info.original_name.as_deref(), Some("Ceiling fan"));
    assert_eq!(spec.info.original_icon.as_deref(), Some("mdi:fan"));
    assert_eq!(spec.info.supported_features, 63);
    assert!(
        spec.recording_policy
            .unrecorded_attributes()
            .contains(FAN_ATTR_PRESET_MODES)
    );
    assert_eq!(
        spec.info.capabilities,
        Some(json!({"preset_modes": ["auto", "eco"]}))
    );

    let null_modes = FanDescriptor::new("fixture", "fan-provider", "no-modes")
        .with_feature(FanFeatures::PRESET_MODE)
        .with_preset_modes(None)
        .entity_spec();
    assert_eq!(
        null_modes.info.capabilities,
        Some(json!({"preset_modes": null}))
    );

    let unavailable = FanDescriptor::new("fixture", "fan-provider", "unavailable")
        .with_feature(FanFeatures::SET_SPEED)
        .with_preset_modes(Some(vec!["auto".into()]))
        .publication(
            &FanSnapshot::unavailable()
                .with_percentage(Some(25))
                .with_extra_attribute("volatile", Some(json!(true))),
        )
        .unwrap();
    assert!(!unavailable.snapshot.available);
    assert_eq!(unavailable.snapshot.state.as_deref(), Some("on"));
    assert_eq!(
        unavailable.snapshot.attributes.get("percentage"),
        Some(&json!(25))
    );
    assert_eq!(
        unavailable.snapshot.attributes.get("volatile"),
        Some(&json!(true))
    );
}

#[test]
fn percentage_helpers_match_pinned_integer_floor_and_ceil_boundaries() {
    let speeds = ["low", "medium", "high", "very_high"];
    assert_eq!(
        ordered_list_item_to_percentage(&speeds, &"low").unwrap(),
        25
    );
    assert_eq!(
        ordered_list_item_to_percentage(&speeds, &"medium").unwrap(),
        50
    );
    assert_eq!(
        ordered_list_item_to_percentage(&speeds, &"high").unwrap(),
        75
    );
    assert_eq!(
        ordered_list_item_to_percentage(&speeds, &"very_high").unwrap(),
        100
    );
    assert_eq!(
        *percentage_to_ordered_list_item(&speeds, 25.0).unwrap(),
        "low"
    );
    assert_eq!(
        *percentage_to_ordered_list_item(&speeds, 26.0).unwrap(),
        "medium"
    );
    assert_eq!(
        *percentage_to_ordered_list_item(&speeds, 100.1).unwrap(),
        "very_high"
    );
    assert!(matches!(
        ordered_list_item_to_percentage(&[], &"low"),
        Err(FanError::SpeedItemNotFound)
    ));
    assert!(matches!(
        percentage_to_ordered_list_item::<&str>(&[], 100.0),
        Err(FanError::EmptyOrderedList)
    ));

    assert_eq!(
        ranged_value_to_percentage((1.0, 255.0), 255.0).unwrap(),
        100
    );
    assert_eq!(ranged_value_to_percentage((1.0, 255.0), 127.0).unwrap(), 49);
    assert_eq!(ranged_value_to_percentage((1.0, 255.0), 10.0).unwrap(), 3);
    assert_eq!(ranged_value_to_percentage((1.0, 255.0), 1.0).unwrap(), 0);
    assert_eq!(ranged_value_to_percentage((0.0, 3.0), 0.0).unwrap(), 25);
    assert_eq!(ranged_value_to_percentage((1.0, 5.0), 3.0).unwrap(), 60);
    assert_eq!(ranged_value_to_percentage((1.0, 5.0), 0.0).unwrap(), 0);
    assert_eq!(ranged_value_to_percentage((1.0, 5.0), -0.01).unwrap(), -1);
    assert_eq!(ranged_value_to_percentage((1.0, 1.1), 0.11).unwrap(), 9);
    assert_eq!(
        percentage_to_ranged_value((1.0, 255.0), 50.0).unwrap(),
        127.5
    );
    assert_eq!(percentage_to_ranged_value((1.0, 255.0), 4.0).unwrap(), 10.2);
    assert_eq!(percentage_to_ranged_value((1.0, 5.0), 0.0).unwrap(), 0.0);
    assert_eq!(percentage_to_ranged_value((3.0, 7.0), 0.0).unwrap(), 2.0);
    assert_eq!(
        percentage_to_ranged_value((1.0, 6.0), 16.0).unwrap().ceil(),
        1.0
    );

    assert_eq!(adjust_speed_percentage(Some(50), 4, None, 1).unwrap(), 75);
    assert_eq!(adjust_speed_percentage(Some(50), 4, None, -1).unwrap(), 25);
    assert_eq!(adjust_speed_percentage(None, 4, None, 1).unwrap(), 25);
    assert_eq!(
        adjust_speed_percentage(Some(95), 0, Some(10), 1).unwrap(),
        100
    );
    assert_eq!(
        adjust_speed_percentage(Some(5), 4, Some(10), -1).unwrap(),
        0
    );
    assert_eq!(
        adjust_speed_percentage(Some(50), 4, Some(0), 1).unwrap(),
        50
    );
    assert!(matches!(
        adjust_speed_percentage(Some(50), 0, None, 1),
        Err(FanError::ZeroSpeedCount)
    ));
    assert!(matches!(
        FanDescriptor::new("fixture", "fan-provider", "bad-count")
            .with_speed_count(0)
            .percentage_step(),
        Err(FanError::ZeroSpeedCount)
    ));
}

#[test]
fn reported_power_state_controls_publication_and_toggle_independently_of_speed() {
    let descriptor = descriptor_with_all_features();
    let unknown = FanSnapshot::new()
        .with_percentage(Some(100))
        .with_reported_state(FanState::Unknown);
    assert_eq!(unknown.state(), FanState::Unknown);
    assert_eq!(
        unknown
            .entity_snapshot(&descriptor)
            .unwrap()
            .state
            .as_deref(),
        Some("unknown")
    );
    let on = FanSnapshot::new()
        .with_percentage(None)
        .with_reported_state(FanState::On);
    assert_eq!(on.state(), FanState::On);
    assert_eq!(
        on.entity_snapshot(&descriptor).unwrap().state.as_deref(),
        Some("on")
    );
    assert_eq!(
        FanCommand::Toggle
            .resolve(&descriptor, &on)
            .unwrap()
            .effects,
        vec![FanEffect::TurnOff]
    );
    let off = FanSnapshot::new()
        .with_percentage(Some(100))
        .with_preset_mode(Some("auto".into()))
        .with_reported_state(FanState::Off);
    assert_eq!(off.state(), FanState::Off);
    assert_eq!(
        off.entity_snapshot(&descriptor).unwrap().state.as_deref(),
        Some("off")
    );
    assert_eq!(
        FanCommand::Toggle
            .resolve(&descriptor, &off)
            .unwrap()
            .effects,
        vec![FanEffect::TurnOn {
            percentage: None,
            preset_mode: None
        }]
    );
}

#[test]
fn service_validation_preserves_coercion_requirements_and_target_context() {
    let mut call = service_call(FAN_DOMAIN, "set_percentage");
    call.data.insert("percentage".into(), json!(42.9));
    call.data.insert("entity_id".into(), json!("fan.office"));
    call.target.insert("area_id".into(), json!("office"));
    call.context.id = "fan-service-context".into();
    let request = FanCommand::parse_service_call(&call).unwrap();
    assert_eq!(
        request.command,
        FanCommand::SetPercentage { percentage: 42 }
    );
    assert_eq!(request.target.get("entity_id"), Some(&json!("fan.office")));
    assert_eq!(request.target.get("area_id"), Some(&json!("office")));
    assert_eq!(request.context.id, "fan-service-context");

    let mut call = service_call(FAN_DOMAIN, "increase_speed");
    call.data.insert("percentage_step".into(), json!(" 2 "));
    assert_eq!(
        FanCommand::parse_service_call(&call).unwrap().command,
        FanCommand::IncreaseSpeed {
            percentage_step: Some(2)
        }
    );

    let mut call = service_call(FAN_DOMAIN, "set_percentage");
    call.data.insert("percentage".into(), json!(true));
    assert_eq!(
        FanCommand::parse_service_call(&call).unwrap().command,
        FanCommand::SetPercentage { percentage: 1 }
    );

    let mut call = service_call(FAN_DOMAIN, "turn_on");
    call.data.insert("percentage".into(), json!(0));
    call.data.insert("preset_mode".into(), json!("auto"));
    assert_eq!(
        FanCommand::parse_service_call(&call).unwrap().command,
        FanCommand::TurnOn {
            percentage: Some(0),
            preset_mode: Some("auto".into())
        }
    );

    let mut call = service_call(FAN_DOMAIN, "oscillate");
    call.data.insert("oscillating".into(), json!("on"));
    assert_eq!(
        FanCommand::parse_service_call(&call).unwrap().command,
        FanCommand::Oscillate { oscillating: true }
    );

    let omitted_direction =
        FanCommand::parse_service_call(&service_call(FAN_DOMAIN, "set_direction")).unwrap();
    assert_eq!(
        omitted_direction.command,
        FanCommand::SetDirection { direction: None }
    );

    let mut call = service_call(FAN_DOMAIN, "set_percentage");
    assert!(matches!(
        FanCommand::parse_service_call(&call),
        Err(FanError::MissingArgument("percentage"))
    ));
    call.data.insert("percentage".into(), json!(101));
    assert!(matches!(
        FanCommand::parse_service_call(&call),
        Err(FanError::PercentageOutOfRange { value: 101, .. })
    ));

    assert!(matches!(
        FanCommand::parse_service_call(&service_call("switch", "turn_on")),
        Err(FanError::WrongDomain(domain)) if domain == "switch"
    ));
    assert!(matches!(
        FanCommand::parse_service_call(&service_call(FAN_DOMAIN, "set_speed")),
        Err(FanError::UnknownService(service)) if service == "set_speed"
    ));
    let mut call = service_call(FAN_DOMAIN, "turn_off");
    call.data.insert("bogus".into(), json!(1));
    assert!(matches!(
        FanCommand::parse_service_call(&call),
        Err(FanError::UnknownServiceField(field)) if field == "bogus"
    ));
    let mut call = service_call(FAN_DOMAIN, "turn_off");
    call.target.insert("bogus".into(), json!(1));
    assert!(matches!(
        FanCommand::parse_service_call(&call),
        Err(FanError::UnknownTargetField(field)) if field == "bogus"
    ));
    let mut call = service_call(FAN_DOMAIN, "turn_off");
    call.data.insert("entity_id".into(), json!("fan.office"));
    call.target.insert("entity_id".into(), json!("fan.office"));
    assert_eq!(
        FanCommand::parse_service_call(&call),
        Err(FanError::EntityIdInDataAndTarget)
    );
}

#[test]
fn commands_check_features_presets_and_preserve_source_effect_order() {
    let descriptor = descriptor_with_all_features();
    let off = FanSnapshot::default().with_percentage(Some(0));
    let zero = FanCommand::SetPercentage { percentage: 0 }
        .resolve(&descriptor, &off)
        .unwrap();
    assert_eq!(
        zero.effects,
        [
            FanEffect::TurnOff,
            FanEffect::SetPercentage { percentage: 0 }
        ]
    );

    let on = FanSnapshot::default().with_percentage(Some(50));
    assert_eq!(
        FanCommand::IncreaseSpeed {
            percentage_step: None
        }
        .resolve(&descriptor, &on)
        .unwrap()
        .effects,
        [FanEffect::SetPercentage { percentage: 75 }]
    );
    assert_eq!(
        FanCommand::DecreaseSpeed {
            percentage_step: Some(100)
        }
        .resolve(&descriptor, &on)
        .unwrap()
        .effects,
        [
            FanEffect::TurnOff,
            FanEffect::SetPercentage { percentage: 0 }
        ]
    );

    assert_eq!(
        FanCommand::Toggle
            .resolve(&descriptor, &on)
            .unwrap()
            .effects,
        [FanEffect::TurnOff]
    );
    assert_eq!(
        FanCommand::Toggle
            .resolve(&descriptor, &FanSnapshot::default())
            .unwrap()
            .effects,
        [FanEffect::TurnOn {
            percentage: None,
            preset_mode: None
        }]
    );
    assert_eq!(
        FanCommand::TurnOn {
            percentage: Some(0),
            preset_mode: Some("auto".into())
        }
        .resolve(&descriptor, &off)
        .unwrap()
        .effects,
        [FanEffect::TurnOn {
            percentage: Some(0),
            preset_mode: Some("auto".into())
        }]
    );
    assert_eq!(
        FanCommand::SetDirection {
            direction: Some("custom".into())
        }
        .resolve(&descriptor, &off)
        .unwrap()
        .effects,
        [FanEffect::SetDirection {
            direction: Some("custom".into())
        }]
    );
    assert_eq!(
        FanCommand::SetPresetMode {
            preset_mode: "eco".into()
        }
        .resolve(&descriptor, &off)
        .unwrap()
        .effects,
        [FanEffect::SetPresetMode {
            preset_mode: "eco".into()
        }]
    );
    assert!(matches!(
        FanCommand::SetPresetMode {
            preset_mode: "invalid".into()
        }
        .resolve(&descriptor, &off),
        Err(FanError::InvalidPresetMode { .. })
    ));
    assert!(matches!(
        FanCommand::TurnOn {
            percentage: None,
            preset_mode: Some("invalid".into())
        }
        .resolve(&descriptor, &off),
        Err(FanError::InvalidPresetMode { .. })
    ));
    assert!(matches!(
        FanCommand::SetPercentage { percentage: 101 }.resolve(&descriptor, &off),
        Err(FanError::PercentageOutOfRange { value: 101, .. })
    ));

    let turn_on_only =
        FanDescriptor::new("fixture", "fan-provider", "turn-on").with_feature(FanFeatures::TURN_ON);
    // The service registration gates turn_on on TURN_ON alone; optional fields
    // are validated against the provider's preset list but not separately gated.
    assert!(
        FanCommand::TurnOn {
            percentage: Some(50),
            preset_mode: None
        }
        .resolve(&turn_on_only, &off)
        .is_ok()
    );
    assert!(matches!(
        FanCommand::Toggle.resolve(&turn_on_only, &off),
        Err(FanError::UnsupportedFeature { required: 48, .. })
    ));
    assert!(matches!(
        FanCommand::SetPresetMode {
            preset_mode: "auto".into()
        }
        .resolve(&turn_on_only, &off),
        Err(FanError::UnsupportedFeature { required: 9, .. })
    ));
}

#[tokio::test]
async fn lifecycle_keeps_capabilities_live_scopes_state_and_honors_registry_removal() {
    let runtime = Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let descriptor = descriptor_with_all_features()
        .with_unique_id("ceiling-17")
        .with_suggested_object_id("ceiling_fan")
        .with_name("Ceiling fan")
        .with_icon("mdi:fan");
    let handle = lifecycle
        .register_batch(vec![descriptor.entity_spec()])
        .unwrap()
        .remove(0);
    let entity_id = handle.current_entity_id().unwrap();

    handle
        .publish_publication(
            descriptor
                .publication(
                    &FanSnapshot::default()
                        .with_percentage(Some(50))
                        .with_preset_mode(Some("auto".into()))
                        .with_current_direction(Some("forward".into()))
                        .with_oscillating(Some(false))
                        .with_extra_attribute("temperature", Some(json!(21))),
                )
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    let online = runtime.get_state(&entity_id).unwrap();
    assert_eq!(online.state, "on");
    assert_eq!(online.attributes.get("percentage"), Some(&json!(50)));
    assert_eq!(online.attributes.get("percentage_step"), Some(&json!(25.0)));
    assert_eq!(online.attributes.get("preset_mode"), Some(&json!("auto")));
    assert_eq!(
        online.attributes.get("preset_modes"),
        Some(&json!(["auto", "eco"]))
    );
    assert_eq!(
        online.attributes.get("supported_features"),
        Some(&json!(63))
    );
    assert_eq!(online.attributes.get("temperature"), Some(&json!(21)));

    registries
        .update_entity(
            &entity_id,
            EntityUpdate {
                name: Some(Some("User fan name".into())),
                icon: Some(Some("mdi:fan-alert".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    let entry = registries.entity(&entity_id).unwrap();
    assert_eq!(entry.name.as_deref(), Some("User fan name"));
    assert_eq!(entry.icon.as_deref(), Some("mdi:fan-alert"));

    handle
        .publish_publication(descriptor.publication(&FanSnapshot::default()).unwrap())
        .unwrap()
        .unwrap();
    let overridden = runtime.get_state(&entity_id).unwrap();
    assert_eq!(
        overridden.attributes.get("friendly_name"),
        Some(&json!("User fan name"))
    );
    assert_eq!(
        overridden.attributes.get("icon"),
        Some(&json!("mdi:fan-alert"))
    );

    handle
        .publish_publication(
            descriptor
                .publication(
                    &FanSnapshot::unavailable()
                        .with_percentage(Some(50))
                        .with_preset_mode(Some("auto".into()))
                        .with_extra_attribute("temperature", Some(json!(22))),
                )
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    let offline = runtime.get_state(&entity_id).unwrap();
    assert_eq!(offline.state, "unavailable");
    assert_eq!(
        offline.attributes.get("preset_modes"),
        Some(&json!(["auto", "eco"]))
    );
    assert_eq!(
        offline.attributes.get("supported_features"),
        Some(&json!(63))
    );
    assert!(!offline.attributes.contains_key("percentage"));
    assert!(!offline.attributes.contains_key("preset_mode"));
    assert!(!offline.attributes.contains_key("temperature"));

    let without_preset_capability = descriptor
        .clone()
        .without_feature(FanFeatures::SET_SPEED)
        .without_feature(FanFeatures::PRESET_MODE);
    handle
        .publish_publication(
            without_preset_capability
                .publication(&FanSnapshot::unavailable())
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    let removed_modes = runtime.get_state(&entity_id).unwrap();
    assert!(!removed_modes.attributes.contains_key("preset_modes"));

    let mut changes = handle.changes();
    registries.remove_entity(&entity_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while handle.status().is_active() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("registry removal reaches the provider handle");
    assert!(
        handle
            .publish_publication(descriptor.publication(&FanSnapshot::default()).unwrap())
            .unwrap()
            .is_none()
    );
    assert!(registries.entity(&entity_id).is_none());
    lifecycle.shutdown().await.unwrap();
}
