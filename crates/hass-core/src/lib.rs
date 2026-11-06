//! Native Home Assistant runtime primitives.
//!
//! This crate provides the shared state, event, persistence, and service
//! contracts used by the Rust rewrite. It implements a bounded core runtime;
//! it does not run Python integrations or control devices by itself.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Version identifier for this native implementation.
pub const VERSION: &str = "0.1.0-rust";

/// Error returned by the core runtime and its storage implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    /// The supplied value does not meet the runtime's input contract.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A requested entity or service does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A persistence operation failed.
    #[error("storage error: {0}")]
    Storage(String),
    /// A service registration or invocation failed.
    #[error("service error: {0}")]
    Service(String),
}

/// Core runtime result type.
pub type Result<T> = std::result::Result<T, Error>;

fn fresh_context_id() -> String {
    Uuid::new_v4().to_string()
}

/// Causal/user metadata associated with a state or event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Context {
    /// Unique identifier for this context.
    #[serde(default = "fresh_context_id")]
    pub id: String,
    /// Identifier of the context that caused this one, if any.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// User associated with the operation, if any.
    #[serde(default)]
    pub user_id: Option<String>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            id: fresh_context_id(),
            parent_id: None,
            user_id: None,
        }
    }
}

/// A point-in-time state snapshot for an entity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub entity_id: String,
    pub state: String,
    pub attributes: Map<String, Value>,
    pub last_changed: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
    pub last_reported: DateTime<Utc>,
    pub context: Context,
}

/// An event emitted by the runtime.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub event_type: String,
    pub data: Map<String, Value>,
    pub origin: String,
    pub time_fired: DateTime<Utc>,
    pub context: Context,
}

/// Maximum number of explicit entity or event-type filters accepted by one
/// history query. Unfiltered queries are bounded by result count instead.
pub const MAX_HISTORY_QUERY_FILTERS: usize = 4096;
/// Maximum number of state or event rows returned by one history query.
pub const MAX_HISTORY_QUERY_RESULTS: usize = 100_000;

/// State-history query bounds and filters.
///
/// Time bounds follow Home Assistant's history-period behavior: ordinary
/// rows must be strictly after `start` and strictly before `end`; when
/// `include_start_time_state` is true, the most recent row at or before
/// `start` is returned as a synthetic boundary snapshot with all timestamps
/// set to `start`. A missing requested entity is represented by an empty
/// series. `minimal_response` and `no_attributes` are retained for adapters
/// that compact the JSON; the core always returns full-fidelity [`State`]
/// values.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryQuery {
    /// `None` selects every recorded entity; `Some([])` selects no entities.
    pub entity_ids: Option<Vec<String>>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub include_start_time_state: bool,
    pub significant_changes_only: bool,
    pub minimal_response: bool,
    pub no_attributes: bool,
    pub exclude_entities: Vec<String>,
}

impl HistoryQuery {
    /// Validate bounds and filter sizes before querying a store.
    pub fn validate(&self) -> Result<()> {
        validate_time_range(self.start, self.end)?;
        let filter_count =
            self.entity_ids.as_ref().map_or(0, Vec::len) + self.exclude_entities.len();
        if filter_count > MAX_HISTORY_QUERY_FILTERS {
            return Err(Error::InvalidInput(format!(
                "history query has {filter_count} entity filters; maximum is {MAX_HISTORY_QUERY_FILTERS}"
            )));
        }
        if let Some(entity_ids) = &self.entity_ids {
            for entity_id in entity_ids {
                validate_entity_id(entity_id)?;
            }
        }
        for entity_id in &self.exclude_entities {
            validate_entity_id(entity_id)?;
        }
        Ok(())
    }
}

/// History rows for one entity. Empty `states` is meaningful for an explicit
/// filter whose entity has no matching history or was excluded.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HistorySeries {
    pub entity_id: String,
    pub states: Vec<State>,
}

/// Event-history query bounds and optional event-type filter.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventHistoryQuery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// `None` selects all event types; `Some([])` selects none.
    pub event_types: Option<Vec<String>>,
}

impl EventHistoryQuery {
    /// Validate bounds and event-type filter sizes before querying a store.
    pub fn validate(&self) -> Result<()> {
        validate_time_range(self.start, self.end)?;
        if self
            .event_types
            .as_ref()
            .is_some_and(|event_types| event_types.len() > MAX_HISTORY_QUERY_FILTERS)
        {
            return Err(Error::InvalidInput(format!(
                "event history query exceeds the {MAX_HISTORY_QUERY_FILTERS} event-type filter limit"
            )));
        }
        if let Some(event_types) = &self.event_types {
            for event_type in event_types {
                if event_type.is_empty() || event_type.chars().count() > 255 {
                    return Err(Error::InvalidInput(
                        "event type filter must contain 1 to 255 characters".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Counts of historical rows removed by a retention purge. Current state and
/// durable metadata rows are deliberately excluded.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PurgeResult {
    pub states_deleted: usize,
    pub events_deleted: usize,
}

/// Result of setting an entity state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StateChange {
    pub state: State,
    pub created: bool,
    /// True when a `state_changed` event was produced. A forced update counts
    /// as changed even when the state and attributes are equal.
    pub changed: bool,
}

/// Persistence boundary for the runtime.
///
/// Implementations must make a state write and its optional event atomic. When
/// `event` is `None`, the write is a report of an unchanged state: update the
/// current state but do not add a history row.
pub trait Store: Send + Sync {
    fn load_states(&self) -> Result<Vec<State>>;
    fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()>;
    fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()>;
    fn append_event(&self, event: &Event) -> Result<()>;
    fn get_data(&self, key: &str) -> Result<Option<Value>>;
    fn set_data(&self, key: &str, value: &Value) -> Result<()>;
    fn delete_data(&self, key: &str) -> Result<bool>;
    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>>;
    fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>>;
    fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>>;
    fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult>;
    /// Reclaim store space where supported. In-memory stores may no-op.
    fn repack(&self) -> Result<()>;
}

#[derive(Default)]
struct MemoryStoreData {
    states: HashMap<String, State>,
    history: Vec<State>,
    events: Vec<Event>,
    metadata: HashMap<String, Value>,
}

/// In-memory implementation of [`Store`], useful for tests and ephemeral runs.
#[derive(Default)]
pub struct MemoryStore {
    data: Mutex<MemoryStoreData>,
}

impl MemoryStore {
    /// Return all persisted events in insertion order.
    pub fn events(&self) -> Result<Vec<Event>> {
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        Ok(data.events.clone())
    }
}

impl Store for MemoryStore {
    fn load_states(&self) -> Result<Vec<State>> {
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        let mut states: Vec<_> = data.states.values().cloned().collect();
        states.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        Ok(states)
    }

    fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        data.states.insert(state.entity_id.clone(), state.clone());
        if let Some(event) = event {
            data.history.push(state.clone());
            data.events.push(event.clone());
        }
        Ok(())
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        data.states.remove(entity_id);
        data.events.push(event.clone());
        Ok(())
    }

    fn append_event(&self, event: &Event) -> Result<()> {
        self.data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?
            .events
            .push(event.clone());
        Ok(())
    }

    fn get_data(&self, key: &str) -> Result<Option<Value>> {
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        Ok(data.metadata.get(key).cloned())
    }

    fn set_data(&self, key: &str, value: &Value) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        data.metadata.insert(key.to_owned(), value.clone());
        Ok(())
    }

    fn delete_data(&self, key: &str) -> Result<bool> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        Ok(data.metadata.remove(key).is_some())
    }

    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>> {
        validate_time_range(start, end)?;
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        let mut states: Vec<_> = data
            .history
            .iter()
            .filter(|state| {
                state.entity_id == entity_id
                    && state.last_updated >= start
                    && state.last_updated <= end
            })
            .cloned()
            .collect();
        states.sort_by_key(|state| state.last_updated);
        Ok(states)
    }

    fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
        query.validate()?;
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;

        let excluded: HashSet<&str> = query.exclude_entities.iter().map(String::as_str).collect();
        let mut matching_ids: Vec<String> = if let Some(entity_ids) = &query.entity_ids {
            let mut seen = HashSet::new();
            entity_ids
                .iter()
                .filter(|entity_id| seen.insert(entity_id.as_str()))
                .cloned()
                .collect()
        } else {
            let mut ids: Vec<_> = data
                .history
                .iter()
                .map(|state| state.entity_id.clone())
                .collect();
            ids.sort();
            ids.dedup();
            ids
        };

        let mut series = Vec::with_capacity(matching_ids.len());
        let mut total_candidates = 0usize;
        for entity_id in matching_ids.drain(..) {
            if excluded.contains(entity_id.as_str()) {
                if query.entity_ids.is_some() {
                    series.push(HistorySeries {
                        entity_id,
                        states: Vec::new(),
                    });
                }
                continue;
            }
            let mut prior: Option<&State> = None;
            let mut rows: Vec<&State> = Vec::new();
            for state in data
                .history
                .iter()
                .filter(|state| state.entity_id == entity_id)
            {
                if state.last_updated <= query.start {
                    if prior.is_none_or(|current| state.last_updated >= current.last_updated) {
                        prior = Some(state);
                    }
                } else if state.last_updated < query.end {
                    rows.push(state);
                    total_candidates += 1;
                    if total_candidates > MAX_HISTORY_QUERY_RESULTS {
                        return Err(Error::InvalidInput(format!(
                            "history query exceeds the {MAX_HISTORY_QUERY_RESULTS} row limit"
                        )));
                    }
                }
            }
            rows.sort_by_key(|state| state.last_updated);

            let mut states = Vec::new();
            if query.include_start_time_state
                && let Some(previous) = prior
            {
                total_candidates += 1;
                if total_candidates > MAX_HISTORY_QUERY_RESULTS {
                    return Err(Error::InvalidInput(format!(
                        "history query exceeds the {MAX_HISTORY_QUERY_RESULTS} row limit"
                    )));
                }
                let mut boundary = previous.clone();
                boundary.last_changed = query.start;
                boundary.last_updated = query.start;
                boundary.last_reported = query.start;
                states.push(boundary);
            }
            states.extend(
                rows.into_iter()
                    .filter(|state| {
                        !query.significant_changes_only || is_significant_history_state(state)
                    })
                    .cloned(),
            );
            if query.entity_ids.is_none() && states.is_empty() {
                continue;
            }
            series.push(HistorySeries { entity_id, states });
        }
        Ok(series)
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
        query.validate()?;
        if query.event_types.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let event_types: Option<HashSet<&str>> = query
            .event_types
            .as_ref()
            .map(|types| types.iter().map(String::as_str).collect());
        let data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        let mut events = Vec::new();
        for event in data.events.iter().filter(|event| {
            event.time_fired > query.start
                && event.time_fired < query.end
                && event_types
                    .as_ref()
                    .is_none_or(|event_types| event_types.contains(event.event_type.as_str()))
        }) {
            events.push(event.clone());
            if events.len() > MAX_HISTORY_QUERY_RESULTS {
                return Err(Error::InvalidInput(format!(
                    "event history query exceeds the {MAX_HISTORY_QUERY_RESULTS} row limit"
                )));
            }
        }
        events.sort_by_key(|event| event.time_fired);
        Ok(events)
    }

    fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        let history_count = data.history.len();
        data.history.retain(|state| state.last_updated >= cutoff);
        let events_count = data.events.len();
        data.events.retain(|event| event.time_fired >= cutoff);
        Ok(PurgeResult {
            states_deleted: history_count - data.history.len(),
            events_deleted: events_count - data.events.len(),
        })
    }

    fn repack(&self) -> Result<()> {
        Ok(())
    }
}

/// Service request passed to a registered handler.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    pub data: Map<String, Value>,
    pub target: Map<String, Value>,
    pub context: Context,
}

/// Values returned by a service handler.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServiceResult {
    pub changed_states: Vec<State>,
    pub response: Option<Value>,
    pub context: Context,
}

/// A synchronous handler registered for a service.
pub type ServiceHandler =
    Arc<dyn Fn(&Runtime, &ServiceCall) -> Result<ServiceResult> + Send + Sync>;

/// Resolves Home Assistant targets before service dispatch. Return `false`
/// when explicit selectors match no entities; the runtime then returns an
/// empty successful result without invoking a handler.
pub type ServiceTargetResolver =
    Arc<dyn Fn(&Runtime, &mut ServiceCall) -> Result<bool> + Send + Sync>;

/// A lease on the installed service-target resolver. Clones share one lease;
/// dropping its last clone clears only the resolver generation it installed.
#[derive(Clone)]
pub struct ServiceTargetResolverGuard {
    _lease: Arc<ServiceTargetResolverLease>,
}

struct ServiceTargetResolverLease {
    runtime: Weak<RuntimeInner>,
    generation: u64,
}

impl Drop for ServiceTargetResolverLease {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let mut installed = runtime
            .service_target_resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if installed
            .as_ref()
            .is_some_and(|resolver| resolver.generation == self.generation)
        {
            *installed = None;
        }
    }
}

/// Selects whether an owner can handle one entity's service call.
pub type EntityMatcher = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct RegisteredService {
    owner: Option<String>,
    description: Value,
    handler: ServiceHandler,
}

#[derive(Clone)]
struct RegisteredEntityService {
    description: Value,
    matcher: EntityMatcher,
    handler: ServiceHandler,
}

// A store has one authoritative runtime cache. Runtime clones share that cache;
// independent constructors must not silently cache stale versions of one store.
static ACTIVE_STORES: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
static SERVICE_TARGET_RESOLVER_GENERATION: AtomicU64 = AtomicU64::new(1);

struct InstalledServiceTargetResolver {
    generation: u64,
    resolver: ServiceTargetResolver,
}

struct StoreLease(usize);

impl StoreLease {
    fn acquire(store: &Arc<dyn Store>) -> Result<Self> {
        let key = Arc::as_ptr(store) as *const () as usize;
        let mut active = ACTIVE_STORES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(key) {
            return Err(Error::Storage(
                "store already belongs to a runtime; clone the Runtime instead".into(),
            ));
        }
        Ok(Self(key))
    }
}

impl Drop for StoreLease {
    fn drop(&mut self) {
        ACTIVE_STORES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

struct RuntimeInner {
    _store_lease: StoreLease,
    store: Arc<dyn Store>,
    states: RwLock<HashMap<String, State>>,
    /// Serializes state and event persistence/publication as one ordered stream.
    write_order: Mutex<()>,
    events: broadcast::Sender<Event>,
    /// Integration dispatcher traffic is private and is never recorded as a
    /// public Home Assistant event (broker payloads may contain private data).
    signals: broadcast::Sender<Event>,
    time_zone: RwLock<String>,
    services: RwLock<BTreeMap<String, BTreeMap<String, RegisteredService>>>,
    entity_services: RwLock<BTreeMap<(String, String), BTreeMap<String, RegisteredEntityService>>>,
    service_target_resolver: Mutex<Option<InstalledServiceTargetResolver>>,
}

/// Shared, cloneable access to the state machine, event bus, and service registry.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

impl Runtime {
    /// Create a runtime and populate its current states from the store.
    pub fn new(store: Arc<dyn Store>) -> Result<Self> {
        let lease = StoreLease::acquire(&store)?;
        let mut states = HashMap::new();
        for state in store.load_states()? {
            validate_entity_id(&state.entity_id)
                .map_err(|error| Error::Storage(format!("invalid restored state: {error}")))?;
            if state.state.chars().count() > 255 {
                return Err(Error::Storage(
                    "restored state exceeds 255 characters".into(),
                ));
            }
            let id = state.entity_id.clone();
            if states.insert(id.clone(), state).is_some() {
                return Err(Error::Storage(format!("duplicate restored entity: {id}")));
            }
        }
        let (events, _) = broadcast::channel(1024);
        let (signals, _) = broadcast::channel(1024);
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                _store_lease: lease,
                store,
                states: RwLock::new(states),
                write_order: Mutex::new(()),
                events,
                signals,
                time_zone: RwLock::new("UTC".into()),
                services: RwLock::new(BTreeMap::new()),
                entity_services: RwLock::new(BTreeMap::new()),
                service_target_resolver: Mutex::new(None),
            }),
        })
    }

    /// Return current states sorted by entity ID.
    pub fn states(&self) -> Vec<State> {
        let mut states: Vec<_> = self
            .inner
            .states
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        states.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        states
    }

    /// Get one current state, if present.
    pub fn get_state(&self, entity_id: &str) -> Option<State> {
        self.inner
            .states
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(entity_id)
            .cloned()
    }

    /// Read durable application metadata from the runtime's store.
    pub fn get_data(&self, key: &str) -> Result<Option<Value>> {
        self.inner.store.get_data(key)
    }

    /// Persist durable application metadata before returning successfully.
    pub fn set_data(&self, key: &str, value: &Value) -> Result<()> {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.store.set_data(key, value)
    }

    /// Delete durable application metadata, returning whether a row existed.
    pub fn delete_data(&self, key: &str) -> Result<bool> {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.store.delete_data(key)
    }

    /// Subscribe to events emitted after successful persistence.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Actual public event-bus receivers, registered under the wildcard bus
    /// topic. Integration-private dispatcher receivers are excluded.
    pub fn event_listeners(&self) -> BTreeMap<String, usize> {
        let count = self.inner.events.receiver_count();
        if count == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([("*".to_owned(), count)])
        }
    }

    /// Atomically snapshot states and subscribe to subsequent public changes.
    /// The ordering guard prevents a client from receiving an older queued
    /// update after the initial snapshot and rolling its cache backward.
    pub fn subscribe_with_states(&self) -> (Vec<State>, broadcast::Receiver<Event>) {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = self.inner.events.subscribe();
        (self.states(), receiver)
    }

    /// Subscribe to private integration dispatcher signals. HTTP/WebSocket
    /// public event subscriptions deliberately do not use this channel.
    pub fn subscribe_signals(&self) -> broadcast::Receiver<Event> {
        self.inner.signals.subscribe()
    }

    /// Configured Home Assistant timezone, independent of the host OS setting.
    pub fn time_zone(&self) -> String {
        self.inner
            .time_zone
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Apply a validated setting after the configuration owner has persisted it.
    pub fn set_time_zone(&self, time_zone: &str) -> Result<()> {
        time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| Error::InvalidInput("unknown IANA time zone".into()))?;
        *self
            .inner
            .time_zone
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = time_zone.into();
        Ok(())
    }

    /// Dispatch actual integration traffic without storing or publicly exposing
    /// its payload. Producers own source I/O; this does not manufacture traffic.
    pub fn dispatch_signal(
        &self,
        signal: &str,
        data: Map<String, Value>,
        context: Context,
    ) -> Result<Event> {
        if signal.is_empty() || signal.len() > 255 {
            return Err(Error::InvalidInput(
                "invalid integration signal name".into(),
            ));
        }
        if serde_json::to_vec(&data)
            .map_err(|_| Error::InvalidInput("invalid signal data".into()))?
            .len()
            > 1024 * 1024
        {
            return Err(Error::InvalidInput(
                "integration signal exceeds 1 MiB".into(),
            ));
        }
        let event = Event {
            event_type: signal.into(),
            data,
            origin: "LOCAL".into(),
            time_fired: Utc::now(),
            context,
        };
        let _ = self.inner.signals.send(event.clone());
        Ok(event)
    }

    /// Set or report an entity state.
    pub fn set_state(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
    ) -> Result<StateChange> {
        self.set_state_checked(entity_id, state, attributes, context, force_update, None)
            .map(|change| change.expect("unconditional state write cannot fail its comparison"))
    }

    /// Publish only when the current snapshot matches `expected`, atomically.
    /// `None` requires the ID to be absent. A failed comparison leaves state,
    /// history, timestamps and events untouched and returns `Ok(None)`.
    pub fn set_state_if_unchanged(
        &self,
        entity_id: &str,
        expected: Option<&State>,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
    ) -> Result<Option<StateChange>> {
        self.set_state_checked(
            entity_id,
            state,
            attributes,
            context,
            force_update,
            Some(expected),
        )
    }

    fn set_state_checked(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
        expected: Option<Option<&State>>,
    ) -> Result<Option<StateChange>> {
        validate_entity_id(entity_id)?;
        let state_len = state.chars().count();
        if state_len > 255 {
            return Err(Error::InvalidInput(format!(
                "state is {state_len} characters; maximum is 255"
            )));
        }

        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.get_state(entity_id);
        if expected.is_some_and(|expected| previous.as_ref() != expected) {
            return Ok(None);
        }
        let now = Utc::now();
        let value_changed = previous.as_ref().is_none_or(|old| old.state != state);
        let attributes_changed = previous
            .as_ref()
            .is_none_or(|old| old.attributes != attributes);
        let created = previous.is_none();

        if !created && !value_changed && !attributes_changed && !force_update {
            let mut reported = previous.expect("previous state was checked");
            reported.last_reported = now;
            // Unchanged reports update persistence and last_reported only. The
            // state context remains the context of the last changed snapshot.
            self.inner.store.put_state(&reported, None)?;
            self.inner
                .states
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(entity_id.to_owned(), reported.clone());
            return Ok(Some(StateChange {
                state: reported,
                created: false,
                changed: false,
            }));
        }

        let changed = created || value_changed || attributes_changed || force_update;
        let next = State {
            entity_id: entity_id.to_owned(),
            state: state.to_owned(),
            attributes,
            // Upstream treats a forced update as a fresh state observation,
            // including last_changed (core.py async_set_internal).
            last_changed: if value_changed || force_update {
                now
            } else {
                previous
                    .as_ref()
                    .expect("unchanged value requires a previous state")
                    .last_changed
            },
            last_updated: if value_changed || attributes_changed || force_update {
                now
            } else {
                previous
                    .as_ref()
                    .expect("unchanged content requires a previous state")
                    .last_updated
            },
            last_reported: now,
            context: context.clone(),
        };
        let event = state_changed_event(entity_id, previous.as_ref(), Some(&next), context, now)?;

        // Persist both the new snapshot and event before exposing either in
        // memory or on the bus.
        self.inner.store.put_state(&next, Some(&event))?;
        self.inner
            .states
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(entity_id.to_owned(), next.clone());
        let _ = self.inner.events.send(event);
        Ok(Some(StateChange {
            state: next,
            created,
            changed,
        }))
    }

    /// Remove an entity's current state, retaining its historical snapshots.
    pub fn remove_state(&self, entity_id: &str, context: Context) -> Result<bool> {
        self.remove_state_checked(entity_id, None, context)
    }

    /// Remove only the exact observed state under the runtime's write lock.
    /// A concurrent replacement, including a newer report with the same value,
    /// remains intact and produces no removal event.
    pub fn remove_state_if_unchanged(&self, expected: &State, context: Context) -> Result<bool> {
        self.remove_state_checked(&expected.entity_id, Some(expected), context)
    }

    fn remove_state_checked(
        &self,
        entity_id: &str,
        expected: Option<&State>,
        context: Context,
    ) -> Result<bool> {
        validate_entity_id(entity_id)?;
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(previous) = self.get_state(entity_id) else {
            return Ok(false);
        };
        if expected.is_some_and(|expected| expected != &previous) {
            return Ok(false);
        }
        let now = Utc::now();
        let event = state_changed_event(entity_id, Some(&previous), None, context, now)?;
        self.inner.store.remove_state(entity_id, &event)?;
        self.inner
            .states
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(entity_id);
        let _ = self.inner.events.send(event);
        Ok(true)
    }

    /// Persist and publish an event.
    pub fn fire_event(
        &self,
        event_type: &str,
        data: Map<String, Value>,
        context: Context,
    ) -> Result<Event> {
        self.fire_event_origin(event_type, data, context, "LOCAL")
    }

    /// Persist a remotely originated REST event with the caller's context.
    pub fn fire_remote_event(
        &self,
        event_type: &str,
        data: Map<String, Value>,
        context: Context,
    ) -> Result<Event> {
        self.fire_event_origin(event_type, data, context, "REMOTE")
    }

    fn fire_event_origin(
        &self,
        event_type: &str,
        data: Map<String, Value>,
        context: Context,
        origin: &str,
    ) -> Result<Event> {
        if event_type.is_empty() {
            return Err(Error::InvalidInput("event type cannot be empty".into()));
        }
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let event = Event {
            event_type: event_type.to_owned(),
            data,
            origin: origin.into(),
            time_fired: Utc::now(),
            context,
        };
        self.inner.store.append_event(&event)?;
        let _ = self.inner.events.send(event.clone());
        Ok(event)
    }

    /// Query stored state history, inclusive of both bounds.
    pub fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>> {
        validate_entity_id(entity_id)?;
        validate_time_range(start, end)?;
        self.inner.store.history(entity_id, start, end)
    }

    /// Query multiple or all entities in one bounded recorder history request.
    pub fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
        query.validate()?;
        self.inner.store.history_query(query)
    }

    /// Query persisted public event history in chronological order.
    pub fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
        query.validate()?;
        self.inner.store.event_history_query(query)
    }

    /// Remove historical state and event rows strictly before `cutoff`.
    /// Current states, durable metadata, and private dispatcher signals are
    /// preserved. The write-order guard serializes purge against runtime writes.
    pub fn purge_before(&self, cutoff: DateTime<Utc>) -> Result<PurgeResult> {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.store.purge_before(cutoff)
    }

    /// Reclaim storage space, serialized with other runtime writes.
    pub fn repack_storage(&self) -> Result<()> {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.store.repack()
    }

    /// Register or replace a legacy service handler.
    ///
    /// A legacy registration cannot replace an owner-scoped service. Use
    /// [`Runtime::register_owned_service`] to update a service owned by an
    /// integration.
    pub fn register_service(
        &self,
        domain: &str,
        service: &str,
        description: Value,
        handler: ServiceHandler,
    ) -> Result<()> {
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        let mut services = self
            .inner
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = services.entry(domain.to_owned()).or_default();
        if let Some(existing) = entries.get(service)
            && let Some(owner) = &existing.owner
        {
            return Err(Error::Service(format!(
                "service {domain}.{service} is owned by {owner}"
            )));
        }
        entries.insert(
            service.to_owned(),
            RegisteredService {
                owner: None,
                description,
                handler,
            },
        );
        Ok(())
    }

    /// Register or update a synchronous service owned by one integration.
    /// A different owner or an existing legacy registration is a conflict.
    pub fn register_owned_service(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        handler: ServiceHandler,
    ) -> Result<()> {
        validate_owner(owner)?;
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        let mut services = self
            .inner
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = services.entry(domain.to_owned()).or_default();
        if let Some(existing) = entries.get(service)
            && existing.owner.as_deref() != Some(owner)
        {
            return Err(Error::Service(format!(
                "service {domain}.{service} is already registered by another owner"
            )));
        }
        entries.insert(
            service.to_owned(),
            RegisteredService {
                owner: Some(owner.to_owned()),
                description,
                handler,
            },
        );
        Ok(())
    }

    /// Remove the legacy services registered by `owner`, leaving every other
    /// owner's and every legacy registration in place.
    pub fn unregister_owned_services(&self, owner: &str) -> Result<usize> {
        validate_owner(owner)?;
        let mut services = self
            .inner
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = 0;
        services.retain(|_, entries| {
            entries.retain(|_, registered| {
                let remove = registered.owner.as_deref() == Some(owner);
                removed += usize::from(remove);
                !remove
            });
            !entries.is_empty()
        });
        Ok(removed)
    }

    /// Install a global service-target resolver only when no resolver is
    /// active. The returned guard does not keep this runtime alive.
    pub fn install_service_target_resolver_if_absent(
        &self,
        resolver: ServiceTargetResolver,
    ) -> Option<ServiceTargetResolverGuard> {
        let mut installed = self
            .inner
            .service_target_resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if installed.is_some() {
            return None;
        }
        let generation = SERVICE_TARGET_RESOLVER_GENERATION.fetch_add(1, Ordering::Relaxed);
        *installed = Some(InstalledServiceTargetResolver {
            generation,
            resolver,
        });
        Some(ServiceTargetResolverGuard {
            _lease: Arc::new(ServiceTargetResolverLease {
                runtime: Arc::downgrade(&self.inner),
                generation,
            }),
        })
    }

    /// Register an owner-scoped provider for calls targeting its entities.
    /// Registering the same owner/domain/service pair replaces only that
    /// owner's callback; other providers remain registered.
    pub fn register_entity_service(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        matcher: EntityMatcher,
        handler: ServiceHandler,
    ) -> Result<()> {
        validate_owner(owner)?;
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        self.inner
            .entity_services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((domain.to_owned(), service.to_owned()))
            .or_default()
            .insert(
                owner.to_owned(),
                RegisteredEntityService {
                    description,
                    matcher,
                    handler,
                },
            );
        Ok(())
    }

    /// Remove all service providers registered by one owner and return the
    /// number of owner/service registrations removed.
    pub fn unregister_entity_services(&self, owner: &str) -> Result<usize> {
        validate_owner(owner)?;
        let mut services = self
            .inner
            .entity_services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = 0;
        services.retain(|_, providers| {
            if providers.remove(owner).is_some() {
                removed += 1;
            }
            !providers.is_empty()
        });
        Ok(removed)
    }

    /// Invoke a registered service without holding the registry lock.
    pub fn call_service(&self, call: ServiceCall) -> Result<ServiceResult> {
        validate_component(&call.domain, "service domain")?;
        validate_component(&call.service, "service name")?;
        let mut call = call;
        let original_domain = call.domain.clone();
        let original_service = call.service.clone();
        let original_context = call.context.clone();
        let legacy = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&call.domain)
            .and_then(|services| services.get(&call.service))
            .map(|registered| Arc::clone(&registered.handler));
        let providers = self
            .inner
            .entity_services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(call.domain.clone(), call.service.clone()))
            .cloned()
            .unwrap_or_default();
        if legacy.is_none() && providers.is_empty() {
            return Err(Error::Service(format!(
                "unknown service {}.{}",
                call.domain, call.service
            )));
        }
        let resolver = self
            .inner
            .service_target_resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|installed| Arc::clone(&installed.resolver));
        if let Some(resolver) = resolver
            && !(resolver)(self, &mut call)?
        {
            return Ok(ServiceResult {
                changed_states: Vec::new(),
                response: None,
                context: original_context,
            });
        }
        if call.domain != original_domain || call.service != original_service {
            return Err(Error::InvalidInput(
                "service target resolver cannot change the service name".into(),
            ));
        }
        // Resolver callbacks normalize target fields only. The original causal
        // context remains the one passed to every handler and returned here.
        call.context = original_context.clone();
        if providers.is_empty() {
            let handler = legacy.ok_or_else(|| {
                Error::Service(format!("unknown service {}.{}", call.domain, call.service))
            })?;
            // Legacy handlers own their payload shape, including target maps
            // and service data fields that happen to be named `entity_id`.
            return handler(self, &call);
        }

        if !call.target.contains_key("entity_id")
            && !call.data.contains_key("entity_id")
            && let Some(handler) = legacy
        {
            return handler(self, &call);
        }
        let Some(requested_ids) = parse_service_target(&call)? else {
            if let Some(handler) = legacy {
                return handler(self, &call);
            }
            return Err(Error::Service(format!(
                "service {}.{} requires an entity target",
                call.domain, call.service
            )));
        };

        let all_requested = requested_ids.len() == 1 && requested_ids[0] == "all";
        let entity_ids = if all_requested {
            let mut matching = Vec::new();
            let mut states = self.states();
            states.retain(|state| entity_domain(&state.entity_id) == call.domain);
            for state in states {
                let owners: Vec<_> = providers
                    .iter()
                    .filter(|(_, provider)| (provider.matcher)(&state.entity_id))
                    .map(|(owner, _)| owner.clone())
                    .collect();
                if owners.len() > 1 {
                    return Err(Error::Service(format!(
                        "entity {} matches multiple providers for {}.{}",
                        state.entity_id, call.domain, call.service
                    )));
                }
                if owners.len() == 1 {
                    matching.push(state.entity_id);
                }
            }
            if matching.is_empty() {
                return Err(Error::Service(format!(
                    "target all matched no current entities for {}.{}",
                    call.domain, call.service
                )));
            }
            matching
        } else {
            requested_ids
        };

        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entity_id in entity_ids {
            validate_entity_id(&entity_id)?;
            let owners: Vec<_> = providers
                .iter()
                .filter(|(_, provider)| (provider.matcher)(&entity_id))
                .map(|(owner, _)| owner.clone())
                .collect();
            match owners.as_slice() {
                [owner] => grouped.entry(owner.clone()).or_default().push(entity_id),
                [] => {
                    return Err(Error::Service(format!(
                        "no provider for target {entity_id} on {}.{}",
                        call.domain, call.service
                    )));
                }
                _ => {
                    return Err(Error::Service(format!(
                        "entity {entity_id} matches multiple providers for {}.{}",
                        call.domain, call.service
                    )));
                }
            }
        }

        // Every entity was validated and assigned before any provider runs.
        let mut changed_states = Vec::new();
        let mut responses = Vec::new();
        for (owner, entity_ids) in grouped {
            let provider = providers
                .get(&owner)
                .expect("grouped owners came from registered providers");
            let mut data = call.data.clone();
            data.remove("entity_id");
            let subcall = ServiceCall {
                domain: call.domain.clone(),
                service: call.service.clone(),
                data,
                target: serde_json::json!({"entity_id": entity_ids})
                    .as_object()
                    .expect("service target is an object")
                    .clone(),
                context: call.context.clone(),
            };
            let result = (provider.handler)(self, &subcall)?;
            changed_states.extend(result.changed_states);
            if let Some(response) = result.response {
                responses.push(response);
            }
        }
        let response = match responses.len() {
            0 => None,
            1 => responses.pop(),
            _ => Some(Value::Array(responses)),
        };
        Ok(ServiceResult {
            changed_states,
            response,
            context: call.context,
        })
    }

    /// Return service descriptions as a JSON object keyed by domain and name.
    pub fn services(&self) -> Value {
        let services = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entity_services = self
            .inner
            .entity_services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut domains = Map::new();
        for (domain, entries) in services.iter() {
            let mut names = Map::new();
            for (service, registered) in entries {
                names.insert(service.clone(), registered.description.clone());
            }
            domains.insert(domain.clone(), Value::Object(names));
        }
        for ((domain, service), providers) in entity_services.iter() {
            let Some((_, provider)) = providers.first_key_value() else {
                continue;
            };
            let names = domains
                .entry(domain.clone())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("service inventory domains are objects");
            // A legacy registration takes precedence for the shared public
            // description; otherwise any live provider keeps the service
            // visible until its last owner unregisters.
            names
                .entry(service.clone())
                .or_insert_with(|| provider.description.clone());
        }
        Value::Object(domains)
    }
}

fn state_changed_event(
    entity_id: &str,
    old_state: Option<&State>,
    new_state: Option<&State>,
    context: Context,
    now: DateTime<Utc>,
) -> Result<Event> {
    let old_state = old_state
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| Error::InvalidInput(format!("cannot encode old state: {error}")))?
        .unwrap_or(Value::Null);
    let new_state = new_state
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| Error::InvalidInput(format!("cannot encode new state: {error}")))?
        .unwrap_or(Value::Null);
    let mut data = Map::new();
    data.insert("entity_id".into(), Value::String(entity_id.to_owned()));
    data.insert("old_state".into(), old_state);
    data.insert("new_state".into(), new_state);
    Ok(Event {
        event_type: "state_changed".into(),
        data,
        origin: "LOCAL".into(),
        time_fired: now,
        context,
    })
}

fn is_significant_history_state(state: &State) -> bool {
    const SIGNIFICANT_DOMAINS: &[&str] = &[
        "climate",
        "device_tracker",
        "humidifier",
        "thermostat",
        "water_heater",
    ];
    state.last_changed == state.last_updated
        || SIGNIFICANT_DOMAINS.contains(&entity_domain(&state.entity_id))
}

fn entity_domain(entity_id: &str) -> &str {
    entity_id.split_once('.').map_or("", |(domain, _)| domain)
}

fn validate_owner(owner: &str) -> Result<()> {
    if owner.trim().is_empty() || owner.chars().count() > 255 || owner.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(
            "service owner must contain 1 to 255 non-control characters".into(),
        ));
    }
    Ok(())
}

fn parse_service_target(call: &ServiceCall) -> Result<Option<Vec<String>>> {
    let unsupported = ["area_id", "device_id", "label_id"];
    if let Some(field) = call
        .target
        .keys()
        .find(|field| field.as_str() != "entity_id")
    {
        return Err(Error::InvalidInput(format!(
            "unsupported service target field {field:?}"
        )));
    }
    if let Some(field) = unsupported
        .iter()
        .find(|field| call.data.contains_key(**field))
    {
        return Err(Error::InvalidInput(format!(
            "unsupported service target field {field:?}"
        )));
    }

    let target_entity = call.target.get("entity_id");
    let data_entity = call.data.get("entity_id");
    if target_entity.is_some() && data_entity.is_some() {
        return Err(Error::InvalidInput(
            "service entity_id must be supplied in target or data, not both".into(),
        ));
    }
    let Some(value) = target_entity.or(data_entity) else {
        return Ok(None);
    };
    let entity_ids = match value {
        Value::String(entity_id) if !entity_id.is_empty() => vec![entity_id.clone()],
        Value::Array(entity_ids) => {
            if entity_ids.is_empty() {
                return Err(Error::InvalidInput(
                    "service entity_id list cannot be empty".into(),
                ));
            }
            if entity_ids.len() > MAX_HISTORY_QUERY_FILTERS {
                return Err(Error::InvalidInput(format!(
                    "service entity_id list exceeds the {MAX_HISTORY_QUERY_FILTERS} target limit"
                )));
            }
            entity_ids
                .iter()
                .map(|entity_id| match entity_id {
                    Value::String(entity_id) if !entity_id.is_empty() => Ok(entity_id.clone()),
                    _ => Err(Error::InvalidInput(
                        "service entity_id list must contain non-empty strings".into(),
                    )),
                })
                .collect::<Result<Vec<_>>>()?
        }
        _ => {
            return Err(Error::InvalidInput(
                "service entity_id must be a string or a list of strings".into(),
            ));
        }
    };
    let is_all = entity_ids.len() == 1 && entity_ids[0] == "all";
    if entity_ids.iter().any(|entity_id| entity_id == "all") && !is_all {
        return Err(Error::InvalidInput(
            "service target all cannot be combined with entity IDs".into(),
        ));
    }
    if !is_all {
        for entity_id in &entity_ids {
            validate_entity_id(entity_id)?;
        }
    }
    let mut unique = HashSet::new();
    if entity_ids
        .iter()
        .any(|entity_id| !unique.insert(entity_id.clone()))
    {
        return Err(Error::InvalidInput(
            "service entity_id list contains duplicates".into(),
        ));
    }
    Ok(Some(entity_ids))
}

fn validate_entity_id(entity_id: &str) -> Result<()> {
    let Some((domain, object_id)) = entity_id.split_once('.') else {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {entity_id:?}: expected domain.object_id"
        )));
    };
    if object_id.contains('.') {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {entity_id:?}: expected exactly one dot"
        )));
    }
    validate_component(domain, "entity domain")?;
    validate_component(object_id, "entity object ID")
}

fn validate_component(value: &str, name: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid {name} {value:?}: expected lowercase ASCII letters, digits, or underscores"
        )));
    }
    Ok(())
}

fn validate_time_range(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<()> {
    if start > end {
        return Err(Error::InvalidInput(
            "history start must be no later than end".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn empty_map() -> Map<String, Value> {
        Map::new()
    }

    fn test_time(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("valid test time")
    }

    fn test_state(entity_id: &str, value: &str, seconds: i64) -> State {
        let at = test_time(seconds);
        State {
            entity_id: entity_id.into(),
            state: value.into(),
            attributes: serde_json::json!({"fixture": true})
                .as_object()
                .expect("object")
                .clone(),
            last_changed: at,
            last_updated: at,
            last_reported: at,
            context: Context {
                id: format!("context-{entity_id}-{seconds}"),
                parent_id: Some("parent-context".into()),
                user_id: Some("fixture-user".into()),
            },
        }
    }

    fn test_event(event_type: &str, seconds: i64) -> Event {
        Event {
            event_type: event_type.into(),
            data: empty_map(),
            origin: "LOCAL".into(),
            time_fired: test_time(seconds),
            context: Context::default(),
        }
    }

    fn runtime_with_store(store: Arc<dyn Store>) -> Runtime {
        Runtime::new(store).expect("runtime should load")
    }

    #[test]
    fn one_store_has_one_authoritative_runtime_until_all_clones_drop() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let cloned_runtime = runtime.clone();
        assert!(matches!(
            Runtime::new(store.clone()),
            Err(Error::Storage(_))
        ));
        drop(runtime);
        assert!(matches!(
            Runtime::new(store.clone()),
            Err(Error::Storage(_))
        ));
        drop(cloned_runtime);
        assert!(Runtime::new(store).is_ok());
    }

    #[test]
    fn history_query_builds_boundary_series_and_preserves_explicit_missing_ids() {
        let store = Arc::new(MemoryStore::default());
        for (entity_id, value, at) in [
            ("sensor.one", "before", 10),
            ("sensor.one", "inside", 20),
            ("sensor.one", "at_end", 30),
            ("sensor.two", "before", 12),
        ] {
            let state = test_state(entity_id, value, at);
            store
                .put_state(&state, Some(&test_event("state_changed", at)))
                .unwrap();
        }
        let runtime = runtime_with_store(store.clone());
        let query = HistoryQuery {
            entity_ids: Some(vec![
                "sensor.one".into(),
                "sensor.missing".into(),
                "sensor.two".into(),
            ]),
            start: test_time(15),
            end: test_time(30),
            include_start_time_state: true,
            significant_changes_only: false,
            minimal_response: true,
            no_attributes: true,
            exclude_entities: vec!["sensor.two".into()],
        };
        let result = runtime.history_query(&query).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|series| series.entity_id.as_str())
                .collect::<Vec<_>>(),
            ["sensor.one", "sensor.missing", "sensor.two"]
        );
        assert_eq!(result[0].states.len(), 2);
        assert_eq!(result[0].states[0].state, "before");
        assert_eq!(result[0].states[0].last_changed, test_time(15));
        assert_eq!(result[0].states[0].last_updated, test_time(15));
        assert_eq!(result[0].states[0].last_reported, test_time(15));
        assert_eq!(result[0].states[0].context.id, "context-sensor.one-10");
        assert_eq!(
            result[0].states[0].attributes,
            test_state("sensor.one", "before", 10).attributes
        );
        assert_eq!(result[0].states[1].state, "inside");
        assert!(result[1].states.is_empty());
        assert!(result[2].states.is_empty());

        let all = runtime
            .history_query(&HistoryQuery {
                entity_ids: None,
                start: test_time(0),
                end: test_time(40),
                include_start_time_state: false,
                significant_changes_only: false,
                minimal_response: false,
                no_attributes: false,
                exclude_entities: vec!["sensor.one".into()],
            })
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].entity_id, "sensor.two");
    }

    #[test]
    fn history_queries_validate_ranges_and_filter_bounds() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let invalid_range = HistoryQuery {
            entity_ids: None,
            start: test_time(2),
            end: test_time(1),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        };
        assert!(matches!(
            runtime.history_query(&invalid_range),
            Err(Error::InvalidInput(_))
        ));
        let too_many = HistoryQuery {
            entity_ids: Some(vec!["sensor.one".into(); MAX_HISTORY_QUERY_FILTERS + 1]),
            start: test_time(0),
            end: test_time(1),
            include_start_time_state: false,
            significant_changes_only: false,
            minimal_response: false,
            no_attributes: false,
            exclude_entities: Vec::new(),
        };
        assert!(matches!(
            runtime.history_query(&too_many),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn significant_history_filters_attribute_only_changes_outside_significant_domains() {
        let store = Arc::new(MemoryStore::default());
        let sensor_at = test_time(10);
        let mut sensor_attribute_change = test_state("sensor.attribute_only", "same", 20);
        sensor_attribute_change.last_changed = sensor_at;
        store
            .put_state(
                &sensor_attribute_change,
                Some(&test_event("state_changed", 20)),
            )
            .unwrap();
        let climate_at = test_time(10);
        let mut climate_attribute_change = test_state("climate.attribute_only", "same", 20);
        climate_attribute_change.last_changed = climate_at;
        store
            .put_state(
                &climate_attribute_change,
                Some(&test_event("state_changed", 20)),
            )
            .unwrap();
        let runtime = runtime_with_store(store);
        let result = runtime
            .history_query(&HistoryQuery {
                entity_ids: None,
                start: test_time(0),
                end: test_time(30),
                include_start_time_state: false,
                significant_changes_only: true,
                minimal_response: false,
                no_attributes: false,
                exclude_entities: Vec::new(),
            })
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].entity_id, "climate.attribute_only");
        assert_eq!(result[0].states[0].last_changed, climate_at);
        assert_eq!(result[0].states[0].last_updated, test_time(20));
    }

    #[test]
    fn event_history_query_filters_and_sorts_strictly_inside_the_range() {
        let store = Arc::new(MemoryStore::default());
        store.append_event(&test_event("edge", 10)).unwrap();
        store.append_event(&test_event("wanted", 20)).unwrap();
        store.append_event(&test_event("ignored", 25)).unwrap();
        store.append_event(&test_event("edge", 30)).unwrap();
        let runtime = runtime_with_store(store);
        let events = runtime
            .event_history_query(&EventHistoryQuery {
                start: test_time(10),
                end: test_time(30),
                event_types: Some(vec!["wanted".into(), "ignored".into()]),
            })
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["wanted", "ignored"]
        );
        assert_eq!(events[0].time_fired, test_time(20));
        assert_eq!(events[1].time_fired, test_time(25));
    }

    #[test]
    fn retention_purge_keeps_current_state_and_private_metadata() {
        let store = Arc::new(MemoryStore::default());
        let old = test_state("sensor.persisted", "old", 10);
        store
            .put_state(&old, Some(&test_event("state_changed", 10)))
            .unwrap();
        let current = test_state("sensor.persisted", "current", 20);
        store
            .put_state(&current, Some(&test_event("state_changed", 20)))
            .unwrap();
        store
            .set_data("private.fixture", &serde_json::json!({"secret":"retained"}))
            .unwrap();
        store.append_event(&test_event("old_custom", 9)).unwrap();
        let runtime = runtime_with_store(store.clone());
        let result = runtime.purge_before(test_time(20)).unwrap();
        assert_eq!(result.states_deleted, 1);
        assert_eq!(result.events_deleted, 2);
        assert_eq!(runtime.get_state("sensor.persisted"), Some(current.clone()));
        assert_eq!(
            runtime.get_data("private.fixture").unwrap(),
            Some(serde_json::json!({"secret":"retained"}))
        );
        drop(runtime);

        let restarted = runtime_with_store(store);
        restarted.repack_storage().unwrap();
        assert_eq!(restarted.get_state("sensor.persisted"), Some(current));
        assert!(
            restarted
                .history("sensor.persisted", test_time(0), test_time(19))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn integration_signals_stay_out_of_the_public_event_bus() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let mut public = runtime.subscribe();
        let mut private = runtime.subscribe_signals();
        runtime
            .dispatch_signal(
                "mqtt_message",
                serde_json::json!({"topic":"fixture/sensor","payload":"42"})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
            )
            .unwrap();
        assert_eq!(private.try_recv().unwrap().event_type, "mqtt_message");
        assert!(matches!(
            public.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(
            runtime
                .dispatch_signal("", Map::new(), Context::default())
                .is_err()
        );
    }

    #[test]
    fn unchanged_report_updates_only_last_reported_and_skips_history() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime_with_store(store.clone());
        let mut events = runtime.subscribe();
        let first = runtime
            .set_state(
                "sensor.temperature",
                "20",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");
        thread::sleep(Duration::from_millis(2));

        let report = runtime
            .set_state(
                "sensor.temperature",
                "20",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap();

        assert!(!report.created);
        assert!(!report.changed);
        assert_eq!(report.state.last_changed, first.last_changed);
        assert_eq!(report.state.last_updated, first.last_updated);
        assert!(report.state.last_reported > first.last_reported);
        assert_eq!(
            store
                .history("sensor.temperature", first.last_changed, Utc::now())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
    }

    #[test]
    fn attribute_and_state_updates_have_expected_timestamps_and_events() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let mut events = runtime.subscribe();
        let first = runtime
            .set_state(
                "sensor.temperature",
                "20",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        thread::sleep(Duration::from_millis(2));
        let mut attrs = empty_map();
        attrs.insert("unit_of_measurement".into(), Value::String("°C".into()));
        let attribute_update = runtime
            .set_state("sensor.temperature", "20", attrs, Context::default(), false)
            .unwrap();
        assert!(attribute_update.changed);
        assert_eq!(attribute_update.state.last_changed, first.last_changed);
        assert!(attribute_update.state.last_updated > first.last_updated);
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");

        thread::sleep(Duration::from_millis(2));
        let state_update = runtime
            .set_state(
                "sensor.temperature",
                "21",
                attribute_update.state.attributes.clone(),
                Context::default(),
                false,
            )
            .unwrap();
        assert!(state_update.changed);
        assert!(state_update.state.last_changed > attribute_update.state.last_changed);
        assert!(state_update.state.last_updated > attribute_update.state.last_updated);
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");
    }

    #[test]
    fn force_update_refreshes_timestamps_like_upstream() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let mut events = runtime.subscribe();
        let first = runtime
            .set_state(
                "sensor.temperature",
                "20",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        thread::sleep(Duration::from_millis(2));
        let forced = runtime
            .set_state(
                "sensor.temperature",
                "20",
                empty_map(),
                Context::default(),
                true,
            )
            .unwrap();

        assert!(forced.changed);
        assert!(forced.state.last_changed > first.last_changed);
        assert!(forced.state.last_updated > first.last_updated);
        assert!(forced.state.last_reported >= first.last_reported);
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");
    }

    #[test]
    fn rejects_invalid_entity_ids_and_state_lengths_by_characters() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        for entity_id in [
            "Sensor.foo",
            "sensor",
            ".foo",
            "sensor.",
            "sensor.a.b",
            "sensor.bad-name",
        ] {
            assert!(
                matches!(
                    runtime.set_state(entity_id, "x", empty_map(), Context::default(), false),
                    Err(Error::InvalidInput(_))
                ),
                "accepted {entity_id:?}"
            );
        }
        assert!(matches!(
            runtime.set_state(
                "sensor.ok",
                &"é".repeat(256),
                empty_map(),
                Context::default(),
                false
            ),
            Err(Error::InvalidInput(_))
        ));
        assert!(
            runtime
                .set_state(
                    "sensor.ok",
                    &"é".repeat(255),
                    empty_map(),
                    Context::default(),
                    false
                )
                .is_ok()
        );
    }

    #[test]
    fn deletion_persists_before_event_and_retains_history() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime_with_store(store.clone());
        let mut events = runtime.subscribe();
        runtime
            .set_state(
                "light.kitchen",
                "on",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap();
        let mut deletion = runtime.subscribe();
        assert!(
            runtime
                .remove_state("light.kitchen", Context::default())
                .unwrap()
        );
        assert!(runtime.get_state("light.kitchen").is_none());
        assert!(store.load_states().unwrap().is_empty());
        assert_eq!(
            store
                .history("light.kitchen", DateTime::<Utc>::MIN_UTC, Utc::now())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");
        let event = deletion.try_recv().unwrap();
        assert_eq!(event.event_type, "state_changed");
        assert!(event.data["new_state"].is_null());
        assert_eq!(event.data["old_state"]["state"], "on");
        assert!(
            !runtime
                .remove_state("light.kitchen", Context::default())
                .unwrap()
        );
    }

    struct FallibleStore {
        inner: MemoryStore,
        fail_writes: AtomicBool,
    }

    impl Store for FallibleStore {
        fn load_states(&self) -> Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected put failure".into()));
            }
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected remove failure".into()));
            }
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected event failure".into()));
            }
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected metadata write failure".into()));
            }
            self.inner.set_data(key, value)
        }

        fn delete_data(&self, key: &str) -> Result<bool> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected metadata delete failure".into()));
            }
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
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected purge failure".into()));
            }
            self.inner.purge_before(cutoff)
        }

        fn repack(&self) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(Error::Storage("injected repack failure".into()));
            }
            self.inner.repack()
        }
    }

    #[test]
    fn conditional_state_writes_require_absence_or_the_exact_owned_snapshot() {
        let store = Arc::new(FallibleStore {
            inner: MemoryStore::default(),
            fail_writes: AtomicBool::new(false),
        });
        let runtime = runtime_with_store(store.clone());
        let created = runtime
            .set_state_if_unchanged(
                "sensor.conditional",
                None,
                "first",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .unwrap()
            .state;
        let mut events = runtime.subscribe();
        assert!(
            runtime
                .set_state_if_unchanged(
                    "sensor.conditional",
                    None,
                    "overwrite",
                    empty_map(),
                    Context::default(),
                    false
                )
                .unwrap()
                .is_none()
        );
        let foreign = runtime
            .set_state(
                "sensor.conditional",
                "replacement",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        events.try_recv().unwrap();
        assert!(
            runtime
                .set_state_if_unchanged(
                    "sensor.conditional",
                    Some(&created),
                    "unavailable",
                    empty_map(),
                    Context::default(),
                    false
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            runtime.get_state("sensor.conditional"),
            Some(foreign.clone())
        );
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(matches!(
            runtime.set_state_if_unchanged(
                "sensor.conditional",
                Some(&foreign),
                "unavailable",
                empty_map(),
                Context::default(),
                false
            ),
            Err(Error::Storage(_))
        ));
        assert_eq!(
            runtime.get_state("sensor.conditional"),
            Some(foreign.clone())
        );
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        store.fail_writes.store(false, Ordering::SeqCst);
        assert!(
            runtime
                .set_state_if_unchanged(
                    "sensor.conditional",
                    Some(&foreign),
                    "unavailable",
                    empty_map(),
                    Context::default(),
                    false
                )
                .unwrap()
                .is_some()
        );
        assert_eq!(
            runtime.get_state("sensor.conditional").unwrap().state,
            "unavailable"
        );
        assert_eq!(
            events.try_recv().unwrap().data["old_state"]["state"],
            "replacement"
        );
    }

    #[test]
    fn conditional_state_removal_preserves_newer_reports_and_failed_writes() {
        let store = Arc::new(FallibleStore {
            inner: MemoryStore::default(),
            fail_writes: AtomicBool::new(false),
        });
        let runtime = runtime_with_store(store.clone());
        let first = runtime
            .set_state(
                "sensor.owned",
                "one",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        let second = runtime
            .set_state(
                "sensor.owned",
                "two",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        let mut events = runtime.subscribe();
        assert!(
            !runtime
                .remove_state_if_unchanged(&first, Context::default())
                .unwrap()
        );
        assert_eq!(runtime.get_state("sensor.owned"), Some(second.clone()));
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        let newest = runtime
            .set_state("sensor.owned", "two", empty_map(), Context::default(), true)
            .unwrap()
            .state;
        events.try_recv().unwrap();
        assert!(
            !runtime
                .remove_state_if_unchanged(&second, Context::default())
                .unwrap()
        );
        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(matches!(
            runtime.remove_state_if_unchanged(&newest, Context::default()),
            Err(Error::Storage(_))
        ));
        assert_eq!(runtime.get_state("sensor.owned"), Some(newest.clone()));
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        store.fail_writes.store(false, Ordering::SeqCst);
        assert!(
            runtime
                .remove_state_if_unchanged(&newest, Context::default())
                .unwrap()
        );
        let removed = events.try_recv().unwrap();
        assert_eq!(removed.data["old_state"]["state"], "two");
        assert!(removed.data["new_state"].is_null());
        assert!(
            !runtime
                .remove_state_if_unchanged(&newest, Context::default())
                .unwrap()
        );
    }

    #[test]
    fn persistence_failures_do_not_mutate_or_publish() {
        let store = Arc::new(FallibleStore {
            inner: MemoryStore::default(),
            fail_writes: AtomicBool::new(false),
        });
        let runtime = runtime_with_store(store.clone());
        let mut events = runtime.subscribe();
        let original = runtime
            .set_state("switch.test", "off", empty_map(), Context::default(), false)
            .unwrap()
            .state;
        assert_eq!(events.try_recv().unwrap().event_type, "state_changed");

        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(matches!(
            runtime.set_state("switch.test", "on", empty_map(), Context::default(), false),
            Err(Error::Storage(_))
        ));
        assert_eq!(runtime.get_state("switch.test").unwrap(), original);
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        assert!(matches!(
            runtime.fire_event("custom_event", empty_map(), Context::default()),
            Err(Error::Storage(_))
        ));
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
        assert!(matches!(
            runtime.remove_state("switch.test", Context::default()),
            Err(Error::Storage(_))
        ));
        assert_eq!(runtime.get_state("switch.test").unwrap(), original);
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
    }

    #[test]
    fn runtime_metadata_round_trips_and_storage_failures_propagate() {
        let store = Arc::new(FallibleStore {
            inner: MemoryStore::default(),
            fail_writes: AtomicBool::new(false),
        });
        let runtime = runtime_with_store(store.clone());
        let value = serde_json::json!({"user": "alice", "roles": ["admin"]});

        assert_eq!(runtime.get_data("auth.users"), Ok(None));
        runtime.set_data("auth.users", &value).unwrap();
        assert_eq!(runtime.get_data("auth.users"), Ok(Some(value.clone())));

        store.fail_writes.store(true, Ordering::SeqCst);
        let changed = serde_json::json!({"user": "bob"});
        assert!(matches!(
            runtime.set_data("auth.users", &changed),
            Err(Error::Storage(_))
        ));
        assert!(matches!(
            runtime.delete_data("auth.users"),
            Err(Error::Storage(_))
        ));
        assert_eq!(runtime.get_data("auth.users"), Ok(Some(value.clone())));

        store.fail_writes.store(false, Ordering::SeqCst);
        assert!(runtime.delete_data("auth.users").unwrap());
        assert!(!runtime.delete_data("auth.users").unwrap());
        assert_eq!(runtime.get_data("auth.users"), Ok(None));
    }

    #[test]
    fn concurrent_updates_publish_in_the_same_order_as_mutations() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        runtime
            .set_state(
                "sensor.counter",
                "initial",
                empty_map(),
                Context::default(),
                false,
            )
            .unwrap();
        let mut events = runtime.subscribe();
        let count = 24;
        let barrier = Arc::new(Barrier::new(count + 1));
        let mut workers = Vec::new();
        for index in 0..count {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                runtime
                    .set_state(
                        "sensor.counter",
                        &index.to_string(),
                        empty_map(),
                        Context::default(),
                        false,
                    )
                    .unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let mut prior = Value::String("initial".into());
        for _ in 0..count {
            let event = events.try_recv().unwrap();
            assert_eq!(event.event_type, "state_changed");
            assert_eq!(event.data["old_state"]["state"], prior);
            prior = event.data["new_state"]["state"].clone();
        }
        assert_eq!(
            runtime.get_state("sensor.counter").unwrap().state,
            prior.as_str().unwrap()
        );
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );
    }

    #[test]
    fn service_handler_can_reenter_runtime_registry_and_state_methods() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let handler: ServiceHandler = Arc::new(|runtime, call| {
            assert!(runtime.services()["test"]["ping"].is_object());
            let change = runtime.set_state(
                "sensor.service",
                "called",
                empty_map(),
                call.context.clone(),
                false,
            )?;
            Ok(ServiceResult {
                changed_states: vec![change.state],
                response: Some(Value::String("ok".into())),
                context: call.context.clone(),
            })
        });
        runtime
            .register_service("test", "ping", serde_json::json!({"name": "Ping"}), handler)
            .unwrap();
        let call = ServiceCall {
            domain: "test".into(),
            service: "ping".into(),
            data: empty_map(),
            target: empty_map(),
            context: Context::default(),
        };
        let result = runtime.call_service(call.clone()).unwrap();
        assert_eq!(result.response, Some(Value::String("ok".into())));
        assert_eq!(runtime.get_state("sensor.service").unwrap().state, "called");
        assert_eq!(
            runtime.call_service(ServiceCall {
                service: "missing".into(),
                ..call
            }),
            Err(Error::Service("unknown service test.missing".into()))
        );
    }

    #[test]
    fn legacy_service_receives_explicit_entity_id_unchanged() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let observed = Arc::new(Mutex::new(None));
        let observed_handler = observed.clone();
        runtime
            .register_service(
                "script",
                "run",
                serde_json::json!({"name":"Run"}),
                Arc::new(move |_runtime, call| {
                    *observed_handler.lock().unwrap() = Some(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let call = ServiceCall {
            domain: "script".into(),
            service: "run".into(),
            data: serde_json::json!({"entity_id":"script.target","payload":"kept"})
                .as_object()
                .unwrap()
                .clone(),
            target: serde_json::json!({"area_id":"room"})
                .as_object()
                .unwrap()
                .clone(),
            context: Context::default(),
        };
        runtime.call_service(call.clone()).unwrap();
        assert_eq!(*observed.lock().unwrap(), Some(call));
    }

    #[test]
    fn owned_services_allow_same_owner_updates_and_cleanup_only_owned_entries() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let service_handler = |response: &'static str| -> ServiceHandler {
            Arc::new(move |_runtime, call| {
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: Some(Value::String(response.into())),
                    context: call.context.clone(),
                })
            })
        };
        runtime
            .register_owned_service(
                "template-engine",
                "template",
                "render",
                serde_json::json!({"version":1}),
                service_handler("old"),
            )
            .unwrap();
        runtime
            .register_owned_service(
                "template-engine",
                "template",
                "render",
                serde_json::json!({"version":2}),
                service_handler("new"),
            )
            .unwrap();
        runtime
            .register_owned_service(
                "other-owner",
                "template",
                "reload",
                serde_json::json!({"name":"Reload"}),
                service_handler("other"),
            )
            .unwrap();
        runtime
            .register_service(
                "template",
                "legacy",
                serde_json::json!({"name":"Legacy"}),
                service_handler("legacy"),
            )
            .unwrap();
        assert!(matches!(
            runtime.register_owned_service(
                "other-owner",
                "template",
                "render",
                Value::Null,
                service_handler("conflict"),
            ),
            Err(Error::Service(_))
        ));
        assert!(matches!(
            runtime.register_owned_service(
                "template-engine",
                "template",
                "legacy",
                Value::Null,
                service_handler("conflict"),
            ),
            Err(Error::Service(_))
        ));
        assert!(matches!(
            runtime.register_service(
                "template",
                "render",
                Value::Null,
                service_handler("legacy conflict"),
            ),
            Err(Error::Service(_))
        ));
        assert_eq!(runtime.services()["template"]["render"]["version"], 2);
        assert_eq!(
            runtime
                .call_service(ServiceCall {
                    domain: "template".into(),
                    service: "render".into(),
                    data: empty_map(),
                    target: empty_map(),
                    context: Context::default(),
                })
                .unwrap()
                .response,
            Some(Value::String("new".into()))
        );
        assert_eq!(
            runtime
                .unregister_owned_services("template-engine")
                .unwrap(),
            1
        );
        assert!(runtime.services()["template"].get("render").is_none());
        assert!(runtime.services()["template"].get("reload").is_some());
        assert!(runtime.services()["template"].get("legacy").is_some());
        assert_eq!(
            runtime
                .unregister_owned_services("template-engine")
                .unwrap(),
            0
        );
        assert_eq!(runtime.unregister_owned_services("other-owner").unwrap(), 1);
        assert!(runtime.services()["template"].get("reload").is_none());
        assert!(runtime.services()["template"].get("legacy").is_some());
    }

    #[test]
    fn service_target_resolver_handles_no_match_unknown_service_and_legacy_passthrough() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_handler = observed.clone();
        runtime
            .register_service(
                "script",
                "run",
                serde_json::json!({"name":"Run"}),
                Arc::new(move |_runtime, call| {
                    observed_handler.lock().unwrap().push(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: Some(Value::String("ran".into())),
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let resolver_counter = resolver_calls.clone();
        let _guard = runtime
            .install_service_target_resolver_if_absent(Arc::new(move |_runtime, call| {
                resolver_counter.fetch_add(1, Ordering::SeqCst);
                if call.target.contains_key("area_id") {
                    Ok(false)
                } else {
                    Ok(true)
                }
            }))
            .expect("first resolver installs");

        let context = Context {
            id: "request-context".into(),
            parent_id: None,
            user_id: Some("user".into()),
        };
        let unmatched = runtime
            .call_service(ServiceCall {
                domain: "script".into(),
                service: "run".into(),
                data: empty_map(),
                target: serde_json::json!({"area_id":"empty-room"})
                    .as_object()
                    .unwrap()
                    .clone(),
                context: context.clone(),
            })
            .unwrap();
        assert!(unmatched.changed_states.is_empty());
        assert_eq!(unmatched.response, None);
        assert_eq!(unmatched.context, context);
        assert!(observed.lock().unwrap().is_empty());

        let passthrough = ServiceCall {
            domain: "script".into(),
            service: "run".into(),
            data: serde_json::json!({"entity_id":"script.target","payload":"kept"})
                .as_object()
                .unwrap()
                .clone(),
            target: empty_map(),
            context: Context::default(),
        };
        assert_eq!(
            runtime.call_service(passthrough.clone()).unwrap().response,
            Some(Value::String("ran".into()))
        );
        assert_eq!(observed.lock().unwrap().last(), Some(&passthrough));
        assert!(matches!(
            runtime.call_service(ServiceCall {
                service: "missing".into(),
                ..passthrough
            }),
            Err(Error::Service(_))
        ));
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn service_target_resolver_normalizes_internal_calls_before_entity_dispatch() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        for (owner, entity_id) in [("one", "light.one"), ("two", "light.two")] {
            let expected_entity = entity_id.to_owned();
            let matcher_entity = expected_entity.clone();
            let seen_handler = seen.clone();
            runtime
                .register_entity_service(
                    owner,
                    "light",
                    "turn_on",
                    serde_json::json!({"name":"Turn on"}),
                    Arc::new(move |candidate| candidate == matcher_entity),
                    Arc::new(move |_runtime, call| {
                        assert_eq!(
                            call.target["entity_id"],
                            serde_json::json!([expected_entity])
                        );
                        seen_handler.lock().unwrap().push(call.target.clone());
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: Some(Value::Null),
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();
        }
        let resolver = Arc::new(|runtime: &Runtime, call: &mut ServiceCall| {
            // A nested install attempt completes immediately, proving that
            // Runtime does not hold the resolver mutex while invoking us.
            assert!(
                runtime
                    .install_service_target_resolver_if_absent(Arc::new(|_, _| Ok(true)))
                    .is_none()
            );
            assert_eq!(
                call.target.get("area_id"),
                Some(&Value::String("room".into()))
            );
            call.target.remove("area_id");
            call.target.insert(
                "entity_id".into(),
                serde_json::json!(["light.one", "light.two"]),
            );
            Ok(true)
        });
        let _guard = runtime
            .install_service_target_resolver_if_absent(resolver)
            .unwrap();
        let result = runtime
            .call_service(ServiceCall {
                domain: "light".into(),
                service: "turn_on".into(),
                data: empty_map(),
                target: serde_json::json!({"area_id":"room"})
                    .as_object()
                    .unwrap()
                    .clone(),
                context: Context::default(),
            })
            .unwrap();
        assert_eq!(result.changed_states.len(), 0);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn target_resolver_guard_is_shared_and_does_not_keep_runtime_alive() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime_with_store(store.clone());
        let first = runtime
            .install_service_target_resolver_if_absent(Arc::new(|_, _| Ok(true)))
            .unwrap();
        let first_clone = first.clone();
        assert!(
            runtime
                .install_service_target_resolver_if_absent(Arc::new(|_, _| Ok(true)))
                .is_none()
        );
        drop(first);
        assert!(
            runtime
                .install_service_target_resolver_if_absent(Arc::new(|_, _| Ok(true)))
                .is_none()
        );
        drop(first_clone);
        let last = runtime
            .install_service_target_resolver_if_absent(Arc::new(|_, _| Ok(true)))
            .expect("last first-generation lease cleared the resolver");
        drop(runtime);

        // The guard owns only a Weak runtime reference, so a new authoritative
        // Runtime can claim the same Store even before the guard is dropped.
        let rebuilt = Runtime::new(store.clone()).unwrap();
        drop(last);
        drop(rebuilt);
        assert!(Runtime::new(store).is_ok());
    }

    #[test]
    fn entity_services_split_targets_and_merge_responses_and_states() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let original_context = Context {
            id: "caller-context".into(),
            parent_id: Some("parent".into()),
            user_id: Some("user".into()),
        };
        for (owner, entity_id, response) in [
            ("provider.a", "switch.one", "first"),
            ("provider.b", "switch.two", "second"),
        ] {
            let expected_id = entity_id.to_owned();
            let expected_id_for_matcher = expected_id.clone();
            let expected_response = response.to_owned();
            let expected_context = original_context.clone();
            runtime
                .register_entity_service(
                    owner,
                    "switch",
                    "turn_on",
                    serde_json::json!({"name":"Turn on"}),
                    Arc::new(move |candidate| candidate == expected_id_for_matcher),
                    Arc::new(move |runtime, call| {
                        assert_eq!(call.target["entity_id"], serde_json::json!([expected_id]));
                        assert_eq!(
                            call.data.get("fixture"),
                            Some(&Value::String("kept".into()))
                        );
                        assert_eq!(call.context, expected_context);
                        let changed = runtime.set_state(
                            &expected_id,
                            "on",
                            empty_map(),
                            call.context.clone(),
                            false,
                        )?;
                        Ok(ServiceResult {
                            changed_states: vec![changed.state],
                            response: Some(Value::String(expected_response.clone())),
                            context: Context::default(),
                        })
                    }),
                )
                .unwrap();
        }
        let result = runtime
            .call_service(ServiceCall {
                domain: "switch".into(),
                service: "turn_on".into(),
                data: serde_json::json!({"fixture":"kept"})
                    .as_object()
                    .unwrap()
                    .clone(),
                target: serde_json::json!({"entity_id":["switch.one","switch.two"]})
                    .as_object()
                    .unwrap()
                    .clone(),
                context: original_context.clone(),
            })
            .unwrap();
        assert_eq!(result.changed_states.len(), 2);
        assert_eq!(result.changed_states[0].entity_id, "switch.one");
        assert_eq!(result.changed_states[1].entity_id, "switch.two");
        assert_eq!(result.changed_states[0].context, original_context);
        assert_eq!(
            result.response,
            Some(serde_json::json!(["first", "second"]))
        );
        assert_eq!(result.context.id, "caller-context");
    }

    #[test]
    fn entity_service_validates_every_target_before_any_handler_runs() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        runtime
            .register_entity_service(
                "owner.one",
                "light",
                "toggle",
                Value::Null,
                Arc::new(|entity_id| entity_id == "light.known"),
                Arc::new(move |_runtime, call| {
                    handler_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let error = runtime.call_service(ServiceCall {
            domain: "light".into(),
            service: "toggle".into(),
            data: empty_map(),
            target: serde_json::json!({"entity_id":["light.known","light.missing"]})
                .as_object()
                .unwrap()
                .clone(),
            context: Context::default(),
        });
        assert!(matches!(error, Err(Error::Service(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn entity_service_ambiguous_match_is_rejected_before_mutation() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        for owner in ["owner.one", "owner.two"] {
            let handler_calls = calls.clone();
            runtime
                .register_entity_service(
                    owner,
                    "sensor",
                    "refresh",
                    Value::Null,
                    Arc::new(|entity_id| entity_id == "sensor.shared"),
                    Arc::new(move |_runtime, call| {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();
        }
        let error = runtime.call_service(ServiceCall {
            domain: "sensor".into(),
            service: "refresh".into(),
            data: empty_map(),
            target: serde_json::json!({"entity_id":"sensor.shared"})
                .as_object()
                .unwrap()
                .clone(),
            context: Context::default(),
        });
        assert!(matches!(error, Err(Error::Service(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn all_entity_service_target_expands_only_current_domain_matches() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        runtime
            .set_state("switch.one", "off", empty_map(), Context::default(), false)
            .unwrap();
        runtime
            .set_state("light.other", "off", empty_map(), Context::default(), false)
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_handler = seen.clone();
        runtime
            .register_entity_service(
                "switch.provider",
                "switch",
                "turn_on",
                Value::Null,
                Arc::new(|entity_id| entity_id.starts_with("switch.")),
                Arc::new(move |_runtime, call| {
                    seen_handler
                        .lock()
                        .unwrap()
                        .push(call.target["entity_id"].clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        runtime
            .call_service(ServiceCall {
                domain: "switch".into(),
                service: "turn_on".into(),
                data: empty_map(),
                target: serde_json::json!({"entity_id":"all"})
                    .as_object()
                    .unwrap()
                    .clone(),
                context: Context::default(),
            })
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), [serde_json::json!(["switch.one"])]);
    }

    #[test]
    fn unregistering_one_owner_keeps_other_service_providers_visible() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        for (owner, entity_id) in [("owner.one", "switch.one"), ("owner.two", "switch.two")] {
            runtime
                .register_entity_service(
                    owner,
                    "switch",
                    "toggle",
                    serde_json::json!({"name":"Toggle"}),
                    Arc::new(move |candidate| candidate == entity_id),
                    Arc::new(|_runtime, call| {
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: Some(Value::Null),
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();
        }
        assert_eq!(runtime.services()["switch"]["toggle"]["name"], "Toggle");
        assert_eq!(runtime.unregister_entity_services("owner.one").unwrap(), 1);
        assert_eq!(runtime.services()["switch"]["toggle"]["name"], "Toggle");
        assert_eq!(runtime.unregister_entity_services("owner.two").unwrap(), 1);
        assert!(runtime.services().get("switch").is_none());
    }
}
