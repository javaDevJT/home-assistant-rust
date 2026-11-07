//! Native capture of emitted Home Assistant log events.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult};
use hass_logger::{
    EventRecord, IntegrationCatalog, IntegrationCatalogProvider, LoggerLevel, event_record,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    Layer,
    fmt::{FormattedFields, format::DefaultFields},
    layer::Context as LayerContext,
    registry::LookupSpan,
};

const DOMAIN: &str = "system_log";
const SERVICE_CLEAR: &str = "clear";
const SERVICE_WRITE: &str = "write";
const EVENT_SYSTEM_LOG: &str = "system_log_event";
const DEFAULT_MAX_ENTRIES: usize = 50;
const DEFAULT_FIRE_EVENT: bool = false;
const MAX_MESSAGES: usize = 5;
const RAW_BACKLOG_LINES: usize = 100;
const RAW_INTEGRATION_BACKLOG_LINES: usize = 5_000;
const RAW_BUFFER_RECORDS: usize = 1_000;
const RAW_COOLDOWN: Duration = Duration::from_millis(500);
const RAW_OWNER: &str = "hass-system-log";
const EVENT_QUEUE_CAPACITY: usize = 256;
const RAW_WEBSOCKET_CONNECTION_LOGGER: &str =
    "homeassistant.components.websocket_api.http.connection";
const RAW_DELIVERY_SPAN: &str = "hass_raw_stream_delivery";
static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);

/// Configuration for the retained error list and optional public event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemLogConfig {
    /// Maximum number of unique warning-and-higher entries retained.
    #[serde(deserialize_with = "deserialize_positive_int")]
    pub max_entries: usize,
    /// Publish each warning-and-higher occurrence on the event bus.
    #[serde(deserialize_with = "deserialize_ha_boolean")]
    pub fire_event: bool,
}

impl Default for SystemLogConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            fire_event: DEFAULT_FIRE_EVENT,
        }
    }
}

/// Validate the configuration without touching a runtime or database.
pub fn validate_config(_config: &SystemLogConfig) -> Result<()> {
    Ok(())
}

fn deserialize_positive_int<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    coerce_positive_int(&value).map_err(serde::de::Error::custom)
}

fn coerce_positive_int(value: &Value) -> std::result::Result<usize, &'static str> {
    let integer = match value {
        Value::Bool(value) => usize::from(*value),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                usize::try_from(integer).map_err(|_| "max_entries must be nonnegative")?
            } else if let Some(integer) = value.as_u64() {
                usize::try_from(integer).map_err(|_| "max_entries exceeds the platform limit")?
            } else {
                let number = value.as_f64().ok_or("max_entries must be an integer")?;
                let truncated = number.trunc();
                if !number.is_finite() || truncated < 0.0 || truncated >= usize::MAX as f64 {
                    return Err("max_entries must be a nonnegative integer");
                }
                truncated as usize
            }
        }
        Value::String(value) => {
            let trimmed = value.trim();
            let bytes = trimmed.as_bytes();
            if bytes.iter().enumerate().any(|(index, byte)| {
                *byte == b'_'
                    && (index == 0
                        || index + 1 == bytes.len()
                        || !bytes[index - 1].is_ascii_digit()
                        || !bytes[index + 1].is_ascii_digit())
            }) {
                return Err("max_entries must be an integer");
            }
            let normalized = trimmed.replace('_', "");
            if normalized.is_empty() {
                return Err("max_entries must be a nonnegative integer");
            }
            let integer = normalized
                .parse::<i128>()
                .map_err(|_| "max_entries must be an integer")?;
            usize::try_from(integer).map_err(|_| "max_entries must be nonnegative")?
        }
        _ => return Err("max_entries must be a nonnegative integer"),
    };
    Ok(integer)
}

fn deserialize_ha_boolean<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    coerce_ha_boolean(&value).map_err(serde::de::Error::custom)
}

fn coerce_ha_boolean(value: &Value) -> std::result::Result<bool, &'static str> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" => Ok(true),
            "0" | "false" | "no" | "off" | "disable" => Ok(false),
            _ => Err("invalid boolean value"),
        },
        Value::Number(value) => value
            .as_f64()
            .map(|number| number != 0.0)
            .ok_or("invalid boolean value"),
        _ => Err("invalid boolean value"),
    }
}

/// Home Assistant severity used by `system_log.write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SystemLogLevel {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl SystemLogLevel {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            "critical" => Ok(Self::Critical),
            _ => Err(Error::InvalidInput(format!(
                "invalid system_log.write level {value:?}"
            ))),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    const fn tracing_level(self) -> Level {
        match self {
            Self::Debug => Level::DEBUG,
            Self::Info => Level::INFO,
            Self::Warning => Level::WARN,
            Self::Error | Self::Critical => Level::ERROR,
        }
    }
}

/// A retained warning-or-higher log row.
///
/// Times intentionally serialize as Unix seconds, and `source` as the same
/// two-item array used by `system_log/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemLogEntry {
    pub name: String,
    pub message: Vec<String>,
    pub level: String,
    pub source: (String, u32),
    pub timestamp: f64,
    pub exception: String,
    pub count: u64,
    pub first_occurred: f64,
}

/// Counts of optional event occurrences that were not persisted.
///
/// Event delivery is intentionally nonblocking on the logging thread. A full
/// queue drops the newest occurrence and increments `queue_dropped`; event
/// persistence failures increment `publish_failed` and are also returned from
/// [`SystemLogIntegration::shutdown`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventDeliveryStatus {
    pub queue_dropped: u64,
    pub publish_failed: u64,
}

/// A reader for the most recent native or persisted console log records.
///
/// The returned strings should each represent one formatted record. The
/// manager applies the global/integration tail limits and integration filter.
pub type LogReaderFuture = Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'static>>;
pub type LogReader = Arc<dyn Fn() -> LogReaderFuture + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    name: String,
    source: (String, u32),
    root_cause: Option<(String, u32, String)>,
}

#[derive(Debug, Clone)]
struct StoredEntry {
    key: DedupKey,
    entry: SystemLogEntry,
}

#[derive(Default)]
struct ErrorStore {
    entries: VecDeque<StoredEntry>,
}

impl ErrorStore {
    fn insert(&mut self, key: DedupKey, entry: SystemLogEntry, max_entries: usize) {
        if let Some(position) = self.entries.iter().position(|stored| stored.key == key) {
            let mut stored = self
                .entries
                .remove(position)
                .expect("entry position was found");
            stored.entry.count += 1;
            stored.entry.timestamp = entry.timestamp;
            if !stored.entry.message.contains(&entry.message[0]) {
                stored.entry.message.push(entry.message[0].clone());
                if stored.entry.message.len() > MAX_MESSAGES {
                    stored.entry.message.remove(0);
                }
            }
            self.entries.push_front(stored);
        } else {
            self.entries.push_front(StoredEntry { key, entry });
        }

        while self.entries.len() > max_entries {
            self.entries.pop_back();
        }
    }
}

struct ManagerInner {
    config: SystemLogConfig,
    catalog: IntegrationCatalogProvider,
    active: AtomicBool,
    errors: Mutex<ErrorStore>,
    raw: Arc<RawStream>,
    reader: Mutex<Option<LogReader>>,
    event_tx: Mutex<Option<mpsc::Sender<SystemLogEntry>>>,
    event_queue_dropped: AtomicU64,
    event_publish_failed: AtomicU64,
    capture_gate: RwLock<()>,
}

impl ManagerInner {
    fn ensure_active(&self) -> Result<()> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(stopped_error())
        }
    }

    fn capture(&self, record: &EventRecord, timestamp: SystemTime) {
        let _capture = self
            .capture_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let timestamp = unix_seconds(timestamp);
        if is_error_level(record.level) {
            let entry = SystemLogEntry {
                name: record.target.clone(),
                message: vec![record.message.clone()],
                level: record.level.as_str().to_owned(),
                source: (
                    record.file.clone().unwrap_or_default(),
                    record.line.unwrap_or(0),
                ),
                timestamp,
                exception: record.exception.clone(),
                count: 1,
                first_occurred: timestamp,
            };
            let key = DedupKey {
                name: entry.name.clone(),
                source: entry.source.clone(),
                // Native tracing records expose a callsite but not a Python
                // traceback root frame. Keep the source key and don't invent
                // one from the message text.
                root_cause: None,
            };
            self.errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, entry.clone(), self.config.max_entries);

            if self.config.fire_event {
                let sender = self
                    .event_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(sender) = sender {
                    // Event bus persistence is moved off the logging thread.
                    // A full queue drops the newest event rather than blocking
                    // all logging; retained rows are still updated above.
                    if sender.try_send(entry).is_err() {
                        self.event_queue_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.event_queue_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if !is_ignored_raw_record(record) {
            let line = record.format_line(timestamp_to_system_time(timestamp));
            self.raw.push(RawRecord {
                target: record.target.clone(),
                line,
            });
        }
    }
}

/// Cloneable weak handle used by API adapters and service tests.
#[derive(Clone)]
pub struct SystemLogManager {
    inner: Weak<ManagerInner>,
}

impl std::fmt::Debug for SystemLogManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemLogManager")
            .field("running", &self.inner.strong_count())
            .finish()
    }
}

impl SystemLogManager {
    fn upgrade(&self) -> Result<Arc<ManagerInner>> {
        let inner = self.inner.upgrade().ok_or_else(stopped_error)?;
        inner.ensure_active()?;
        Ok(inner)
    }

    /// Return a weak tracing layer for the app's existing subscriber.
    pub fn layer(&self) -> SystemLogLayer {
        SystemLogLayer {
            inner: self.inner.clone(),
        }
    }

    /// Read retained rows newest first.
    pub fn list(&self) -> Result<Vec<SystemLogEntry>> {
        let inner = self.upgrade()?;
        let rows = inner
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .iter()
            .map(|stored| stored.entry.clone())
            .collect();
        Ok(rows)
    }

    /// Clear retained rows without clearing raw stream subscriptions.
    pub fn clear(&self) -> Result<()> {
        let inner = self.upgrade()?;
        inner
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .clear();
        Ok(())
    }

    /// Return the current counters for optional event delivery failures.
    pub fn event_delivery_status(&self) -> Result<EventDeliveryStatus> {
        let inner = self.upgrade()?;
        Ok(EventDeliveryStatus {
            queue_dropped: inner.event_queue_dropped.load(Ordering::Relaxed),
            publish_failed: inner.event_publish_failed.load(Ordering::Relaxed),
        })
    }

    /// Start a raw stream subscription before any asynchronous backlog read.
    pub fn subscribe_raw(&self, integration: Option<&str>) -> Result<RawLogSubscription> {
        let inner = self.upgrade()?;
        let targets = integration
            .map(|domain| integration_targets(domain, &(inner.catalog)()))
            .transpose()?;
        let id = inner.raw.next_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(SubscriptionState {
            targets,
            queue: Mutex::new(SubscriberQueue {
                collecting: true,
                initialized: false,
                pending: VecDeque::new(),
                batches: VecDeque::new(),
                queued_records: 0,
                overflowed: false,
                closed: false,
            }),
            notify: Notify::new(),
        });
        {
            let mut raw = inner
                .raw
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            raw.subscribers.insert(id, Arc::downgrade(&state));
        }
        Ok(RawLogSubscription {
            id,
            manager: self.inner.clone(),
            state,
            initial_called: false,
        })
    }

    /// Install a callback used for the initial stream backlog.
    pub fn register_log_reader(&self, reader: LogReader) -> Result<()> {
        let inner = self.upgrade()?;
        *inner
            .reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reader);
        Ok(())
    }
}

/// Additive native subscriber layer. It never owns the app runtime.
#[derive(Clone)]
pub struct SystemLogLayer {
    inner: Weak<ManagerInner>,
}

impl std::fmt::Debug for SystemLogLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemLogLayer")
            .field("running", &self.inner.strong_count())
            .finish()
    }
}

impl<S> Layer<S> for SystemLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, context: LayerContext<'_, S>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        let mut record = event_record(event);
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                let mut text = span.name().to_owned();
                let extensions = span.extensions();
                if let Some(fields) = extensions.get::<FormattedFields<DefaultFields>>()
                    && !fields.is_empty()
                {
                    use std::fmt::Write;
                    let _ = write!(text, "{{{fields}}}");
                }
                record.spans.push(text);
            }
        }
        inner.capture(&record, SystemTime::now());
    }
}

/// Owns services and worker tasks while exposing only weak manager/layer handles.
pub struct SystemLogIntegration {
    runtime: Runtime,
    owner: String,
    inner: Arc<ManagerInner>,
    stop_tx: watch::Sender<bool>,
    raw_worker: Option<JoinHandle<()>>,
    event_worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SystemLogIntegration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemLogIntegration")
            .field("owner", &self.owner)
            .field("running", &self.inner.active.load(Ordering::Acquire))
            .finish()
    }
}

impl SystemLogIntegration {
    /// Register system-log services and start bounded capture workers.
    pub fn start(
        runtime: Runtime,
        config: SystemLogConfig,
        catalog: IntegrationCatalogProvider,
    ) -> Result<Self> {
        validate_config(&config)?;

        let (event_tx, event_rx) = if config.fire_event {
            let (sender, receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let inner = Arc::new(ManagerInner {
            config,
            catalog,
            active: AtomicBool::new(true),
            errors: Mutex::new(ErrorStore::default()),
            raw: Arc::new(RawStream::default()),
            reader: Mutex::new(None),
            event_tx: Mutex::new(event_tx),
            event_queue_dropped: AtomicU64::new(0),
            event_publish_failed: AtomicU64::new(0),
            capture_gate: RwLock::new(()),
        });
        let manager = SystemLogManager {
            inner: Arc::downgrade(&inner),
        };
        let owner = format!(
            "{RAW_OWNER}-{}",
            NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed)
        );

        if let Err(error) = register_services(&runtime, &owner, manager.clone()) {
            inner.active.store(false, Ordering::Release);
            inner
                .event_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            return Err(error);
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let raw_worker = tokio::spawn(run_raw_worker(inner.raw.clone(), stop_rx.clone()));
        let event_worker = event_rx.map(|receiver| {
            tokio::spawn(run_event_worker(
                runtime.clone(),
                inner.clone(),
                receiver,
                stop_rx,
            ))
        });

        Ok(Self {
            runtime,
            owner,
            inner,
            stop_tx,
            raw_worker: Some(raw_worker),
            event_worker,
        })
    }

    /// Return a weak manager for WebSocket and HTTP adapters.
    pub fn manager(&self) -> SystemLogManager {
        SystemLogManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Return a weak layer for composition into the app's existing registry.
    pub fn layer(&self) -> SystemLogLayer {
        SystemLogLayer {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Unregister services and stop capture workers.
    pub async fn shutdown(mut self) -> Result<()> {
        let mut failures = Vec::new();
        if let Err(error) = self.stop() {
            failures.push(error.to_string());
        }
        if let Some(worker) = self.raw_worker.take()
            && let Err(error) = worker.await
        {
            failures.push(format!("raw stream worker failed: {error}"));
        }
        if let Some(worker) = self.event_worker.take()
            && let Err(error) = worker.await
        {
            failures.push(format!("event publisher worker failed: {error}"));
        }
        let queue_dropped = self.inner.event_queue_dropped.load(Ordering::Relaxed);
        let publish_failed = self.inner.event_publish_failed.load(Ordering::Relaxed);
        if queue_dropped > 0 || publish_failed > 0 {
            failures.push(format!(
                "event delivery lost {queue_dropped} queued occurrence(s) and failed to publish {publish_failed} occurrence(s)"
            ));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(format!(
                "system log shutdown: {}",
                failures.join("; ")
            )))
        }
    }

    fn stop(&mut self) -> Result<()> {
        if !self.inner.active.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let _capture = self
            .inner
            .capture_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner
            .event_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        close_stream(&self.inner.raw);
        let _ = self.stop_tx.send(true);
        self.runtime.unregister_owned_services(&self.owner)?;
        Ok(())
    }
}

impl Drop for SystemLogIntegration {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A subscription guard that unregisters itself when dropped.
pub struct RawLogSubscription {
    id: u64,
    manager: Weak<ManagerInner>,
    state: Arc<SubscriptionState>,
    initial_called: bool,
}

impl std::fmt::Debug for RawLogSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawLogSubscription")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl RawLogSubscription {
    /// Read the backlog after live capture is active; live records collected
    /// during the read are appended once after the backlog.
    pub async fn initial(&mut self) -> Result<Vec<String>> {
        if self.initial_called {
            return Err(Error::InvalidInput(
                "raw subscription initial backlog was already read".into(),
            ));
        }
        self.initial_called = true;
        let inner = self.manager.upgrade().ok_or_else(stopped_error)?;
        inner.ensure_active()?;
        let reader = inner
            .reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut backlog = if let Some(reader) = reader {
            reader().await?
        } else {
            Vec::new()
        };
        backlog = filter_backlog(backlog, self.state.targets.as_deref());
        let max_lines = if self.state.targets.is_some() {
            RAW_INTEGRATION_BACKLOG_LINES
        } else {
            RAW_BACKLOG_LINES
        };
        if backlog.len() > max_lines {
            backlog.drain(..backlog.len() - max_lines);
        }

        // Drain records captured since subscription before switching the
        // queue into live mode. Events emitted after the reader finishes stay
        // live; records already reflected in its backlog can be deduplicated.
        flush_raw(&inner.raw);
        let mut queue = self
            .state
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.closed {
            return Err(stopped_error());
        }
        if queue.overflowed {
            return Err(raw_overflow_error());
        }
        let mut in_backlog: HashMap<String, usize> = backlog
            .iter()
            .map(|line| normalize_first_line(line))
            .fold(HashMap::new(), |mut counts, line| {
                *counts.entry(line).or_default() += 1;
                counts
            });
        let pending = std::mem::take(&mut queue.pending);
        let mut live = Vec::new();
        for line in pending {
            let key = normalize_first_line(&line);
            match in_backlog.get_mut(&key) {
                Some(overlap) if *overlap > 0 => *overlap -= 1,
                _ => live.push(line),
            }
        }
        queue.collecting = false;
        queue.initialized = true;
        backlog.extend(live);
        Ok(backlog)
    }

    /// Wait for the next coalesced live array of formatted records.
    pub async fn next_batch(&mut self) -> Result<Option<Vec<String>>> {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = self
                    .state
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !queue.initialized {
                    return Err(Error::InvalidInput(
                        "read the raw subscription initial backlog first".into(),
                    ));
                }
                if queue.overflowed {
                    return Err(raw_overflow_error());
                }
                if let Some(batch) = queue.batches.pop_front() {
                    queue.queued_records = queue.queued_records.saturating_sub(batch.len());
                    return Ok(Some(batch));
                }
                if queue.closed {
                    return Ok(None);
                }
            }
            notified.await;
        }
    }
}

impl Drop for RawLogSubscription {
    fn drop(&mut self) {
        if let Some(inner) = self.manager.upgrade() {
            let mut raw = inner
                .raw
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            raw.subscribers.remove(&self.id);
            if raw.subscribers.is_empty() {
                raw.records.clear();
            }
        }
        let mut queue = self
            .state
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.closed = true;
        self.state.notify.notify_waiters();
    }
}

#[derive(Default)]
struct RawStream {
    next_id: AtomicU64,
    state: Mutex<RawState>,
    notify: Notify,
}

#[derive(Default)]
struct RawState {
    records: VecDeque<RawRecord>,
    subscribers: HashMap<u64, Weak<SubscriptionState>>,
}

#[derive(Debug, Clone)]
struct RawRecord {
    target: String,
    line: String,
}

struct SubscriptionState {
    targets: Option<Vec<String>>,
    queue: Mutex<SubscriberQueue>,
    notify: Notify,
}

struct SubscriberQueue {
    collecting: bool,
    initialized: bool,
    pending: VecDeque<String>,
    batches: VecDeque<Vec<String>>,
    queued_records: usize,
    overflowed: bool,
    closed: bool,
}

impl RawStream {
    fn push(&self, record: RawRecord) {
        let mut raw = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        raw.subscribers
            .retain(|_, subscriber| subscriber.strong_count() > 0);
        if raw.subscribers.is_empty() {
            raw.records.clear();
            return;
        }
        raw.records.push_back(record);
        if raw.records.len() > RAW_BUFFER_RECORDS {
            raw.records.pop_front();
            for subscriber in raw.subscribers.values().filter_map(Weak::upgrade) {
                subscriber
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .overflowed = true;
                subscriber.notify.notify_one();
            }
        }
        self.notify.notify_one();
    }
}

async fn run_raw_worker(raw: Arc<RawStream>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    close_stream(&raw);
                    return;
                }
            }
            _ = raw.notify.notified() => {}
        }
        flush_raw(&raw);

        let cooldown = tokio::time::sleep(RAW_COOLDOWN);
        tokio::pin!(cooldown);
        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        close_stream(&raw);
                        return;
                    }
                }
                _ = raw.notify.notified() => {}
                _ = &mut cooldown => break,
            }
        }
        flush_raw(&raw);
    }
}

fn flush_raw(raw: &RawStream) {
    let (records, subscribers) = {
        let mut state = raw
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .subscribers
            .retain(|_, subscriber| subscriber.strong_count() > 0);
        let records = std::mem::take(&mut state.records);
        let subscribers = state
            .subscribers
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        (records, subscribers)
    };

    if records.is_empty() {
        return;
    }
    for subscriber in subscribers {
        let lines: Vec<String> = records
            .iter()
            .filter(|record| {
                subscriber
                    .targets
                    .as_deref()
                    .is_none_or(|targets| logger_matches(&record.target, targets))
            })
            .map(|record| record.line.clone())
            .collect();
        if lines.is_empty() {
            continue;
        }
        let mut queue = subscriber
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.closed {
            continue;
        }
        if queue.collecting {
            queue.pending.extend(lines);
            if queue.pending.len() > RAW_BUFFER_RECORDS {
                queue.overflowed = true;
                while queue.pending.len() > RAW_BUFFER_RECORDS {
                    queue.pending.pop_front();
                }
            }
        } else {
            queue.queued_records += lines.len();
            queue.batches.push_back(lines);
            while queue.queued_records > RAW_BUFFER_RECORDS {
                queue.overflowed = true;
                if let Some(batch) = queue.batches.pop_front() {
                    queue.queued_records = queue.queued_records.saturating_sub(batch.len());
                } else {
                    break;
                }
            }
        }
        subscriber.notify.notify_one();
    }
}

fn close_stream(raw: &RawStream) {
    let subscribers = {
        let mut state = raw
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.records.clear();
        state
            .subscribers
            .drain()
            .filter_map(|(_, subscriber)| subscriber.upgrade())
            .collect::<Vec<_>>()
    };
    for subscriber in subscribers {
        let mut queue = subscriber
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.closed = true;
        subscriber.notify.notify_waiters();
    }
}

async fn run_event_worker(
    runtime: Runtime,
    inner: Arc<ManagerInner>,
    mut receiver: mpsc::Receiver<SystemLogEntry>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    while let Some(entry) = receiver.recv().await {
                        publish_event(&runtime, &inner, entry).await;
                    }
                    return;
                }
            }
            entry = receiver.recv() => {
                let Some(entry) = entry else { return; };
                publish_event(&runtime, &inner, entry).await;
            }
        }
    }
}

async fn publish_event(runtime: &Runtime, inner: &ManagerInner, entry: SystemLogEntry) {
    let Ok(Value::Object(data)) = serde_json::to_value(entry) else {
        inner.event_publish_failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let runtime = runtime.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || {
            runtime.fire_event(EVENT_SYSTEM_LOG, data, Context::default())
        })
        .await,
        Ok(Ok(_))
    ) {
        inner.event_publish_failed.fetch_add(1, Ordering::Relaxed);
    }
}

fn register_services(runtime: &Runtime, owner: &str, manager: SystemLogManager) -> Result<()> {
    let clear_manager = manager.clone();
    runtime.register_owned_service(
        owner,
        DOMAIN,
        SERVICE_CLEAR,
        json!({"description":"Clear retained system log entries","fields":{}}),
        Arc::new(move |_runtime, call| {
            if !call.data.is_empty() || !call.target.is_empty() {
                return Err(Error::InvalidInput(
                    "system_log.clear accepts no fields or target".into(),
                ));
            }
            clear_manager.clear()?;
            Ok(empty_result(call))
        }),
    )?;

    let write_manager = manager;
    if let Err(error) = runtime.register_owned_service(
        owner,
        DOMAIN,
        SERVICE_WRITE,
        json!({
            "description":"Write a real record through the system logger",
            "fields": {
                "message":{"required":true},
                "level":{"required":false,"default":"error"},
                "logger":{"required":false,"default":"homeassistant.components.system_log.external"}
            }
        }),
        Arc::new(move |_runtime, call| {
            if !call.target.is_empty() {
                return Err(Error::InvalidInput(
                    "system_log.write does not accept a target".into(),
                ));
            }
            if call
                .data
                .keys()
                .any(|key| !matches!(key.as_str(), "message" | "level" | "logger"))
            {
                return Err(Error::InvalidInput(
                    "system_log.write contains an unknown field".into(),
                ));
            }
            let message = call
                .data
                .get("message")
                .ok_or_else(|| Error::InvalidInput("system_log.write requires a message".into()))
                .and_then(|value| coerce_service_string(value, "message"))?;
            let level = call
                .data
                .get("level")
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| {
                            Error::InvalidInput("system_log.write level must be a string".into())
                        })
                        .and_then(SystemLogLevel::parse)
                })
                .transpose()?
                .unwrap_or(SystemLogLevel::Error);
            let target = call
                .data
                .get("logger")
                .map(|value| coerce_service_string(value, "logger"))
                .transpose()?
                .unwrap_or_else(|| "homeassistant.components.system_log.external".into());
            let target = if target.is_empty() {
                "root".to_owned()
            } else {
                target
            };

            // The weak manager check makes shutdown races fail without
            // keeping the integration/runtime alive through a service closure.
            write_manager.upgrade()?;
            emit_write(&target, level, &message);
            Ok(empty_result(call))
        }),
    ) {
        let _ = runtime.unregister_owned_services(owner);
        return Err(error);
    }
    Ok(())
}

/// Match the pinned `cv.string` conversion for JSON scalar service fields.
fn coerce_service_string(value: &Value, field: &str) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(if *value { "True" } else { "False" }.into()),
        Value::Number(value) if value.is_f64() => {
            let number = value
                .as_f64()
                .ok_or_else(|| Error::InvalidInput(format!("Invalid {field} number")))?;
            let value = format!("{number:?}");
            if let Some((base, exponent)) = value.split_once('e') {
                let exponent = exponent
                    .parse::<i32>()
                    .map_err(|_| Error::InvalidInput(format!("Invalid {field} number")))?;
                Ok(format!(
                    "{base}e{}{abs:02}",
                    if exponent < 0 { "-" } else { "+" },
                    abs = exponent.unsigned_abs()
                ))
            } else {
                Ok(value)
            }
        }
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(Error::InvalidInput(format!(
            "{field} must be a scalar string value"
        ))),
    }
}

#[track_caller]
fn emit_write(target: &str, level: SystemLogLevel, message: &str) {
    // The upstream service invokes one dynamic logger method at one common
    // callsite. Preserve that source for all severity branches so repeated
    // writes to the same logger deduplicate the same way.
    let caller = std::panic::Location::caller();
    let source_file = caller.file();
    let source_line = caller.line();
    match level.tracing_level() {
        Level::ERROR => tracing::error!(
            target: "hass_system_log::external",
            { hass.target = target, hass.level = level.as_str(), hass.file = source_file, hass.line = source_line },
            "{message}"
        ),
        Level::WARN => tracing::warn!(
            target: "hass_system_log::external",
            { hass.target = target, hass.level = level.as_str(), hass.file = source_file, hass.line = source_line },
            "{message}"
        ),
        Level::INFO => tracing::info!(
            target: "hass_system_log::external",
            { hass.target = target, hass.level = level.as_str(), hass.file = source_file, hass.line = source_line },
            "{message}"
        ),
        Level::DEBUG => tracing::debug!(
            target: "hass_system_log::external",
            { hass.target = target, hass.level = level.as_str(), hass.file = source_file, hass.line = source_line },
            "{message}"
        ),
        _ => unreachable!("the log service does not emit trace records"),
    }
}

fn empty_result(call: &ServiceCall) -> ServiceResult {
    ServiceResult {
        changed_states: Vec::new(),
        response: Some(Value::Null),
        context: call.context.clone(),
    }
}

fn integration_targets(domain: &str, catalog: &IntegrationCatalog) -> Result<Vec<String>> {
    let Some(targets) = catalog.get(domain) else {
        return Err(Error::NotFound("integration".into()));
    };
    let mut prefixes = vec![format!("homeassistant.components.{domain}")];
    prefixes.extend(targets.iter().cloned());
    prefixes.sort();
    prefixes.dedup();
    Ok(prefixes)
}

fn logger_matches(name: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| logger_prefix_matches(name, prefix))
}

fn logger_prefix_matches(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with("::"))
}

fn is_ignored_raw_record(record: &EventRecord) -> bool {
    let is_debug = record.level == LoggerLevel::Debug && record.severity_number != 5;
    if is_debug && logger_prefix_matches(&record.target, RAW_WEBSOCKET_CONNECTION_LOGGER) {
        return true;
    }
    let is_raw_delivery = record.spans.iter().any(|span| {
        span == RAW_DELIVERY_SPAN
            || span
                .strip_prefix(RAW_DELIVERY_SPAN)
                .is_some_and(|suffix| suffix.starts_with('{'))
    });
    is_raw_delivery && record.severity_number <= LoggerLevel::Debug.number()
}

fn filter_backlog(lines: Vec<String>, targets: Option<&[String]>) -> Vec<String> {
    let Some(targets) = targets else {
        return lines;
    };
    let mut matches = false;
    lines
        .into_iter()
        .filter(|line| {
            if let Some(target) = line_target(&normalize_first_line(line)) {
                matches = logger_matches(target, targets);
            }
            matches
        })
        .collect()
}

fn line_target(line: &str) -> Option<&str> {
    let (prefix, rest) = line.split_once(" [")?;
    if !is_native_log_header(prefix) && !is_home_assistant_log_header(prefix) {
        return None;
    }
    let end = rest.find("] ")?;
    Some(&rest[..end])
}

fn is_native_log_header(prefix: &str) -> bool {
    let Some((timestamp, level)) = prefix.rsplit_once(' ') else {
        return false;
    };
    if !matches!(
        level,
        "TRACE" | "DEBUG" | "INFO" | "WARN" | "WARNING" | "ERROR" | "CRITICAL"
    ) {
        return false;
    }
    let bytes = timestamp.as_bytes();
    bytes.len() >= 19
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b':'
        && bytes[17..19].iter().all(u8::is_ascii_digit)
}

fn is_home_assistant_log_header(prefix: &str) -> bool {
    let mut parts = prefix.splitn(4, ' ');
    let Some(date) = parts.next() else {
        return false;
    };
    let Some(time) = parts.next() else {
        return false;
    };
    let Some(level) = parts.next() else {
        return false;
    };
    let Some(thread) = parts.next() else {
        return false;
    };
    let date_bytes = date.as_bytes();
    date_bytes.len() == 10
        && date_bytes[0..4].iter().all(u8::is_ascii_digit)
        && date_bytes[4] == b'-'
        && date_bytes[5..7].iter().all(u8::is_ascii_digit)
        && date_bytes[7] == b'-'
        && date_bytes[8..10].iter().all(u8::is_ascii_digit)
        && !time.is_empty()
        && matches!(
            level,
            "TRACE" | "DEBUG" | "INFO" | "WARNING" | "ERROR" | "CRITICAL"
        )
        && thread.starts_with('(')
        && thread.ends_with(')')
}

fn normalize_first_line(line: &str) -> String {
    let first = line.split_once('\n').map_or(line, |(head, _)| head);
    strip_ansi(first).trim_end_matches('\r').to_owned()
}

fn strip_ansi(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && (0x20..=0x3f).contains(&bytes[index]) {
                let done = (0x40..=0x7e).contains(&bytes[index]);
                index += 1;
                if done {
                    break;
                }
            }
            continue;
        }
        let rest = &value[index..];
        let Some(ch) = rest.chars().next() else {
            break;
        };
        result.push(ch);
        index += ch.len_utf8();
    }
    result
}

fn is_error_level(level: LoggerLevel) -> bool {
    matches!(
        level,
        LoggerLevel::Warning | LoggerLevel::Error | LoggerLevel::Critical
    )
}

fn unix_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn timestamp_to_system_time(seconds: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(seconds.max(0.0))
}

fn stopped_error() -> Error {
    Error::Service("system log integration is stopped".into())
}

fn raw_overflow_error() -> Error {
    Error::Service("raw system log subscription overflowed".into())
}

#[cfg(test)]
mod tests;
