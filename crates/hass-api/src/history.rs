//! Recorder history serialization follows the pinned HTTP and compressed WS
//! formats, rather than exposing private runtime persistence fields.
use hass_core::State;
use serde_json::{Value, json};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCommand {
    #[serde(rename = "id")]
    _id: u64,
    #[serde(rename = "type")]
    _kind: String,
    start_time: String,
    end_time: Option<String>,
    entity_ids: Vec<String>,
    #[serde(default = "yes")]
    include_start_time_state: bool,
    #[serde(default = "yes")]
    significant_changes_only: bool,
    #[serde(default)]
    minimal_response: bool,
    #[serde(default)]
    no_attributes: bool,
}
fn yes() -> bool {
    true
}

pub(crate) async fn history_during_period(
    state: &crate::AppState,
    identity: &crate::extensions::Identity,
    command: &Value,
) -> hass_core::Result<Value> {
    use chrono::{DateTime, Utc};
    use hass_core::{Error, HistoryQuery};
    let request: HistoryCommand = serde_json::from_value(command.clone())
        .map_err(|_| Error::InvalidInput("Invalid history command".into()))?;
    let parse = |value: &str| {
        DateTime::parse_from_rfc3339(value)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|_| Error::InvalidInput("History timestamp must be RFC 3339".into()))
    };
    let start = parse(&request.start_time)?;
    let end = request
        .end_time
        .as_deref()
        .map(parse)
        .transpose()?
        .unwrap_or_else(Utc::now);
    let query = HistoryQuery {
        entity_ids: Some(request.entity_ids),
        start,
        end,
        include_start_time_state: request.include_start_time_state,
        significant_changes_only: request.significant_changes_only,
        minimal_response: request.minimal_response,
        no_attributes: request.no_attributes,
        exclude_entities: Vec::new(),
    };
    query.validate()?;
    if !identity.can_read_states || start > Utc::now() {
        return Ok(json!({}));
    }
    let runtime = state.api.runtime.clone();
    let series = tokio::task::spawn_blocking(move || runtime.history_query(&query))
        .await
        .map_err(|_| Error::Service("History query task failed".into()))??;
    let result = series
        .into_iter()
        .filter(|series| !series.states.is_empty())
        .map(|series| {
            let states = serialize_series(
                &series.states,
                request.minimal_response,
                request.no_attributes,
                true,
            );
            (series.entity_id, Value::Array(states))
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(Value::Object(result))
}

fn needs_attributes(entity_id: &str) -> bool {
    matches!(
        entity_id.split('.').next(),
        Some("climate" | "humidifier" | "input_datetime" | "thermostat" | "water_heater")
    )
}

pub(crate) fn serialize_series(
    states: &[State],
    minimal: bool,
    no_attributes: bool,
    compressed: bool,
) -> Vec<Value> {
    let mut output = Vec::with_capacity(states.len());
    let mut previous = None;
    for state in states {
        let needs = needs_attributes(&state.entity_id);
        let minimal = minimal && !needs;
        if minimal && previous == Some(state.state.as_str()) {
            continue;
        }
        let abbreviated = minimal && !output.is_empty();
        previous = Some(state.state.as_str());
        let value = if compressed {
            let timestamp = |time: chrono::DateTime<chrono::Utc>| {
                time.timestamp() as f64 + f64::from(time.timestamp_subsec_nanos()) / 1_000_000_000.0
            };
            let mut value = json!({"s":state.state,"lu":timestamp(state.last_updated)});
            if !abbreviated {
                if !no_attributes || needs {
                    value["a"] = json!(state.attributes);
                }
                if state.last_changed != state.last_updated {
                    value["lc"] = json!(timestamp(state.last_changed));
                }
            }
            value
        } else if abbreviated {
            json!({"state":state.state,"last_changed":state.last_updated})
        } else {
            json!({"entity_id":state.entity_id,"state":state.state,
                "attributes":if !no_attributes || needs {json!(state.attributes)} else {json!({})},
                "last_changed":state.last_changed,"last_updated":state.last_updated})
        };
        output.push(value);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use hass_core::Context;
    #[test]
    fn history_shapes_omit_runtime_context_and_compress_only_subsequent_changes() {
        let first = State {
            entity_id: "sensor.fixture".into(),
            state: "1".into(),
            attributes: serde_json::Map::from_iter([("unit_of_measurement".into(), json!("W"))]),
            last_changed: Utc.timestamp_opt(1, 0).unwrap(),
            last_updated: Utc.timestamp_opt(2, 0).unwrap(),
            last_reported: Utc.timestamp_opt(2, 0).unwrap(),
            context: Context::default(),
        };
        let mut second = first.clone();
        second.state = "2".into();
        second.last_changed = Utc.timestamp_opt(3, 0).unwrap();
        second.last_updated = second.last_changed;
        let full = serialize_series(&[first.clone(), second.clone()], false, false, false);
        assert_eq!(full[0].as_object().unwrap().len(), 5);
        assert!(full[0].get("context").is_none());
        let minimal = serialize_series(
            &[first.clone(), first.clone(), second.clone()],
            true,
            true,
            false,
        );
        assert_eq!(minimal.len(), 2);
        assert_eq!(minimal[0]["attributes"], json!({}));
        assert_eq!(minimal[1].as_object().unwrap().len(), 2);
        let compressed = serialize_series(&[first, second], false, true, true);
        assert_eq!(compressed[0], json!({"s":"1","lu":2.0,"lc":1.0}));
        assert_eq!(compressed[1], json!({"s":"2","lu":3.0}));
    }
}
