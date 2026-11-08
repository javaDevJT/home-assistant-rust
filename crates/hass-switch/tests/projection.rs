use hass_core::{Context, MemoryStore, Runtime};
use hass_entity::{EntityLifecycle, EntityStatus};
use hass_registry::{EntityUpdate, Registries};
use hass_switch::{SwitchDescriptor, SwitchDeviceClass, SwitchSnapshot};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition did not become true");
}

#[tokio::test]
async fn live_publication_retains_offline_assumed_state_and_scopes_extra_attributes() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
    let descriptor =
        SwitchDescriptor::new("fixture", "live-switch", "outlet").with_unique_id("live-outlet");
    let handle = lifecycle
        .register_provider_batch("live-switch", vec![descriptor.entity_spec()])
        .unwrap()
        .remove(0);
    let id = handle.current_entity_id().unwrap();
    handle
        .publish_publication(
            SwitchSnapshot::unavailable(Some(true))
                .with_assumed_state(true)
                .with_extra_attribute("detail", Some(json!(1)))
                .publication(),
        )
        .unwrap();
    let offline = runtime.get_state(&id).unwrap();
    assert_eq!(offline.state, "unavailable");
    assert_eq!(offline.attributes.get("assumed_state"), Some(&json!(true)));
    assert!(!offline.attributes.contains_key("detail"));

    handle
        .publish_publication(SwitchSnapshot::unavailable(None).publication())
        .unwrap();
    assert!(
        !runtime
            .get_state(&id)
            .unwrap()
            .attributes
            .contains_key("assumed_state")
    );
    handle
        .publish_publication(
            SwitchSnapshot::new(Some(false))
                .with_extra_attribute("assumed_state", Some(json!(true)))
                .publication(),
        )
        .unwrap();
    assert_eq!(
        runtime
            .get_state(&id)
            .unwrap()
            .attributes
            .get("assumed_state"),
        Some(&json!(true))
    );
    handle
        .publish_publication(
            SwitchSnapshot::unavailable(Some(false))
                .with_extra_attribute("assumed_state", Some(json!(true)))
                .publication(),
        )
        .unwrap();
    assert!(
        !runtime
            .get_state(&id)
            .unwrap()
            .attributes
            .contains_key("assumed_state")
    );
    handle.release().unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_preserves_switch_identity_context_overrides_and_status_changes() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let descriptor = SwitchDescriptor::new("fixture", "switch-provider", "entry/outlet")
        .with_unique_id("entry-outlet")
        .with_suggested_object_id("kitchen_outlet")
        .with_name("Kitchen outlet")
        .with_icon("mdi:power-plug")
        .with_device_class(SwitchDeviceClass::Switch);
    let handle = lifecycle
        .register_provider_batch("switch-provider", vec![descriptor.entity_spec()])
        .unwrap()
        .remove(0);
    let original_id = handle.current_entity_id().unwrap();
    let context = Context {
        id: "switch-context".into(),
        parent_id: Some("switch-parent".into()),
        user_id: Some("switch-user".into()),
    };

    handle
        .publish(
            SwitchSnapshot::new(Some(true))
                .with_assumed_state(true)
                .with_context(context.clone())
                .with_extra_attribute("reading", Some(json!(42)))
                .entity_snapshot(),
        )
        .unwrap()
        .unwrap();
    let initial_state = runtime.get_state(&original_id).unwrap();
    assert_eq!(initial_state.state, "on");
    assert_eq!(initial_state.context, context);
    assert_eq!(
        initial_state.attributes.get("device_class"),
        Some(&json!("switch"))
    );
    assert_eq!(
        initial_state.attributes.get("friendly_name"),
        Some(&json!("Kitchen outlet"))
    );
    assert_eq!(
        initial_state.attributes.get("icon"),
        Some(&json!("mdi:power-plug"))
    );
    assert_eq!(
        initial_state.attributes.get("assumed_state"),
        Some(&json!(true))
    );
    assert_eq!(initial_state.attributes.get("reading"), Some(&json!(42)));

    let renamed_id = "switch.renamed_kitchen_outlet";
    registries
        .update_entity(
            &original_id,
            EntityUpdate {
                new_entity_id: Some(renamed_id.into()),
                name: Some(Some("User switch name".into())),
                icon: Some(Some("mdi:toggle-switch".into())),
                device_class: Some(Some("outlet".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state(&original_id).is_none()
            && runtime.get_state(renamed_id).is_some()
            && handle.current_entity_id().as_deref() == Some(renamed_id)
    })
    .await;

    handle
        .publish(
            SwitchSnapshot::new(Some(false))
                .with_context(context.clone())
                .with_extra_attribute("explicit_null", Some(serde_json::Value::Null))
                .entity_snapshot(),
        )
        .unwrap()
        .unwrap();
    let renamed_state = runtime.get_state(renamed_id).unwrap();
    assert_eq!(renamed_state.state, "off");
    assert_eq!(renamed_state.context, context);
    assert_eq!(
        renamed_state.attributes.get("device_class"),
        Some(&json!("outlet"))
    );
    assert_eq!(
        renamed_state.attributes.get("friendly_name"),
        Some(&json!("User switch name"))
    );
    assert_eq!(
        renamed_state.attributes.get("icon"),
        Some(&json!("mdi:toggle-switch"))
    );
    assert_eq!(
        renamed_state.attributes.get("explicit_null"),
        Some(&serde_json::Value::Null)
    );

    registries
        .update_entity(
            renamed_id,
            EntityUpdate {
                disabled_by: Some(Some("user".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime.get_state(renamed_id).is_none()
            && matches!(handle.status(), EntityStatus::Disabled { .. })
    })
    .await;
    assert!(handle.current_entity_id().is_none());
    assert!(
        handle
            .publish(SwitchSnapshot::new(Some(true)).entity_snapshot())
            .unwrap()
            .is_none()
    );

    registries
        .update_entity(
            renamed_id,
            EntityUpdate {
                disabled_by: Some(None),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| {
        runtime
            .get_state(renamed_id)
            .is_some_and(|state| state.state == "off")
            && matches!(handle.status(), EntityStatus::Active { .. })
    })
    .await;

    handle.release().unwrap();
    assert!(matches!(handle.status(), EntityStatus::Released { .. }));
    let released_state = runtime.get_state(renamed_id).unwrap();
    assert_eq!(released_state.state, "unavailable");
    assert_eq!(
        released_state.attributes.get("restored"),
        Some(&json!(true))
    );
    assert_eq!(registries.entities().len(), 1);
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_projects_unknown_and_unavailable_switch_states() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
    let spec = SwitchDescriptor::new("fixture", "switch-availability", "door-switch")
        .with_unique_id("door-switch")
        .with_device_class(SwitchDeviceClass::Switch)
        .entity_spec();
    let handle = lifecycle
        .register_provider_batch("switch-availability", vec![spec])
        .unwrap()
        .remove(0);
    let entity_id = handle.current_entity_id().unwrap();

    handle
        .publish(
            SwitchSnapshot::new(None)
                .with_extra_attribute("detail", Some(json!("unknown value")))
                .entity_snapshot(),
        )
        .unwrap()
        .unwrap();
    let unknown = runtime.get_state(&entity_id).unwrap();
    assert_eq!(unknown.state, "unknown");
    assert_eq!(
        unknown.attributes.get("device_class"),
        Some(&json!("switch"))
    );
    assert_eq!(
        unknown.attributes.get("detail"),
        Some(&json!("unknown value"))
    );

    handle
        .publish(
            SwitchSnapshot::unavailable(None)
                .with_assumed_state(true)
                .with_extra_attribute("detail", Some(json!("not exposed while unavailable")))
                .entity_snapshot(),
        )
        .unwrap()
        .unwrap();
    let unavailable = runtime.get_state(&entity_id).unwrap();
    assert_eq!(unavailable.state, "unavailable");
    assert_eq!(
        unavailable.attributes.get("device_class"),
        Some(&json!("switch"))
    );
    assert!(!unavailable.attributes.contains_key("detail"));
    assert!(!unavailable.attributes.contains_key("assumed_state"));
    lifecycle.shutdown().await.unwrap();
}
