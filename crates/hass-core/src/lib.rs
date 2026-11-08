//! Native Home Assistant runtime primitives.
//!
//! This crate provides the shared state, event, persistence, and service
//! contracts used by the Rust rewrite. It implements a bounded core runtime;
//! it does not run Python integrations or control devices by itself.

mod components;
mod service_dispatch;
#[cfg(test)]
mod service_snapshot_tests;
mod service_tasks;

pub use components::LoadedComponents;
pub use service_tasks::{OwnedServiceCall, ServiceCallCancellationHandle};

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
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
    /// A data-entry submission failed field-level schema validation.
    #[error("invalid data")]
    InvalidData {
        /// Validation messages keyed by field, preserving native JSON values.
        schema_errors: Map<String, Value>,
    },
    /// A requested entity or service does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A persistence operation failed.
    #[error("storage error: {0}")]
    Storage(String),
    /// A service registration or invocation failed.
    #[error("service error: {0}")]
    Service(String),
    /// A known service rejected its response mode or resolved entity targets.
    #[error("{0}")]
    ServiceValidation(String),
    /// A domain operation raised a user-facing Home Assistant error.
    #[error("{0}")]
    HomeAssistant(String),
    /// The domain rejects this operation on the requested resource.
    #[error("{0}")]
    NotAllowed(String),
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

/// Attributes retained on the live state but omitted from recorded state
/// history. The current state and state-change event remain full-fidelity.
///
/// The native store also keeps full `state_changed` events in event history;
/// unlike Home Assistant Recorder's generic event table, it does not omit
/// those events. This policy affects only the state-history projection.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateRecordingPolicy {
    unrecorded_attributes: BTreeSet<String>,
}

impl StateRecordingPolicy {
    /// Add attribute names that should be omitted from recorded state history.
    #[must_use]
    pub fn with_unrecorded_attributes(
        mut self,
        attributes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.unrecorded_attributes
            .extend(attributes.into_iter().map(Into::into));
        self
    }

    /// Return the attribute names omitted from recorded state history.
    #[must_use]
    pub fn unrecorded_attributes(&self) -> &BTreeSet<String> {
        &self.unrecorded_attributes
    }

    /// Return a history snapshot with excluded attributes removed.
    #[must_use]
    pub fn history_state(&self, state: &State) -> State {
        let mut history_state = state.clone();
        for attribute in &self.unrecorded_attributes {
            history_state.attributes.remove(attribute);
        }
        history_state
    }

    fn is_empty(&self) -> bool {
        self.unrecorded_attributes.is_empty()
    }
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
    /// Atomically persist a full current state, its history projection, and
    /// the full state-change event.
    ///
    /// The default preserves compatibility with existing stores and forwards
    /// through `put_state`, which also keeps failure-injecting wrappers in the
    /// call path. Stores that persist state history should override this method
    /// to use `history_state` for history rows while retaining `state` for the
    /// current-state row. The event is intentionally full-fidelity.
    fn put_state_with_history(
        &self,
        state: &State,
        history_state: &State,
        event: &Event,
    ) -> Result<()> {
        let _ = history_state;
        self.put_state(state, Some(event))
    }
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

    fn put_state_with_history(
        &self,
        state: &State,
        history_state: &State,
        event: &Event,
    ) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| Error::Storage("memory store lock poisoned".into()))?;
        data.states.insert(state.entity_id.clone(), state.clone());
        data.history.push(history_state.clone());
        data.events.push(event.clone());
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
/// Future returned by an async service handler.
pub type ServiceFuture = Pin<Box<dyn Future<Output = Result<ServiceResult>> + Send + 'static>>;
/// An async handler receives owned inputs so they can safely live across `.await`.
pub type AsyncServiceHandler = Arc<dyn Fn(Runtime, ServiceCall) -> ServiceFuture + Send + Sync>;
/// Installs a trusted execution-local scope for one handler invocation/poll.
pub type ServiceExecutionScope = Arc<dyn Fn() -> Box<dyn Any + Send> + Send + Sync>;

thread_local! {
    static ACTIVE_SERVICE_EXECUTION_SCOPES: RefCell<Vec<ServiceExecutionScope>> = RefCell::new(Vec::new());
}

pub(crate) fn active_service_execution_scope() -> Option<ServiceExecutionScope> {
    ACTIVE_SERVICE_EXECUTION_SCOPES.with(|scopes| scopes.borrow().last().cloned())
}

struct ActiveServiceExecutionScopeGuard;

impl ActiveServiceExecutionScopeGuard {
    fn enter(scope: ServiceExecutionScope) -> Self {
        ACTIVE_SERVICE_EXECUTION_SCOPES.with(|scopes| scopes.borrow_mut().push(scope));
        Self
    }
}

impl Drop for ActiveServiceExecutionScopeGuard {
    fn drop(&mut self) {
        ACTIVE_SERVICE_EXECUTION_SCOPES.with(|scopes| {
            scopes
                .borrow_mut()
                .pop()
                .expect("service execution scopes are entered and left in stack order");
        });
    }
}

pub(crate) struct ServiceExecutionScopeGuard {
    _scope_guard: Box<dyn Any + Send>,
    _active_scope_guard: ActiveServiceExecutionScopeGuard,
}

impl ServiceExecutionScopeGuard {
    pub(crate) fn enter(scope: &ServiceExecutionScope) -> Self {
        let active_scope_guard = ActiveServiceExecutionScopeGuard::enter(Arc::clone(scope));
        let scope_guard = scope();
        Self {
            _scope_guard: scope_guard,
            _active_scope_guard: active_scope_guard,
        }
    }
}

/// Declares whether a service supports response-bearing calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceResponseSupport {
    /// The service does not return a response.
    None,
    /// The caller may choose whether to request a response.
    Optional,
    /// The caller must request a response.
    Only,
}
/// Authorization hook applied to every normalized entity target before dispatch.
pub type ServiceTargetAuthorizer = Arc<dyn Fn(&[String]) -> Result<()> + Send + Sync>;
/// Transport options kept outside [`ServiceCall`] for source compatibility.
pub struct ServiceCallRequest {
    pub call: ServiceCall,
    pub return_response: bool,
    pub authorize_targets: Option<ServiceTargetAuthorizer>,
}

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

#[derive(Clone)]
struct RegisteredService {
    owner: Option<String>,
    description: Value,
    response_support: ServiceResponseSupport,
    handler: ServiceHandlerKind,
}

#[derive(Clone)]
struct RegisteredEntityService {
    description: Value,
    response_support: ServiceResponseSupport,
    matcher: EntityMatcher,
    handler: ServiceHandlerKind,
}

#[derive(Clone)]
enum ServiceHandlerKind {
    Sync(ServiceHandler),
    Async(AsyncServiceHandler),
}

struct ServiceRegistrations {
    legacy: Option<RegisteredService>,
    providers: BTreeMap<String, RegisteredEntityService>,
}

impl ServiceRegistrations {
    fn response_support(&self) -> ServiceResponseSupport {
        self.legacy
            .as_ref()
            .map(|registered| registered.response_support)
            .or_else(|| {
                self.providers
                    .first_key_value()
                    .map(|(_, provider)| provider.response_support)
            })
            .unwrap_or(ServiceResponseSupport::None)
    }
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

/// Atomic location/timezone snapshot for native time and solar helpers.
/// The configuration owner persists settings before applying this view.
#[derive(Clone, Debug, PartialEq)]
pub struct HomeContext {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
    pub time_zone: String,
}

impl Default for HomeContext {
    fn default() -> Self {
        Self {
            latitude: 0.0,
            longitude: 0.0,
            elevation: 0.0,
            time_zone: "UTC".into(),
        }
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
    home_context: RwLock<HomeContext>,
    services: RwLock<BTreeMap<String, BTreeMap<String, RegisteredService>>>,
    entity_services: RwLock<BTreeMap<(String, String), BTreeMap<String, RegisteredEntityService>>>,
    service_registration: Mutex<()>,
    service_tasks: Arc<service_tasks::ServiceTaskTracker>,
    service_target_resolver: Mutex<Option<InstalledServiceTargetResolver>>,
    loaded_components: LoadedComponents,
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
                home_context: RwLock::new(HomeContext::default()),
                services: RwLock::new(BTreeMap::new()),
                entity_services: RwLock::new(BTreeMap::new()),
                service_registration: Mutex::new(()),
                service_tasks: Arc::new(service_tasks::ServiceTaskTracker::new()),
                service_target_resolver: Mutex::new(None),
                loaded_components: LoadedComponents::default(),
            }),
        })
    }

    /// Return this runtime's shared, process-lifetime component receipts.
    pub fn loaded_components(&self) -> LoadedComponents {
        self.inner.loaded_components.clone()
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
        self.home_context().time_zone
    }

    /// Read location and timezone together, including live saved config edits.
    pub fn home_context(&self) -> HomeContext {
        self.inner
            .home_context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Apply the configuration owner's already-persisted location and timezone.
    pub fn set_home_context(&self, context: HomeContext) -> Result<()> {
        if !context.latitude.is_finite()
            || !(-90.0..=90.0).contains(&context.latitude)
            || !context.longitude.is_finite()
            || !(-180.0..=180.0).contains(&context.longitude)
            || !context.elevation.is_finite()
        {
            return Err(Error::InvalidInput(
                "invalid home coordinates or elevation".into(),
            ));
        }
        context
            .time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| Error::InvalidInput("unknown IANA time zone".into()))?;
        *self
            .inner
            .home_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = context;
        Ok(())
    }

    /// Apply a validated setting after the configuration owner has persisted it.
    pub fn set_time_zone(&self, time_zone: &str) -> Result<()> {
        time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| Error::InvalidInput("unknown IANA time zone".into()))?;
        self.inner
            .home_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .time_zone = time_zone.into();
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
        self.set_state_with_recording_policy(
            entity_id,
            state,
            attributes,
            context,
            force_update,
            &StateRecordingPolicy::default(),
        )
    }

    /// Set or report an entity state while omitting configured attributes from
    /// persisted history. Live state and state-change events retain every
    /// attribute.
    pub fn set_state_with_recording_policy(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
        recording_policy: &StateRecordingPolicy,
    ) -> Result<StateChange> {
        self.set_state_checked(
            entity_id,
            state,
            attributes,
            context,
            force_update,
            recording_policy,
            None,
        )
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
        self.set_state_if_unchanged_with_recording_policy(
            entity_id,
            expected,
            state,
            attributes,
            context,
            force_update,
            &StateRecordingPolicy::default(),
        )
    }

    /// Publish only when the current snapshot matches `expected`, atomically,
    /// while applying `recording_policy` to history only.
    ///
    /// `None` requires the ID to be absent. A failed comparison leaves state,
    /// history, timestamps and events untouched and returns `Ok(None)`.
    #[allow(clippy::too_many_arguments)] // Mirrors the existing guarded state API.
    pub fn set_state_if_unchanged_with_recording_policy(
        &self,
        entity_id: &str,
        expected: Option<&State>,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
        recording_policy: &StateRecordingPolicy,
    ) -> Result<Option<StateChange>> {
        self.set_state_checked(
            entity_id,
            state,
            attributes,
            context,
            force_update,
            recording_policy,
            Some(expected),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn set_state_checked(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
        force_update: bool,
        recording_policy: &StateRecordingPolicy,
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
        if recording_policy.is_empty() {
            self.inner.store.put_state(&next, Some(&event))?;
        } else {
            let history_state = recording_policy.history_state(&next);
            self.inner
                .store
                .put_state_with_history(&next, &history_state, &event)?;
        }
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
        self.register_service_kind(
            None,
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register or replace a legacy service with an explicit response capability.
    pub fn register_service_with_response_support(
        &self,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        handler: ServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            None,
            domain,
            service,
            description,
            support,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register or replace a legacy async service handler.
    pub fn register_async_service(
        &self,
        domain: &str,
        service: &str,
        description: Value,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            None,
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            ServiceHandlerKind::Async(handler),
        )
    }
    /// Register or replace a legacy async service with an explicit response capability.
    pub fn register_async_service_with_response_support(
        &self,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            None,
            domain,
            service,
            description,
            support,
            ServiceHandlerKind::Async(handler),
        )
    }
    fn register_service_kind(
        &self,
        owner: Option<&str>,
        domain: &str,
        service: &str,
        description: Value,
        response_support: ServiceResponseSupport,
        handler: ServiceHandlerKind,
    ) -> Result<()> {
        if let Some(owner) = owner {
            validate_owner(owner)?;
        }
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        let _registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing_owner = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(domain)
            .and_then(|entries| entries.get(service))
            .and_then(|registered| registered.owner.clone());
        if existing_owner.as_deref() != owner
            && self
                .inner
                .services
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(domain)
                .and_then(|entries| entries.get(service))
                .is_some()
        {
            if owner.is_none() {
                let existing_owner = existing_owner.unwrap_or_default();
                return Err(Error::Service(format!(
                    "service {domain}.{service} is owned by {existing_owner}"
                )));
            }
            return Err(Error::Service(format!(
                "service {domain}.{service} is already registered by another owner"
            )));
        }
        self.validate_response_support(domain, service, response_support, Some(owner), None)?;
        let replaced = self
            .inner
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(domain.to_owned())
            .or_default()
            .insert(
                service.to_owned(),
                RegisteredService {
                    owner: owner.map(str::to_owned),
                    description,
                    response_support,
                    handler,
                },
            );
        drop(_registration);
        drop(replaced);
        Ok(())
    }
    fn validate_response_support(
        &self,
        domain: &str,
        service: &str,
        response_support: ServiceResponseSupport,
        replaced_service_owner: Option<Option<&str>>,
        replaced_entity_owner: Option<&str>,
    ) -> Result<()> {
        let legacy = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(domain)
            .and_then(|entries| entries.get(service))
            .filter(|registered| {
                replaced_service_owner.is_none_or(|owner| registered.owner.as_deref() != owner)
            })
            .map(|registered| registered.response_support);
        let entities = self
            .inner
            .entity_services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entity_conflict = entities
            .get(&(domain.to_owned(), service.to_owned()))
            .into_iter()
            .flat_map(|providers| providers.iter())
            .any(|(owner, provider)| {
                Some(owner.as_str()) != replaced_entity_owner
                    && provider.response_support != response_support
            });
        if legacy.is_some_and(|support| support != response_support) || entity_conflict {
            return Err(Error::Service(format!(
                "service {domain}.{service} response support conflicts with an existing registration"
            )));
        }
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
        self.register_service_kind(
            Some(owner),
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register or update an owned synchronous service with a response capability.
    pub fn register_owned_service_with_response_support(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        handler: ServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            Some(owner),
            domain,
            service,
            description,
            support,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register or update an async service owned by one integration.
    pub fn register_owned_async_service(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            Some(owner),
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            ServiceHandlerKind::Async(handler),
        )
    }
    /// Register or update an owned async service with a response capability.
    pub fn register_owned_async_service_with_response_support(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_service_kind(
            Some(owner),
            domain,
            service,
            description,
            support,
            ServiceHandlerKind::Async(handler),
        )
    }

    /// Remove the legacy services registered by `owner`, leaving every other
    /// owner's and every legacy registration in place.
    pub fn unregister_owned_services(&self, owner: &str) -> Result<usize> {
        validate_owner(owner)?;
        let registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = {
            let mut services = self
                .inner
                .services
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut removed = Vec::new();
            for entries in services.values_mut() {
                let keys = entries
                    .iter()
                    .filter(|(_, entry)| entry.owner.as_deref() == Some(owner))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                for name in keys {
                    if let Some(entry) = entries.remove(&name) {
                        removed.push(entry);
                    }
                }
            }
            services.retain(|_, entries| !entries.is_empty());
            removed
        };
        let count = removed.len();
        // A captured resource may unregister another owner in its destructor.
        // Release every registry lock before dropping callbacks.
        drop(registration);
        drop(removed);
        Ok(count)
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
        self.register_entity_service_kind(
            owner,
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            matcher,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register an owner-scoped sync entity provider with an explicit response capability.
    // This mirrors the established registration fields plus one typed capability.
    #[allow(clippy::too_many_arguments)]
    pub fn register_entity_service_with_response_support(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        matcher: EntityMatcher,
        handler: ServiceHandler,
    ) -> Result<()> {
        self.register_entity_service_kind(
            owner,
            domain,
            service,
            description,
            support,
            matcher,
            ServiceHandlerKind::Sync(handler),
        )
    }
    /// Register an owner-scoped async provider for calls targeting its entities.
    pub fn register_entity_async_service(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        matcher: EntityMatcher,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_entity_service_kind(
            owner,
            domain,
            service,
            description,
            ServiceResponseSupport::None,
            matcher,
            ServiceHandlerKind::Async(handler),
        )
    }
    /// Register an owner-scoped async entity provider with a response capability.
    // Keep the async form aligned with the sync registration contract.
    #[allow(clippy::too_many_arguments)]
    pub fn register_entity_async_service_with_response_support(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        support: ServiceResponseSupport,
        matcher: EntityMatcher,
        handler: AsyncServiceHandler,
    ) -> Result<()> {
        self.register_entity_service_kind(
            owner,
            domain,
            service,
            description,
            support,
            matcher,
            ServiceHandlerKind::Async(handler),
        )
    }
    // Shared implementation for the same explicit provider registration tuple.
    #[allow(clippy::too_many_arguments)]
    fn register_entity_service_kind(
        &self,
        owner: &str,
        domain: &str,
        service: &str,
        description: Value,
        response_support: ServiceResponseSupport,
        matcher: EntityMatcher,
        handler: ServiceHandlerKind,
    ) -> Result<()> {
        validate_owner(owner)?;
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        let _registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_response_support(domain, service, response_support, None, Some(owner))?;
        let replaced = self
            .inner
            .entity_services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((domain.to_owned(), service.to_owned()))
            .or_default()
            .insert(
                owner.to_owned(),
                RegisteredEntityService {
                    description,
                    response_support,
                    matcher,
                    handler,
                },
            );
        drop(_registration);
        drop(replaced);
        Ok(())
    }

    /// Remove all service providers registered by one owner and return the
    /// number of owner/service registrations removed.
    pub fn unregister_entity_services(&self, owner: &str) -> Result<usize> {
        validate_owner(owner)?;
        let registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = {
            let mut services = self
                .inner
                .entity_services
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut removed = Vec::new();
            services.retain(|_, providers| {
                if let Some(provider) = providers.remove(owner) {
                    removed.push(provider);
                }
                !providers.is_empty()
            });
            removed
        };
        let count = removed.len();
        // A captured resource may unregister another owner in its destructor.
        // Release every registry lock before dropping callbacks.
        drop(registration);
        drop(removed);
        Ok(count)
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
    use std::cell::Cell;
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

    fn service_call(domain: &str, service: &str, target: Map<String, Value>) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: empty_map(),
            target,
            context: Context {
                id: "async-service-context".into(),
                parent_id: Some("async-service-parent".into()),
                user_id: Some("async-service-user".into()),
            },
        }
    }

    struct ScopeCountGuard(Arc<AtomicUsize>);

    impl Drop for ScopeCountGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    thread_local! {
        static TEST_EXECUTION_SCOPE_VALUE: Cell<Option<u8>> = const { Cell::new(None) };
    }

    struct TestExecutionScopeGuard {
        previous: Option<u8>,
        active: Arc<AtomicUsize>,
    }

    impl Drop for TestExecutionScopeGuard {
        fn drop(&mut self) {
            TEST_EXECUTION_SCOPE_VALUE.with(|value| value.set(self.previous));
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }

    #[test]
    fn home_context_is_shared_validated_and_updated_as_one_snapshot() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        assert_eq!(runtime.home_context(), HomeContext::default());
        let clone = runtime.clone();
        let context = HomeContext {
            latitude: 42.0,
            longitude: -83.0,
            elevation: -20.0,
            time_zone: "America/Detroit".into(),
        };
        runtime.set_home_context(context.clone()).unwrap();
        assert_eq!(clone.home_context(), context);
        for invalid in [
            HomeContext {
                latitude: 91.0,
                ..context.clone()
            },
            HomeContext {
                longitude: f64::NAN,
                ..context.clone()
            },
            HomeContext {
                elevation: f64::INFINITY,
                ..context.clone()
            },
            HomeContext {
                time_zone: "invalid/timezone".into(),
                ..context.clone()
            },
        ] {
            assert!(clone.set_home_context(invalid).is_err());
            assert_eq!(runtime.home_context(), context);
        }
        runtime.set_time_zone("Asia/Tokyo").unwrap();
        assert_eq!(
            clone.home_context(),
            HomeContext {
                time_zone: "Asia/Tokyo".into(),
                ..context
            }
        );
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
    fn state_recording_policy_only_projects_history_attributes() {
        let store = Arc::new(MemoryStore::default());
        let runtime = runtime_with_store(store.clone());
        let policy = StateRecordingPolicy::default().with_unrecorded_attributes(["tag_id"]);
        let mut attributes = Map::new();
        attributes.insert("tag_id".into(), Value::String("fixture-tag".into()));
        attributes.insert("device_id".into(), Value::String("scanner-1".into()));
        let mut events = runtime.subscribe();

        let _first = runtime
            .set_state_with_recording_policy(
                "tag.fixture",
                "2026-11-06T12:00:00+00:00",
                attributes.clone(),
                Context::default(),
                false,
                &policy,
            )
            .unwrap();
        let public_event = events.try_recv().unwrap();
        assert_eq!(
            public_event.data["new_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );
        assert_eq!(
            runtime.get_state("tag.fixture").unwrap().attributes["tag_id"],
            "fixture-tag"
        );
        let history = store
            .history(
                "tag.fixture",
                DateTime::<Utc>::MIN_UTC,
                DateTime::<Utc>::MAX_UTC,
            )
            .unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].attributes.contains_key("tag_id"));
        assert_eq!(history[0].attributes["device_id"], "scanner-1");
        let events_in_store = store
            .event_history_query(&EventHistoryQuery {
                start: DateTime::<Utc>::MIN_UTC,
                end: DateTime::<Utc>::MAX_UTC,
                event_types: Some(vec!["state_changed".into()]),
            })
            .unwrap();
        assert_eq!(
            events_in_store[0].data["new_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );

        let unchanged = runtime
            .set_state_with_recording_policy(
                "tag.fixture",
                "2026-11-06T12:00:00+00:00",
                attributes.clone(),
                Context::default(),
                false,
                &policy,
            )
            .unwrap();
        assert!(!unchanged.changed);
        assert_eq!(
            store
                .history(
                    "tag.fixture",
                    DateTime::<Utc>::MIN_UTC,
                    DateTime::<Utc>::MAX_UTC
                )
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            runtime.set_state_if_unchanged_with_recording_policy(
                "tag.fixture",
                None,
                "ignored",
                attributes.clone(),
                Context::default(),
                false,
                &policy,
            ),
            Ok(None)
        ));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        let forced = runtime
            .set_state_with_recording_policy(
                "tag.fixture",
                "2026-11-06T12:00:00+00:00",
                attributes,
                Context::default(),
                true,
                &policy,
            )
            .unwrap();
        assert!(forced.changed);
        assert_eq!(
            events.try_recv().unwrap().data["new_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );
        assert_eq!(
            store
                .history(
                    "tag.fixture",
                    DateTime::<Utc>::MIN_UTC,
                    DateTime::<Utc>::MAX_UTC
                )
                .unwrap()
                .len(),
            2
        );
        let guarded = runtime
            .set_state_if_unchanged_with_recording_policy(
                "tag.fixture",
                Some(&forced.state),
                "cleared",
                serde_json::json!({"tag_id":"fixture-tag"})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
                false,
                &policy,
            )
            .unwrap()
            .unwrap();
        assert_eq!(runtime.get_state("tag.fixture"), Some(guarded.state));
        assert_eq!(
            store
                .history(
                    "tag.fixture",
                    DateTime::<Utc>::MIN_UTC,
                    DateTime::<Utc>::MAX_UTC
                )
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn recording_policy_default_store_hook_preserves_failure_injection() {
        let store = Arc::new(FallibleStore {
            inner: MemoryStore::default(),
            fail_writes: AtomicBool::new(true),
        });
        let runtime = runtime_with_store(store.clone());
        let mut events = runtime.subscribe();
        let policy = StateRecordingPolicy::default().with_unrecorded_attributes(["tag_id"]);
        let mut attributes = Map::new();
        attributes.insert("tag_id".into(), Value::String("fixture-tag".into()));

        assert!(matches!(
            runtime.set_state_with_recording_policy(
                "tag.fixture",
                "unknown",
                attributes,
                Context::default(),
                false,
                &policy,
            ),
            Err(Error::Storage(_))
        ));
        assert!(runtime.get_state("tag.fixture").is_none());
        assert!(store.inner.load_states().unwrap().is_empty());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
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
        assert!(matches!(error, Err(Error::ServiceValidation(_))));
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
        assert!(matches!(error, Err(Error::ServiceValidation(_))));
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

    #[tokio::test]
    async fn async_dispatch_mixes_entity_handlers_and_preflights_once() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let sync_contexts = Arc::new(Mutex::new(Vec::new()));
        let sync_seen = Arc::clone(&sync_contexts);
        runtime
            .register_entity_service_with_response_support(
                "owner.a",
                "light",
                "operate",
                serde_json::json!({"name":"Operate"}),
                ServiceResponseSupport::Optional,
                Arc::new(|entity_id| entity_id == "light.one"),
                Arc::new(move |_runtime, call| {
                    sync_seen.lock().unwrap().push(call.context.clone());
                    Ok(ServiceResult {
                        changed_states: vec![test_state("light.one", "on", 100)],
                        response: Some(serde_json::json!("sync")),
                        context: Context::default(),
                    })
                }),
            )
            .unwrap();
        let async_contexts = Arc::new(Mutex::new(Vec::new()));
        let async_seen = Arc::clone(&async_contexts);
        runtime
            .register_entity_async_service_with_response_support(
                "owner.b",
                "light",
                "operate",
                serde_json::json!({"name":"Operate"}),
                ServiceResponseSupport::Optional,
                Arc::new(|entity_id| entity_id == "light.two"),
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    let seen = Arc::clone(&async_seen);
                    Box::pin(async move {
                        seen.lock().unwrap().push(call.context.clone());
                        Ok(ServiceResult {
                            changed_states: vec![test_state("light.two", "off", 101)],
                            response: Some(serde_json::json!("async")),
                            context: Context::default(),
                        })
                    })
                }),
            )
            .unwrap();

        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let resolver_count = Arc::clone(&resolver_calls);
        let _resolver = runtime
            .install_service_target_resolver_if_absent(Arc::new(move |_runtime, call| {
                resolver_count.fetch_add(1, Ordering::SeqCst);
                call.target.insert(
                    "entity_id".into(),
                    serde_json::json!(["light.one", "light.two"]),
                );
                Ok(true)
            }))
            .expect("resolver installs");
        let authorized = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let authorized_ids = Arc::clone(&authorized);
        let authorize_targets: ServiceTargetAuthorizer = Arc::new(move |entity_ids| {
            authorized_ids.lock().unwrap().push(entity_ids.to_vec());
            Ok(())
        });
        let call = service_call("light", "operate", empty_map());
        let result = runtime
            .call_service_request_async(ServiceCallRequest {
                call: call.clone(),
                return_response: true,
                authorize_targets: Some(authorize_targets),
            })
            .await
            .unwrap();

        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *authorized.lock().unwrap(),
            [vec!["light.one".to_owned(), "light.two".to_owned()]]
        );
        assert_eq!(
            runtime.service_response_support("light", "operate"),
            Some(ServiceResponseSupport::Optional)
        );
        assert_eq!(result.response, Some(serde_json::json!(["sync", "async"])));
        assert_eq!(result.changed_states.len(), 2);
        assert_eq!(result.context, call.context);
        assert_eq!(
            sync_contexts.lock().unwrap().as_slice(),
            std::slice::from_ref(&call.context)
        );
        assert_eq!(*async_contexts.lock().unwrap(), [call.context]);
        assert_eq!(runtime.services()["light"]["operate"]["name"], "Operate");
    }

    #[tokio::test]
    async fn response_capabilities_gate_calls_and_reject_inconsistent_providers() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let sync_effects = Arc::clone(&effects);
        runtime
            .register_service(
                "demo",
                "plain",
                serde_json::json!({"name":"Plain"}),
                Arc::new(move |_runtime, call| {
                    sync_effects.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let unsupported = runtime
            .call_service_request_async(ServiceCallRequest {
                call: service_call("demo", "plain", empty_map()),
                return_response: true,
                authorize_targets: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            unsupported,
            Error::ServiceValidation(
                "Service does not support responses. Remove return_response from request.".into()
            )
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);

        runtime
            .register_async_service_with_response_support(
                "demo",
                "required",
                serde_json::json!({"name":"Required"}),
                ServiceResponseSupport::Only,
                Arc::new(|_runtime, call| -> ServiceFuture {
                    Box::pin(async move {
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: Some(Value::Null),
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();
        let required = runtime
            .call_service_request_async(ServiceCallRequest {
                call: service_call("demo", "required", empty_map()),
                return_response: false,
                authorize_targets: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            required,
            Error::ServiceValidation(
                "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters."
                    .into()
            )
        );

        runtime
            .register_async_service_with_response_support(
                "demo",
                "optional",
                serde_json::json!({"name":"Optional"}),
                ServiceResponseSupport::Optional,
                Arc::new(|_runtime, call| -> ServiceFuture {
                    Box::pin(async move {
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: Some(serde_json::json!("optional")),
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();
        for return_response in [false, true] {
            let result = runtime
                .call_service_request_async(ServiceCallRequest {
                    call: service_call("demo", "optional", empty_map()),
                    return_response,
                    authorize_targets: None,
                })
                .await
                .unwrap();
            assert_eq!(
                result.response,
                return_response.then(|| serde_json::json!("optional"))
            );
        }

        runtime
            .register_entity_service_with_response_support(
                "owner.a",
                "light",
                "operate",
                serde_json::json!({"name":"Operate"}),
                ServiceResponseSupport::Optional,
                Arc::new(|_| true),
                Arc::new(|_runtime, call| {
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let inconsistent = runtime.register_entity_async_service(
            "owner.b",
            "light",
            "operate",
            serde_json::json!({"name":"Operate"}),
            Arc::new(|_| true),
            Arc::new(|_runtime, call| -> ServiceFuture {
                Box::pin(async move {
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context,
                    })
                })
            }),
        );
        assert!(
            matches!(inconsistent, Err(Error::Service(message)) if message.contains("response support conflicts"))
        );
        assert_eq!(
            runtime.service_response_support("light", "operate"),
            Some(ServiceResponseSupport::Optional)
        );
        assert_eq!(runtime.services()["demo"]["plain"]["name"], "Plain");
        runtime
            .register_entity_service_with_response_support(
                "owner.a",
                "light",
                "operate",
                serde_json::json!({"name":"Updated operate"}),
                ServiceResponseSupport::Only,
                Arc::new(|_| true),
                Arc::new(|_runtime, call| {
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: Some(Value::Null),
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        assert_eq!(
            runtime.service_response_support("light", "operate"),
            Some(ServiceResponseSupport::Only)
        );
        assert_eq!(
            runtime.services()["light"]["operate"]["name"],
            "Updated operate"
        );
        assert_eq!(runtime.unregister_entity_services("owner.a").unwrap(), 1);
        assert_eq!(runtime.service_response_support("light", "operate"), None);
    }

    #[tokio::test]
    async fn async_only_handler_is_rejected_by_sync_api_before_effects() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let async_effects = Arc::clone(&effects);
        runtime
            .register_async_service(
                "demo",
                "async_only",
                Value::Null,
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    async_effects.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();
        let call = service_call("demo", "async_only", empty_map());
        let error = runtime.call_service(call.clone()).unwrap_err();
        assert!(
            matches!(error, Error::Service(message) if message.contains("requires asynchronous dispatch"))
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        runtime.call_service_async(call).await.unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn target_validation_and_authorization_finish_before_async_handlers() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let handler_effects = Arc::new(AtomicUsize::new(0));
        let effects = Arc::clone(&handler_effects);
        runtime
            .register_entity_async_service(
                "owner.light",
                "light",
                "turn_on",
                Value::Null,
                Arc::new(|entity_id| entity_id == "light.one"),
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();

        let auth_calls = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::clone(&auth_calls);
        let deny_targets = Arc::new(AtomicBool::new(false));
        let should_deny = Arc::clone(&deny_targets);
        let authorize: ServiceTargetAuthorizer = Arc::new(move |_entity_ids| {
            auth_count.fetch_add(1, Ordering::SeqCst);
            if should_deny.load(Ordering::SeqCst) {
                Err(Error::InvalidInput("fixture authorization denied".into()))
            } else {
                Ok(())
            }
        });
        let invalid_target = runtime
            .call_service_request_async(ServiceCallRequest {
                call: service_call(
                    "light",
                    "turn_on",
                    serde_json::json!({"entity_id":["light.one", "light.missing"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                return_response: false,
                authorize_targets: Some(Arc::clone(&authorize)),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(invalid_target, Error::ServiceValidation(message) if message.contains("no provider for target light.missing"))
        );
        assert_eq!(auth_calls.load(Ordering::SeqCst), 0);
        assert_eq!(handler_effects.load(Ordering::SeqCst), 0);

        deny_targets.store(true, Ordering::SeqCst);
        let denied = runtime
            .call_service_request_async(ServiceCallRequest {
                call: service_call(
                    "light",
                    "turn_on",
                    serde_json::json!({"entity_id":"light.one"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                return_response: false,
                authorize_targets: Some(authorize),
            })
            .await
            .unwrap_err();
        assert_eq!(
            denied,
            Error::InvalidInput("fixture authorization denied".into())
        );
        assert_eq!(auth_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler_effects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn execution_scope_wraps_handler_creation_polls_and_sync_fallback() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let active = Arc::new(AtomicUsize::new(0));
        let scope_entries = Arc::new(AtomicUsize::new(0));
        let scope_active = Arc::clone(&active);
        let entries = Arc::clone(&scope_entries);
        let scope: ServiceExecutionScope = Arc::new(move || {
            entries.fetch_add(1, Ordering::SeqCst);
            scope_active.fetch_add(1, Ordering::SeqCst);
            Box::new(ScopeCountGuard(Arc::clone(&scope_active)))
        });
        let async_active = Arc::clone(&active);
        runtime
            .register_async_service(
                "demo",
                "scoped_async",
                Value::Null,
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    assert_eq!(async_active.load(Ordering::SeqCst), 1);
                    let active = Arc::clone(&async_active);
                    Box::pin(async move {
                        assert_eq!(active.load(Ordering::SeqCst), 1);
                        tokio::task::yield_now().await;
                        assert_eq!(active.load(Ordering::SeqCst), 1);
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();
        let sync_active = Arc::clone(&active);
        runtime
            .register_service(
                "demo",
                "scoped_sync",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    assert_eq!(sync_active.load(Ordering::SeqCst), 1);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();

        runtime
            .call_service_async_scoped(
                service_call("demo", "scoped_async", empty_map()),
                Arc::clone(&scope),
            )
            .await
            .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(scope_entries.load(Ordering::SeqCst) >= 3);
        runtime
            .call_service_async_scoped(service_call("demo", "scoped_sync", empty_map()), scope)
            .await
            .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn canceled_async_handler_future_is_dropped_with_request() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let dropped = Arc::new(AtomicBool::new(false));
        let handler_dropped = Arc::clone(&dropped);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let handler_started = Arc::clone(&started_tx);
        runtime
            .register_async_service(
                "demo",
                "pending",
                Value::Null,
                Arc::new(move |_runtime, _call| -> ServiceFuture {
                    let dropped = Arc::clone(&handler_dropped);
                    let started = handler_started.lock().unwrap().take();
                    Box::pin(async move {
                        let _drop_flag = DropFlag(dropped);
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        std::future::pending::<Result<ServiceResult>>().await
                    })
                }),
            )
            .unwrap();
        let call_runtime = runtime.clone();
        let call = tokio::spawn(async move {
            call_runtime
                .call_service_async(service_call("demo", "pending", empty_map()))
                .await
        });
        started_rx.await.unwrap();
        call.abort();
        let _ = call.await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropped_owned_async_waiter_detaches_and_shutdown_joins_call() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let handler_started = Arc::clone(&started_tx);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let handler_release = Arc::clone(&release_rx);
        let handler_finished = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&handler_finished);
        runtime
            .register_async_service(
                "demo",
                "owned_waiter_drop",
                Value::Null,
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    let started = handler_started.lock().unwrap().take();
                    let release = handler_release.lock().unwrap().take();
                    let finished = Arc::clone(&finished);
                    Box::pin(async move {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        release.expect("release receiver is present").await.unwrap();
                        finished.store(true, Ordering::SeqCst);
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();

        let owned = runtime
            .spawn_service_call_async(service_call("demo", "owned_waiter_drop", empty_map()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("owned async handler did not start")
            .unwrap();
        drop(owned);

        let shutdown_runtime = runtime.clone();
        let shutdown =
            tokio::spawn(async move { shutdown_runtime.shutdown_service_tasks_async().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runtime.inner.service_tasks.admission_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown did not close owned task admission");
        assert!(!shutdown.is_finished());
        assert!(!handler_finished.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown did not join the detached owned call")
            .unwrap()
            .unwrap();
        assert!(handler_finished.load(Ordering::SeqCst));
        assert!(matches!(
            runtime.spawn_service_call_async(service_call("demo", "owned_waiter_drop", empty_map())),
            Err(Error::Service(message)) if message.contains("admission is closed")
        ));
        assert_eq!(runtime.inner.service_tasks.pending_count(), 0);
    }

    #[tokio::test]
    async fn explicit_owned_call_cancellation_drops_callback_without_shutdown_failure() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let dropped = Arc::new(AtomicBool::new(false));
        let handler_dropped = Arc::clone(&dropped);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let handler_started = Arc::clone(&started_tx);
        runtime
            .register_async_service(
                "demo",
                "owned_cancel",
                Value::Null,
                Arc::new(move |_runtime, _call| -> ServiceFuture {
                    let dropped = Arc::clone(&handler_dropped);
                    let started = handler_started.lock().unwrap().take();
                    Box::pin(async move {
                        let _drop_flag = DropFlag(dropped);
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        std::future::pending::<Result<ServiceResult>>().await
                    })
                }),
            )
            .unwrap();

        let mut owned = runtime
            .spawn_service_call_async(service_call("demo", "owned_cancel", empty_map()))
            .unwrap();
        let cancellation = owned.cancellation_handle();
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("owned async handler did not start")
            .unwrap();
        cancellation.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), owned.result())
            .await
            .expect("canceled owned call did not resolve its waiter");
        assert!(matches!(
            result,
            Err(Error::Service(message)) if message.contains("owned async service call was canceled")
        ));
        assert!(dropped.load(Ordering::SeqCst));
        for _ in 0..100 {
            if runtime.inner.service_tasks.pending_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.inner.service_tasks.pending_count(), 0);
        let shutdown = runtime.shutdown_service_tasks_async().await;
        assert!(
            shutdown.is_ok(),
            "completed intentional cancellation failed shutdown: {shutdown:?}"
        );
    }

    #[tokio::test]
    async fn first_owned_call_error_does_not_cancel_an_unrelated_owned_call() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        runtime
            .register_async_service(
                "demo",
                "owned_error",
                Value::Null,
                Arc::new(|_runtime, _call| -> ServiceFuture {
                    Box::pin(async { Err(Error::Service("first provider failed".to_owned())) })
                }),
            )
            .unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let handler_started = Arc::clone(&started_tx);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let handler_release = Arc::clone(&release_rx);
        let handler_finished = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&handler_finished);
        runtime
            .register_async_service(
                "demo",
                "owned_sibling",
                Value::Null,
                Arc::new(move |_runtime, call| -> ServiceFuture {
                    let started = handler_started.lock().unwrap().take();
                    let release = handler_release.lock().unwrap().take();
                    let finished = Arc::clone(&finished);
                    Box::pin(async move {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        release.expect("release receiver is present").await.unwrap();
                        finished.store(true, Ordering::SeqCst);
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();

        let mut pending = runtime
            .spawn_service_call_async(service_call("demo", "owned_sibling", empty_map()))
            .unwrap();
        let mut failing = runtime
            .spawn_service_call_async(service_call("demo", "owned_error", empty_map()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("unrelated owned call did not start")
            .unwrap();
        assert_eq!(
            failing.result().await.unwrap_err(),
            Error::Service("first provider failed".to_owned())
        );
        assert!(!handler_finished.load(Ordering::SeqCst));

        let shutdown_runtime = runtime.clone();
        let shutdown =
            tokio::spawn(async move { shutdown_runtime.shutdown_service_tasks_async().await });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        assert!(!handler_finished.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown did not wait for the unrelated owned call")
            .unwrap()
            .unwrap();
        assert!(handler_finished.load(Ordering::SeqCst));
        assert!(pending.result().await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_nested_call_carries_execution_scope_across_polls_and_restores_it() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let active = Arc::new(AtomicUsize::new(0));
        let scope_active = Arc::clone(&active);
        let scope: ServiceExecutionScope = Arc::new(move || {
            scope_active.fetch_add(1, Ordering::SeqCst);
            let previous = TEST_EXECUTION_SCOPE_VALUE.with(|value| value.replace(Some(7)));
            Box::new(TestExecutionScopeGuard {
                previous,
                active: Arc::clone(&scope_active),
            })
        });

        runtime
            .register_async_service(
                "demo",
                "owned_scoped_child",
                Value::Null,
                Arc::new(|_runtime, call| -> ServiceFuture {
                    assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                    Box::pin(async move {
                        assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                        tokio::task::yield_now().await;
                        assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();

        runtime
            .register_async_service(
                "demo",
                "owned_scoped_parent",
                Value::Null,
                Arc::new(|runtime, call| -> ServiceFuture {
                    assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                    let mut child = runtime
                        .spawn_service_call_async(service_call(
                            "demo",
                            "owned_scoped_child",
                            empty_map(),
                        ))
                        .expect("nested owned call should be admitted");
                    Box::pin(async move {
                        assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                        child.result().await?;
                        assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), Some(7));
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context,
                        })
                    })
                }),
            )
            .unwrap();

        runtime
            .call_service_async_scoped(
                service_call("demo", "owned_scoped_parent", empty_map()),
                scope,
            )
            .await
            .unwrap();
        assert_eq!(TEST_EXECUTION_SCOPE_VALUE.with(Cell::get), None);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        runtime.shutdown_service_tasks_async().await.unwrap();
    }

    #[tokio::test]
    async fn owned_async_panics_are_observed_by_waiters_and_shutdown() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        runtime
            .register_async_service(
                "demo",
                "owned_panic",
                Value::Null,
                Arc::new(|_runtime, call| -> ServiceFuture {
                    let should_panic = call.service == "owned_panic";
                    let context = call.context;
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        if should_panic {
                            panic!("owned async panic fixture");
                        }
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context,
                        })
                    })
                }),
            )
            .unwrap();
        let mut owned = runtime
            .spawn_service_call_async(service_call("demo", "owned_panic", empty_map()))
            .unwrap();

        let shutdown = runtime.shutdown_service_tasks_async().await.unwrap_err();
        assert!(shutdown.to_string().contains("owned async panic fixture"));
        let result = owned.result().await.unwrap_err();
        assert!(result.to_string().contains("owned async panic fixture"));
        assert!(matches!(
            runtime.spawn_service_call_async(service_call("demo", "owned_panic", empty_map())),
            Err(Error::Service(message)) if message.contains("admission is closed")
        ));
    }

    #[tokio::test]
    async fn detached_owned_panic_is_retained_until_shutdown() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        runtime
            .register_async_service(
                "demo",
                "detached_owned_panic",
                Value::Null,
                Arc::new(|_runtime, call| -> ServiceFuture {
                    let context = call.context;
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        if context.id == "async-service-context" {
                            panic!("detached owned async panic fixture");
                        }
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context,
                        })
                    })
                }),
            )
            .unwrap();

        let owned = runtime
            .spawn_service_call_async(service_call("demo", "detached_owned_panic", empty_map()))
            .unwrap();
        drop(owned);
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.inner.service_tasks.pending_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached async task did not finish before shutdown");
        assert_eq!(runtime.inner.service_tasks.pending_count(), 0);

        let shutdown = runtime.shutdown_service_tasks_async().await.unwrap_err();
        assert!(
            shutdown
                .to_string()
                .contains("detached owned async panic fixture")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_sync_call_is_joined_by_shared_shutdown_and_reaped() {
        let runtime = runtime_with_store(Arc::new(MemoryStore::default()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let handler_started = Arc::clone(&started_tx);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release_on_drop = ReleaseOnDrop(Some(release_tx));
        let release_rx = Arc::new(Mutex::new(release_rx));
        let handler_finished = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&handler_finished);
        runtime
            .register_service(
                "demo",
                "block",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    if let Some(started) = handler_started.lock().unwrap().take() {
                        let _ = started.send(());
                    }
                    release_rx.lock().unwrap().recv().unwrap();
                    finished.store(true, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let call_runtime = runtime.clone();
        let call = tokio::spawn(async move {
            call_runtime
                .call_service_async(service_call("demo", "block", empty_map()))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("blocking service handler did not start")
            .unwrap();
        call.abort();
        let _ = call.await;

        let shutdown_runtime = runtime.clone();
        let canceled_waiter =
            tokio::spawn(async move { shutdown_runtime.shutdown_service_tasks_async().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runtime.inner.service_tasks.admission_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown did not close service task admission");
        let denied = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.call_service_async(service_call("demo", "block", empty_map())),
        )
        .await
        .expect("closed admission probe did not return")
        .unwrap_err();
        assert!(
            matches!(denied, Error::Service(message) if message.contains("admission is closed"))
        );
        assert!(!handler_finished.load(Ordering::SeqCst));
        canceled_waiter.abort();
        let _ = canceled_waiter.await;

        let shutdown_runtime = runtime.clone();
        let shared_waiter =
            tokio::spawn(async move { shutdown_runtime.shutdown_service_tasks_async().await });
        tokio::task::yield_now().await;
        assert!(!shared_waiter.is_finished());
        release_on_drop
            .0
            .take()
            .expect("release token is present")
            .send(())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), shared_waiter)
            .await
            .expect("shutdown did not join the blocking handler")
            .unwrap()
            .unwrap();
        assert!(handler_finished.load(Ordering::SeqCst));
        runtime.shutdown_service_tasks_async().await.unwrap();
        for _ in 0..100 {
            if runtime.inner.service_tasks.pending_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.inner.service_tasks.pending_count(), 0);
    }
}
