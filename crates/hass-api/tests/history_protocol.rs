//! Pinned history boundary, significance and serialization contracts over IO.
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use hass_api::{ApiState, router};
use hass_core::{Context, Event, MemoryStore, Runtime, State, Store};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn http_flags_and_compressed_websocket_history_preserve_boundary_semantics() {
    let store = Arc::new(MemoryStore::default());
    for (updated, changed, value, reading) in [
        (100, 100, "on", 1),
        (200, 100, "on", 2),
        (300, 300, "off", 3),
    ] {
        let context = Context::default();
        let state = State {
            entity_id: "sensor.history_fixture".into(),
            state: value.into(),
            attributes: Map::from_iter([("reading".into(), json!(reading))]),
            last_changed: Utc.timestamp_opt(changed, 0).unwrap(),
            last_updated: Utc.timestamp_opt(updated, 0).unwrap(),
            last_reported: Utc.timestamp_opt(updated, 0).unwrap(),
            context: context.clone(),
        };
        let event = Event {
            event_type: "state_changed".into(),
            data: Map::new(),
            origin: "LOCAL".into(),
            time_fired: state.last_updated,
            context,
        };
        store.put_state(&state, Some(&event)).unwrap();
    }
    let runtime = Runtime::new(store).unwrap();
    let app = router(ApiState {
        runtime,
        access_token: "history-fixture-token".into(),
        config: json!({}),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let start = Utc.timestamp_opt(150, 0).unwrap().to_rfc3339();
    let end = Utc.timestamp_opt(400, 0).unwrap().to_rfc3339();
    let url = format!("{base}/api/history/period/{start}");
    let history: Value = client
        .get(&url)
        .bearer_auth("history-fixture-token")
        .query(&[
            ("filter_entity_id", "sensor.history_fixture"),
            ("end_time", end.as_str()),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history[0].as_array().unwrap().len(), 2);
    assert_eq!(history[0][0]["state"], "on");
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(history[0][0]["last_changed"].as_str().unwrap())
            .unwrap()
            .timestamp(),
        150
    );
    assert_eq!(history[0][0]["attributes"]["reading"], 1);
    assert_eq!(history[0][1]["state"], "off");
    assert!(history[0][0].get("context").is_none());
    let all: Value = client
        .get(&url)
        .bearer_auth("history-fixture-token")
        .query(&[
            ("filter_entity_id", "sensor.history_fixture"),
            ("end_time", end.as_str()),
            ("significant_changes_only", "0"),
            ("skip_initial_state", "1"),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all[0].as_array().unwrap().len(), 2);
    assert_eq!(all[0][0]["attributes"]["reading"], 2);
    let minimal: Value = client
        .get(&url)
        .bearer_auth("history-fixture-token")
        .query(&[
            ("filter_entity_id", "sensor.history_fixture"),
            ("end_time", end.as_str()),
            ("minimal_response", "1"),
            ("no_attributes", "1"),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(minimal[0][0]["attributes"], json!({}));
    assert_eq!(minimal[0][1].as_object().unwrap().len(), 2);
    let (mut ws, _) = connect_async(format!("ws://{address}/api/websocket"))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap();
    ws.send(Message::Text(
        json!({"type":"auth","access_token":"history-fixture-token"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let auth = ws.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(auth.to_text().unwrap()).unwrap()["type"],
        "auth_ok"
    );
    ws.send(Message::Text(json!({"id":1,"type":"history/history_during_period","start_time":start,"end_time":end,"entity_ids":["sensor.history_fixture"],"minimal_response":true,"no_attributes":true}).to_string().into())).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let history: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(history["success"], true);
    assert_eq!(
        history["result"]["sensor.history_fixture"],
        json!([{"s":"on","lu":150.0},{"s":"off","lu":300.0}])
    );
    ws.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
}
