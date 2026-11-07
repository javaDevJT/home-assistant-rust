use super::*;

use chrono::{DateTime, Utc};
use hass_core::{
    Event, EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, State, Store,
};
use serde_json::{Map, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tracing_subscriber::prelude::*;

fn catalog_provider(catalog: IntegrationCatalog) -> IntegrationCatalogProvider {
    Arc::new(move || catalog.clone())
}

fn default_catalog() -> IntegrationCatalogProvider {
    catalog_provider(IntegrationCatalog::new())
}

fn service_call(runtime: &Runtime, service: &str, data: Value) -> Result<ServiceResult> {
    let data = data
        .as_object()
        .cloned()
        .ok_or_else(|| Error::InvalidInput("test service data must be an object".into()))?;
    runtime.call_service(ServiceCall {
        domain: DOMAIN.into(),
        service: service.into(),
        data,
        target: Map::new(),
        context: Context::default(),
    })
}

fn test_event(level: LoggerLevel, message: &str) -> EventRecord {
    EventRecord {
        target: "test.logger".into(),
        message: message.into(),
        level,
        severity_number: level.number(),
        file: Some("tests/system_log.rs".into()),
        line: Some(42),
        exception: String::new(),
        fields: BTreeMap::new(),
        spans: Vec::new(),
    }
}

#[test]
fn config_matches_pinned_scalar_validation() {
    let defaults: SystemLogConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(defaults.max_entries, DEFAULT_MAX_ENTRIES);
    assert!(!defaults.fire_event);

    let coerced: SystemLogConfig =
        serde_json::from_value(json!({"max_entries": 3.9, "fire_event": " YES "})).unwrap();
    assert_eq!(coerced.max_entries, 3);
    assert!(coerced.fire_event);

    let zero: SystemLogConfig =
        serde_json::from_value(json!({"max_entries": 0, "fire_event": 0})).unwrap();
    assert_eq!(zero.max_entries, 0);
    assert!(!zero.fire_event);
    validate_config(&zero).unwrap();

    let boolean_integer: SystemLogConfig =
        serde_json::from_value(json!({"max_entries": true, "fire_event": false})).unwrap();
    assert_eq!(boolean_integer.max_entries, 1);

    for invalid in [
        json!({"max_entries": -1}),
        json!({"max_entries": null}),
        json!({"fire_event": null}),
        json!({"fire_event": "maybe"}),
        json!({"unknown_inner_key": true}),
    ] {
        assert!(serde_json::from_value::<SystemLogConfig>(invalid).is_err());
    }
}

#[test]
fn event_queue_overflow_is_observable_without_blocking_capture() {
    let (sender, _receiver) = mpsc::channel(1);
    sender
        .try_send(SystemLogEntry {
            name: "full".into(),
            message: vec!["already queued".into()],
            level: "WARNING".into(),
            source: ("test.rs".into(), 1),
            timestamp: 0.0,
            exception: String::new(),
            count: 1,
            first_occurred: 0.0,
        })
        .unwrap();
    let inner = Arc::new(ManagerInner {
        config: SystemLogConfig {
            max_entries: 50,
            fire_event: true,
        },
        catalog: default_catalog(),
        active: AtomicBool::new(true),
        errors: Mutex::new(ErrorStore::default()),
        raw: Arc::new(RawStream::default()),
        reader: Mutex::new(None),
        event_tx: Mutex::new(Some(sender)),
        event_queue_dropped: AtomicU64::new(0),
        event_publish_failed: AtomicU64::new(0),
        capture_gate: RwLock::new(()),
    });
    let manager = SystemLogManager {
        inner: Arc::downgrade(&inner),
    };

    inner.capture(
        &test_event(LoggerLevel::Warning, "overflow fixture"),
        UNIX_EPOCH,
    );

    assert_eq!(
        manager.event_delivery_status().unwrap(),
        EventDeliveryStatus {
            queue_dropped: 1,
            publish_failed: 0,
        }
    );
    assert_eq!(manager.list().unwrap().len(), 1);
}

#[tokio::test]
async fn write_service_coerces_scalars_and_deduplicates_shared_callsite() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let integration = SystemLogIntegration::start(
        runtime.clone(),
        SystemLogConfig::default(),
        default_catalog(),
    )
    .unwrap();
    let manager = integration.manager();
    let mut raw = manager.subscribe_raw(None).unwrap();
    assert!(raw.initial().await.unwrap().is_empty());
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(integration.layer()));

    tracing::dispatcher::with_default(&dispatch, || {
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": true, "level": "warning", "logger": false}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": 2.5, "level": "critical", "logger": ""}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": "critical first", "level": "critical", "logger": "shared"}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": "warning second", "level": "warning", "logger": "shared"}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": 8, "level": "debug", "logger": "demo"}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": 3, "level": "info"}),
        )?;
        Ok::<_, Error>(())
    })
    .unwrap();

    let entries = manager.list().unwrap();
    let warning = entries.iter().find(|entry| entry.name == "False").unwrap();
    assert_eq!(warning.message, ["True"]);
    assert_eq!(warning.level, "WARNING");

    let root_logger = entries.iter().find(|entry| entry.name == "root").unwrap();
    assert_eq!(root_logger.message, ["2.5"]);
    assert_eq!(root_logger.level, "CRITICAL");

    let shared = entries.iter().find(|entry| entry.name == "shared").unwrap();
    assert_eq!(shared.count, 2);
    assert_eq!(shared.level, "CRITICAL");
    assert_eq!(shared.message, ["critical first", "warning second"]);

    let batch = tokio::time::timeout(Duration::from_secs(2), raw.next_batch())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(batch.len(), 6);
    assert!(batch.iter().any(|line| line.contains("DEBUG [demo] 8")));
    assert!(
        batch
            .iter()
            .any(|line| line.contains("INFO [homeassistant.components.system_log.external] 3"))
    );
    assert!(
        batch
            .iter()
            .any(|line| line.contains("CRITICAL [root] 2.5"))
    );
    assert!(store.events().unwrap().is_empty());

    drop(raw);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn fire_event_persists_each_occurrence_before_deduplication() {
    let store = Arc::new(MemoryStore::default());
    let runtime = Runtime::new(store.clone()).unwrap();
    let integration = SystemLogIntegration::start(
        runtime.clone(),
        SystemLogConfig {
            max_entries: 50,
            fire_event: true,
        },
        default_catalog(),
    )
    .unwrap();
    let manager = integration.manager();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(integration.layer()));
    tracing::dispatcher::with_default(&dispatch, || {
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": "first", "level": "warning", "logger": "same"}),
        )?;
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": "second", "level": "critical", "logger": "same"}),
        )?;
        Ok::<_, Error>(())
    })
    .unwrap();
    assert_eq!(manager.event_delivery_status().unwrap().queue_dropped, 0);
    assert_eq!(manager.list().unwrap()[0].count, 2);

    integration.shutdown().await.unwrap();
    let events = store.events().unwrap();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event.event_type, EVENT_SYSTEM_LOG);
        assert_eq!(event.data["count"], 1);
        assert_eq!(event.data["name"], "same");
    }
}

#[tokio::test]
async fn write_service_rejects_invalid_fields_but_accepts_empty_logger_name() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let integration = SystemLogIntegration::start(
        runtime.clone(),
        SystemLogConfig::default(),
        default_catalog(),
    )
    .unwrap();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(integration.layer()));

    for invalid in [
        json!({}),
        json!({"message": null}),
        json!({"message": []}),
        json!({"message": {}}),
        json!({"message": "bad", "logger": null}),
        json!({"message": "bad", "logger": []}),
        json!({"message": "bad", "level": "ERROR"}),
        json!({"message": "bad", "level": 2}),
        json!({"message": "bad", "unknown": true}),
    ] {
        assert!(matches!(
            service_call(&runtime, SERVICE_WRITE, invalid),
            Err(Error::InvalidInput(_))
        ));
    }

    tracing::dispatcher::with_default(&dispatch, || {
        service_call(
            &runtime,
            SERVICE_WRITE,
            json!({"message": "root name", "logger": ""}),
        )
    })
    .unwrap();
    assert_eq!(integration.manager().list().unwrap()[0].name, "root");
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_initial_backlog_removes_only_matching_duplicate_occurrences() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let integration =
        SystemLogIntegration::start(runtime, SystemLogConfig::default(), default_catalog())
            .unwrap();
    let manager = integration.manager();
    let line = "2026-11-06T12:00:00.000Z INFO [demo] repeated".to_owned();
    let backlog_line = line.clone();
    manager
        .register_log_reader(Arc::new(move || {
            let backlog = vec![backlog_line.clone()];
            Box::pin(async move { Ok(backlog) })
        }))
        .unwrap();
    let mut subscription = manager.subscribe_raw(None).unwrap();
    let inner = manager.upgrade().unwrap();
    for _ in 0..2 {
        inner.raw.push(RawRecord {
            target: "demo".into(),
            line: line.clone(),
        });
    }
    let initial = subscription.initial().await.unwrap();
    assert_eq!(initial, [line.clone(), line]);

    drop(subscription);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_stream_skips_websocket_debug_echo_but_keeps_trace() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let integration =
        SystemLogIntegration::start(runtime, SystemLogConfig::default(), default_catalog())
            .unwrap();
    let manager = integration.manager();
    let mut subscription = manager.subscribe_raw(None).unwrap();
    assert!(subscription.initial().await.unwrap().is_empty());
    let inner = manager.upgrade().unwrap();

    let mut debug = test_event(LoggerLevel::Debug, "websocket debug echo");
    debug.target = RAW_WEBSOCKET_CONNECTION_LOGGER.into();
    inner.capture(&debug, UNIX_EPOCH);

    let mut trace = debug.clone();
    trace.message = "websocket trace".into();
    trace.severity_number = 5;
    inner.capture(&trace, UNIX_EPOCH);

    let mut api_debug = test_event(LoggerLevel::Debug, "native send debug echo");
    api_debug.target = "tungstenite::protocol".into();
    api_debug.spans.push(RAW_DELIVERY_SPAN.into());
    inner.capture(&api_debug, UNIX_EPOCH);

    let mut api_trace = api_debug.clone();
    api_trace.message = "native send trace echo".into();
    api_trace.severity_number = 5;
    inner.capture(&api_trace, UNIX_EPOCH);

    let mut unrelated_debug = test_event(LoggerLevel::Debug, "unrelated provider debug");
    unrelated_debug.target = "hass_api::subscription".into();
    inner.capture(&unrelated_debug, UNIX_EPOCH);

    let batch = tokio::time::timeout(Duration::from_secs(2), subscription.next_batch())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().any(|line| {
        line.contains(
            "TRACE [homeassistant.components.websocket_api.http.connection] websocket trace",
        )
    }));
    assert!(
        batch
            .iter()
            .any(|line| line.contains("unrelated provider debug"))
    );

    drop(subscription);
    integration.shutdown().await.unwrap();
}

#[test]
fn backlog_filter_changes_match_state_only_for_real_headers() {
    let targets = vec!["homeassistant.components.demo".into()];
    let filtered = filter_backlog(
        vec![
            "2024-01-01 12:00:00.000 WARNING (MainThread) [homeassistant.components.demo] warning"
                .into(),
            "  continuation mentions [unrelated.domain] and stays included".into(),
            "2024-01-01 12:00:01.000 INFO (MainThread) [elsewhere] unrelated".into(),
            "  continuation mentions [homeassistant.components.demo] and stays excluded".into(),
            "2026-11-06T12:00:02.000Z ERROR [homeassistant.components.demo] native error".into(),
        ],
        Some(&targets),
    );
    assert_eq!(filtered.len(), 3);
    assert!(filtered[1].contains("stays included"));
    assert!(filtered[2].contains("native error"));
}

#[tokio::test]
async fn shutdown_reports_event_persistence_failures() {
    let runtime = Runtime::new(Arc::new(FailingEventStore::default())).unwrap();
    let integration = SystemLogIntegration::start(
        runtime,
        SystemLogConfig {
            max_entries: 50,
            fire_event: true,
        },
        default_catalog(),
    )
    .unwrap();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(integration.layer()));
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::warn!(target: "system_log_test::failure", "expected store failure");
    });

    let error = integration.shutdown().await.unwrap_err();
    assert!(error.to_string().contains("failed to publish 1 occurrence"));
}

#[derive(Default)]
struct FailingEventStore {
    inner: MemoryStore,
}

impl Store for FailingEventStore {
    fn load_states(&self) -> Result<Vec<State>> {
        self.inner.load_states()
    }

    fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
        self.inner.put_state(state, event)
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
        self.inner.remove_state(entity_id, event)
    }

    fn append_event(&self, _event: &Event) -> Result<()> {
        Err(Error::Storage("event persistence fixture failure".into()))
    }

    fn get_data(&self, key: &str) -> Result<Option<Value>> {
        self.inner.get_data(key)
    }

    fn set_data(&self, key: &str, value: &Value) -> Result<()> {
        self.inner.set_data(key, value)
    }

    fn delete_data(&self, key: &str) -> Result<bool> {
        self.inner.delete_data(key)
    }

    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>> {
        self.inner.history(entity_id, start, end)
    }

    fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
        self.inner.history_query(query)
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
        self.inner.event_history_query(query)
    }

    fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult> {
        self.inner.purge_before(cutoff)
    }

    fn repack(&self) -> Result<()> {
        self.inner.repack()
    }
}
