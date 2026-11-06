//! Native Home Assistant runtime primitives.
//!
//! This crate provides the shared state, event, persistence, and service
//! contracts used by the Rust rewrite. It implements a bounded core runtime;
//! it does not run Python integrations or control devices by itself.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

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
    fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<State>>;
}

#[derive(Default)]
struct MemoryStoreData {
    states: HashMap<String, State>,
    history: Vec<State>,
    events: Vec<Event>,
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

struct RegisteredService {
    description: Value,
    handler: ServiceHandler,
}

// A store has one authoritative runtime cache. Runtime clones share that cache;
// independent constructors must not silently cache stale versions of one store.
static ACTIVE_STORES: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();

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
    services: RwLock<BTreeMap<String, BTreeMap<String, RegisteredService>>>,
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
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                _store_lease: lease,
                store,
                states: RwLock::new(states),
                write_order: Mutex::new(()),
                events,
                services: RwLock::new(BTreeMap::new()),
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

    /// Subscribe to events emitted after successful persistence.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
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
            return Ok(StateChange {
                state: reported,
                created: false,
                changed: false,
            });
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
        Ok(StateChange {
            state: next,
            created,
            changed,
        })
    }

    /// Remove an entity's current state, retaining its historical snapshots.
    pub fn remove_state(&self, entity_id: &str, context: Context) -> Result<bool> {
        validate_entity_id(entity_id)?;
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(previous) = self.get_state(entity_id) else {
            return Ok(false);
        };
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
            origin: "LOCAL".into(),
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

    /// Register or replace a synchronous service handler.
    pub fn register_service(
        &self,
        domain: &str,
        service: &str,
        description: Value,
        handler: ServiceHandler,
    ) -> Result<()> {
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        self.inner
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(domain.to_owned())
            .or_default()
            .insert(
                service.to_owned(),
                RegisteredService {
                    description,
                    handler,
                },
            );
        Ok(())
    }

    /// Invoke a registered service without holding the registry lock.
    pub fn call_service(&self, call: ServiceCall) -> Result<ServiceResult> {
        validate_component(&call.domain, "service domain")?;
        validate_component(&call.service, "service name")?;
        let handler = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&call.domain)
            .and_then(|services| services.get(&call.service))
            .map(|registered| Arc::clone(&registered.handler))
            .ok_or_else(|| {
                Error::Service(format!("unknown service {}.{}", call.domain, call.service))
            })?;
        handler(self, &call)
    }

    /// Return service descriptions as a JSON object keyed by domain and name.
    pub fn services(&self) -> Value {
        let services = self
            .inner
            .services
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
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn empty_map() -> Map<String, Value> {
        Map::new()
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

        fn history(
            &self,
            entity_id: &str,
            start: DateTime<Utc>,
            end: DateTime<Utc>,
        ) -> Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }
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
}
