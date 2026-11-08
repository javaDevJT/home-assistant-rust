use super::*;
use serde_json::json;

#[test]
fn descriptor_projects_pinned_capabilities_and_rejects_invalid_mode_sets() {
    let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
        .with_supported_color_modes([LightColorMode::Hs, LightColorMode::ColorTemp])
        .with_supported_features(LightFeatures::EFFECT | LightFeatures::TRANSITION)
        .with_effect_list(["rainbow", "pulse"]);
    let attributes = descriptor.capability_attributes().unwrap();
    assert_eq!(
        attributes.get(attr::SUPPORTED_COLOR_MODES),
        Some(&json!(["color_temp", "hs"]))
    );
    assert_eq!(
        attributes.get(attr::MIN_COLOR_TEMP_KELVIN),
        Some(&json!(LIGHT_DEFAULT_MIN_KELVIN))
    );
    assert_eq!(
        attributes.get(attr::MAX_COLOR_TEMP_KELVIN),
        Some(&json!(LIGHT_DEFAULT_MAX_KELVIN))
    );
    assert_eq!(
        attributes.get(attr::EFFECT_LIST),
        Some(&json!(["rainbow", "pulse"]))
    );
    let spec = descriptor.try_entity_spec().unwrap();
    assert_eq!(spec.info.supported_features, 36);
    assert!(!attributes.contains_key(attr::SUPPORTED_FEATURES));

    for invalid in [
        vec![],
        vec![LightColorMode::Unknown],
        vec![LightColorMode::OnOff, LightColorMode::Hs],
        vec![LightColorMode::Brightness, LightColorMode::Rgb],
        vec![LightColorMode::White],
    ] {
        assert!(
            LightDescriptor::new("fixture", "invalid", "invalid")
                .with_supported_color_modes(invalid)
                .validate()
                .is_err()
        );
    }
}

#[test]
fn snapshot_projects_native_color_attributes_and_null_inactive_modes() {
    let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
        .with_supported_color_modes([LightColorMode::Hs, LightColorMode::ColorTemp])
        .with_feature(LightFeature::Effect)
        .with_effect_list(["rainbow"]);
    let projected = LightSnapshot::new(Some(true))
        .with_brightness(Some(91))
        .with_color_mode(Some(LightColorMode::Hs))
        .with_hs_color(Some([0.0, 100.0]))
        .with_effect(Some("rainbow"))
        .project(&descriptor)
        .unwrap();

    assert_eq!(projected.snapshot.state.as_deref(), Some("on"));
    assert_eq!(
        projected.snapshot.attributes.get(attr::COLOR_MODE),
        Some(&json!("hs"))
    );
    assert_eq!(
        projected.snapshot.attributes.get(attr::BRIGHTNESS),
        Some(&json!(91))
    );
    assert_eq!(
        projected.snapshot.attributes.get(attr::EFFECT),
        Some(&json!("rainbow"))
    );
    assert_eq!(
        projected.snapshot.attributes.get(attr::RGB_COLOR),
        Some(&json!([255, 0, 0]))
    );
    assert_eq!(
        projected.snapshot.attributes.get(attr::XY_COLOR),
        Some(&json!([0.701, 0.299]))
    );
    assert_eq!(
        projected.snapshot.attributes.get(attr::COLOR_TEMP_KELVIN),
        Some(&Value::Null)
    );

    let unavailable = LightSnapshot::unavailable(Some(true))
        .with_color_mode(Some(LightColorMode::Unknown))
        .project(&descriptor)
        .unwrap();
    assert!(!unavailable.publication.snapshot.available);
    assert_eq!(
        unavailable
            .publication
            .snapshot
            .attributes
            .get(attr::SUPPORTED_COLOR_MODES),
        Some(&json!(["color_temp", "hs"]))
    );
    assert_eq!(
        unavailable
            .publication
            .snapshot
            .attributes
            .get(attr::EFFECT),
        Some(&Value::Null)
    );
}

#[test]
fn state_mode_validation_allows_effect_only_onoff_and_brightness_modes() {
    let descriptor = LightDescriptor::new("fixture", "light-provider", "desk")
        .with_supported_color_modes([LightColorMode::Rgb])
        .with_feature(LightFeature::Effect);
    let effect_brightness = LightSnapshot::new(Some(true))
        .with_color_mode(Some(LightColorMode::Brightness))
        .with_effect(Some("pulse"))
        .project(&descriptor);
    assert!(effect_brightness.is_ok());

    let no_effect = LightSnapshot::new(Some(true))
        .with_color_mode(Some(LightColorMode::Brightness))
        .project(&descriptor);
    assert!(matches!(
        no_effect,
        Err(LightError::UnsupportedColorMode(_))
    ));

    let missing_mode = LightSnapshot::new(Some(true)).project(&descriptor);
    assert_eq!(missing_mode.unwrap_err(), LightError::MissingColorMode);
}
