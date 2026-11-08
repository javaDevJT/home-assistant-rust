use hass_core::{MemoryStore, Runtime};
use hass_entity::EntityLifecycle;
use hass_light::{LightColorMode, LightDescriptor, LightFeatures, LightSnapshot};
use hass_registry::Registries;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn publication_updates_light_capabilities_while_unavailable() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();

    let registered = LightDescriptor::new("fixture", "live-light", "desk")
        .with_supported_color_modes([LightColorMode::Rgb])
        .with_supported_features(LightFeatures::FLASH);
    let handle = lifecycle
        .register_provider_batch("live-light", vec![registered.try_entity_spec().unwrap()])
        .unwrap()
        .remove(0);
    let entity_id = handle.current_entity_id().unwrap();

    let updated = LightDescriptor::new("fixture", "live-light", "desk")
        .with_supported_color_modes([LightColorMode::ColorTemp, LightColorMode::Hs])
        .with_supported_features(LightFeatures::EFFECT)
        .with_min_color_temp_kelvin(2_200)
        .with_max_color_temp_kelvin(6_500)
        .with_effect_list(["rainbow"]);
    let publication = LightSnapshot::unavailable(None)
        .with_extra_attribute("diagnostic", Some(json!("must be omitted")))
        .project(&updated)
        .unwrap()
        .publication;
    handle.publish_publication(publication).unwrap();

    let state = runtime.get_state(&entity_id).unwrap();
    assert_eq!(state.state, "unavailable");
    assert_eq!(
        state.attributes.get("supported_color_modes"),
        Some(&json!(["color_temp", "hs"]))
    );
    assert_eq!(state.attributes.get("supported_features"), Some(&json!(4)));
    assert_eq!(
        state.attributes.get("min_color_temp_kelvin"),
        Some(&json!(2_200))
    );
    assert_eq!(
        state.attributes.get("max_color_temp_kelvin"),
        Some(&json!(6_500))
    );
    assert_eq!(
        state.attributes.get("effect_list"),
        Some(&json!(["rainbow"]))
    );
    assert!(!state.attributes.contains_key("diagnostic"));

    handle.release().unwrap();
    lifecycle.shutdown().await.unwrap();
}
