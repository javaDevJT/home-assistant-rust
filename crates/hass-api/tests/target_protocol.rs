use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiFeatures, ApiState, router_with_features};
use hass_core::{Context, MemoryStore, Runtime, ServiceCall, ServiceResult};
use hass_registry::{AreaInput, EntityRegistration, Registries};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn internal_http_and_websocket_services_share_live_registry_target_resolution() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let area = registries
        .create_area(AreaInput {
            name: "Kitchen".into(),
            ..AreaInput::default()
        })
        .unwrap();
    let mut registration = EntityRegistration::new("switch", "fixture", "target-wire-switch");
    registration.suggested_object_id = Some("island".into());
    registration.area_id = Some(area.id.clone());
    let entity = registries.get_or_create_entity(registration).unwrap();
    runtime
        .set_state(
            &entity.entity_id,
            "off",
            Map::new(),
            Context::default(),
            false,
        )
        .unwrap();
    for (service, state) in [("turn_on", "on"), ("turn_off", "off")] {
        let id = entity.entity_id.clone();
        runtime
            .register_entity_service(
                "wire-fixture",
                "switch",
                service,
                json!({}),
                Arc::new(move |candidate| candidate == id),
                Arc::new(move |runtime, call| {
                    let mut changed = Vec::new();
                    for id in call
                        .target
                        .get("entity_id")
                        .or_else(|| call.data.get("entity_id"))
                        .unwrap()
                        .as_array()
                        .unwrap()
                    {
                        changed.push(
                            runtime
                                .set_state(
                                    id.as_str().unwrap(),
                                    state,
                                    Map::new(),
                                    call.context.clone(),
                                    false,
                                )?
                                .state,
                        );
                    }
                    Ok(ServiceResult {
                        changed_states: changed,
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
    }
    let app = router_with_features(
        ApiState {
            runtime: runtime.clone(),
            access_token: "target-fixture-token".into(),
            config: json!({}),
        },
        ApiFeatures {
            registries: Some(registries),
            ..ApiFeatures::default()
        },
        None,
    );
    // This bypasses HTTP entirely, as script/automation actions do. The
    // Runtime hook must resolve the same actual area to the provider entity.
    let result = runtime
        .call_service(ServiceCall {
            domain: "switch".into(),
            service: "turn_on".into(),
            data: Map::new(),
            target: Map::from_iter([("area_id".into(), json!(area.id))]),
            context: Context::default(),
        })
        .unwrap();
    assert_eq!(result.changed_states[0].entity_id, entity.entity_id);
    assert_eq!(runtime.get_state(&entity.entity_id).unwrap().state, "on");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let response = client
        .post(format!("http://{address}/api/services/switch/turn_off"))
        .bearer_auth("target-fixture-token")
        .json(&json!({"target":{"area_id":area.id}}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(runtime.get_state(&entity.entity_id).unwrap().state, "off");
    let response: Value = client
        .post(format!("http://{address}/api/services/switch/turn_on"))
        .bearer_auth("target-fixture-token")
        .json(&json!({"target":{"area_id":"missing-area"}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response, json!([]));
    assert_eq!(runtime.get_state(&entity.entity_id).unwrap().state, "off");
    let (mut ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap();
    ws.send(Message::Text(
        json!({"type":"auth","access_token":"target-fixture-token"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.next().await.unwrap().unwrap();
    ws.send(Message::Text(json!({"id":1,"type":"call_service","domain":"switch","service":"turn_on","target":{"area_id":area.id}}).to_string().into())).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let result: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(
        result["result"]["changed_states"][0]["entity_id"],
        entity.entity_id
    );
    assert_eq!(runtime.get_state(&entity.entity_id).unwrap().state, "on");
    ws.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}
