//! Native Home Assistant person integration.
//!
//! People are derived only from device-tracker states already published to
//! [`Runtime`]. The crate performs no device I/O and has no Python bridge.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use hass_core::{Context, Error, Event, Result, Runtime, State};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// Private Runtime signal for stored-person collection changes.
pub const PERSON_COLLECTION_UPDATED_SIGNAL: &str = "person_collection_updated";

const STORAGE_KEY: &str = "hass-person.collection.v1";
const STORAGE_VERSION: u32 = 1;
const TRACKER_ENTITY_PREFIX: &str = "device_tracker.";
const HOME_ENTITY_ID: &str = "zone.home";
const STATE_UNKNOWN: &str = "unknown";
const STATE_UNAVAILABLE: &str = "unavailable";
const STATE_HOME: &str = "home";

/// Validate whether a linked auth user exists.
///
/// The root application supplies this callback from its `AuthManager`. A
/// missing user should return `Ok(false)`; provider failures should be
/// returned as errors so an uncertain identity is never accepted.
pub type PersonUserValidator = Arc<dyn Fn(&str) -> Result<bool> + Send + Sync>;

/// Resolve a person's stable unique ID and current entity-registry binding.
///
/// Return `None` when the matching registry entry is disabled. The root
/// application should pass a closure using its shared native registries.
pub type PersonEntityResolver = Arc<dyn Fn(&PersonConfig) -> Result<Option<String>> + Send + Sync>;

/// A YAML or stored person configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonConfig {
    /// Stable person unique ID, independent of the entity's object ID.
    pub id: String,
    /// User-visible name.
    pub name: String,
    /// Runtime entities that supply this person's presence.
    #[serde(default, deserialize_with = "deserialize_trackers")]
    pub device_trackers: Vec<String>,
    /// Optional linked auth user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Optional entity picture reference. Upload validation belongs to the
    /// root application's image-upload service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

impl PersonConfig {
    /// Validate IDs, names, and tracker domains against the pinned schema.
    pub fn validate(&self) -> Result<()> {
        validate_person_id(&self.id)?;
        validate_name(&self.name)?;
        validate_trackers(&self.device_trackers)
    }
}

/// Data accepted when creating an editable person.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonCreate {
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_trackers")]
    pub device_trackers: Vec<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
}

/// Whether a typed update leaves a nullable field unchanged or sets its value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FieldUpdate<T> {
    /// Preserve the current value.
    #[default]
    Keep,
    /// Set the value, including `None` for a nullable field.
    Set(T),
}

/// Partial edit for an editable person.
///
/// The upstream update schema defaults an omitted `device_trackers` field to
/// an empty list. Callers such as `person/update_own_profile` must explicitly
/// pass through the current list when they only intend to update the name or
/// picture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonUpdate {
    pub name: Option<String>,
    pub user_id: FieldUpdate<Option<String>>,
    /// `None` means the upstream default `[]`; `Some` replaces the list.
    pub device_trackers: Option<Vec<String>>,
    pub picture: FieldUpdate<Option<String>>,
}

/// A person associated with an auth user, whether editable or YAML-defined.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonForUser {
    pub person: PersonConfig,
    pub editable: bool,
}

/// Atomic collection snapshot returned to WebSocket or API adapters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonCollectionSnapshot {
    /// Editable, persisted people.
    pub storage: Vec<PersonConfig>,
    /// Read-only YAML people.
    pub config: Vec<PersonConfig>,
}

/// Owning lifecycle for a person integration instance.
pub struct PersonIntegration {
    manager: PersonManager,
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl PersonIntegration {
    /// Load person metadata, publish restored states, and watch Runtime events.
    ///
    /// The integration starts in the pre-start phase, matching Home
    /// Assistant's entity setup order. Call [`Self::mark_started`] after the
    /// application has finished setting up tracker providers.
    pub async fn start(
        runtime: Runtime,
        yaml: Vec<PersonConfig>,
        validator: PersonUserValidator,
        entity_resolver: PersonEntityResolver,
    ) -> Result<Self> {
        start_integration(runtime, yaml, validator, entity_resolver).await
    }

    /// Return a weakly-owned manager handle for storage CRUD and YAML reload.
    pub fn manager(&self) -> PersonManager {
        self.manager.clone()
    }

    /// Recompute people from the current Runtime trackers after startup.
    pub fn mark_started(&self) -> Result<()> {
        self.manager.mark_started()
    }

    /// Stop event handling and invalidate retained manager handles.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shared.active.store(false, Ordering::Release);
        self.shutdown_tx.send_replace(true);
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|error| Error::Service(format!("person worker failed: {error}")))??;
        }
        Ok(())
    }
}

impl Drop for PersonIntegration {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
        self.shutdown_tx.send_replace(true);
    }
}

/// Weak handle for synchronous collection operations.
#[derive(Clone)]
pub struct PersonManager {
    shared: Weak<Shared>,
}

impl PersonManager {
    /// List editable people ordered by stable ID.
    pub fn list(&self) -> Result<Vec<PersonConfig>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.storage.values().cloned().collect())
    }

    /// List YAML-defined people ordered by stable ID.
    pub fn yaml_items(&self) -> Result<Vec<PersonConfig>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.yaml.clone())
    }

    /// Get one editable person.
    pub fn get(&self, id: &str) -> Result<Option<PersonConfig>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.storage.get(id).cloned())
    }

    /// Return the current entity-registry binding for a configured person.
    /// Disabled registry entries have no active Runtime entity and return
    /// `None`; the person remains in its YAML or storage collection.
    pub fn entity_id(&self, id: &str) -> Result<Option<String>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.entity_ids.get(id).cloned())
    }

    /// Find the linked person, preferring editable storage over YAML.
    pub fn person_for_user(&self, user_id: &str) -> Result<Option<PersonForUser>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        if let Some(person) = data
            .storage
            .values()
            .find(|person| person.user_id.as_deref() == Some(user_id))
        {
            return Ok(Some(PersonForUser {
                person: person.clone(),
                editable: true,
            }));
        }
        Ok(data
            .yaml
            .iter()
            .find(|person| person.user_id.as_deref() == Some(user_id))
            .cloned()
            .map(|person| PersonForUser {
                person,
                editable: false,
            }))
    }

    /// Return person entity IDs that track one device-tracker entity.
    pub fn persons_with_entity(&self, tracker_entity_id: &str) -> Result<Vec<String>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data
            .yaml
            .iter()
            .chain(data.storage.values())
            .filter(|person| data.entity_ids.contains_key(&person.id))
            .filter(|person| {
                person
                    .device_trackers
                    .iter()
                    .any(|entity_id| entity_id == tracker_entity_id)
            })
            .filter_map(|person| data.entity_ids.get(&person.id).cloned())
            .collect())
    }

    /// Return the configured tracker entities for a person entity.
    pub fn entities_in_person(&self, person_entity_id: &str) -> Result<Vec<String>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let Some(id) = data
            .entity_ids
            .iter()
            .find_map(|(id, entity_id)| (entity_id == person_entity_id).then_some(id))
        else {
            return Ok(Vec::new());
        };
        Ok(data
            .yaml
            .iter()
            .chain(data.storage.values())
            .find(|person| person.id == *id)
            .map(|person| person.device_trackers.clone())
            .unwrap_or_default())
    }

    /// Add a device tracker to the stored person linked to an auth user.
    ///
    /// Missing links and already-present tracker IDs are no-ops, matching the
    /// source helper. YAML-defined people are read-only and are not modified.
    pub fn add_user_device_tracker(&self, user_id: &str, tracker_entity_id: &str) -> Result<()> {
        validate_trackers(&[tracker_entity_id.to_owned()])?;
        let person = {
            let shared = self.live()?;
            let data = lock(&shared.data);
            Self::ensure_active(&shared)?;
            data.storage
                .values()
                .find(|person| person.user_id.as_deref() == Some(user_id))
                .cloned()
        };
        let Some(mut person) = person else {
            return Ok(());
        };
        if person
            .device_trackers
            .iter()
            .any(|entity_id| entity_id == tracker_entity_id)
        {
            return Ok(());
        }
        person.device_trackers.push(tracker_entity_id.to_owned());
        self.update(
            &person.id,
            PersonUpdate {
                device_trackers: Some(person.device_trackers),
                ..PersonUpdate::default()
            },
        )?;
        Ok(())
    }

    /// Create an editable person with a name-derived ID.
    pub fn create(&self, create: PersonCreate) -> Result<PersonConfig> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        validate_name(&create.name)?;
        validate_trackers(&create.device_trackers)?;
        validate_user_binding(&shared, &data, create.user_id.as_deref(), None)?;

        let id = generate_person_id(&create.name, &data);
        let person = PersonConfig {
            id,
            name: create.name,
            device_trackers: create.device_trackers,
            user_id: create.user_id,
            picture: create.picture,
        };
        person.validate()?;

        let old_data = data.clone();
        let mut next = old_data.clone();
        next.storage.insert(person.id.clone(), person.clone());
        next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
        next.owned_entity_ids = owned_entity_ids(&next.entity_ids);
        self.commit_storage_change(&shared, &mut data, old_data, next, &person, "added")?;
        Ok(person)
    }

    /// Update an editable person. An omitted tracker list becomes `[]`, as in
    /// the pinned `PersonStorageCollection.UPDATE_SCHEMA`.
    pub fn update(&self, id: &str, update: PersonUpdate) -> Result<PersonConfig> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let current = data
            .storage
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("person {id}")))?;

        if let Some(name) = update.name.as_deref() {
            validate_name(name)?;
        }
        let device_trackers = update.device_trackers.unwrap_or_default();
        validate_trackers(&device_trackers)?;
        let next_user_id = match update.user_id {
            FieldUpdate::Keep => current.user_id.clone(),
            FieldUpdate::Set(user_id) => user_id,
        };
        if next_user_id != current.user_id {
            validate_user_binding(&shared, &data, next_user_id.as_deref(), Some(id))?;
        }
        let next_picture = match update.picture {
            FieldUpdate::Keep => current.picture.clone(),
            FieldUpdate::Set(picture) => picture,
        };
        let person = PersonConfig {
            id: current.id.clone(),
            name: update.name.unwrap_or(current.name),
            device_trackers,
            user_id: next_user_id,
            picture: next_picture,
        };
        person.validate()?;

        let old_data = data.clone();
        let mut next = old_data.clone();
        next.storage.insert(person.id.clone(), person.clone());
        next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
        next.owned_entity_ids = owned_entity_ids(&next.entity_ids);
        self.commit_storage_change(&shared, &mut data, old_data, next, &person, "updated")?;
        Ok(person)
    }

    /// Delete an editable person and remove its managed entity state.
    pub fn delete(&self, id: &str) -> Result<PersonConfig> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let old_data = data.clone();
        let removed = old_data
            .storage
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("person {id}")))?;
        let old_entity_id = old_data.entity_ids.get(id).cloned();
        let old_state = old_entity_id
            .as_deref()
            .and_then(|entity_id| shared.runtime.get_state(entity_id));
        let mut next = old_data.clone();
        next.storage.remove(id);
        next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
        next.owned_entity_ids = owned_entity_ids(&next.entity_ids);
        let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
        persist_collection(&shared.runtime, &next)?;
        if let Some(entity_id) = old_entity_id.as_deref()
            && let Err(error) = shared.runtime.remove_state(entity_id, Context::default())
        {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_state(&shared.runtime, entity_id, old_state.as_ref());
            return Err(error);
        }
        *data = next;
        if let Err(error) = emit_collection_change(&shared, "removed", id, Some(&removed)) {
            *data = old_data;
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            if let Some(entity_id) = old_entity_id.as_deref() {
                restore_state(&shared.runtime, entity_id, old_state.as_ref());
            }
            return Err(error);
        }
        Ok(removed)
    }

    /// Replace YAML-defined people while preserving editable storage people.
    /// YAML entries with unknown linked users are skipped, matching the source
    /// helper's per-person filtering behavior.
    pub fn reload_yaml(&self, yaml: Vec<PersonConfig>) -> Result<Vec<PersonConfig>> {
        let shared = self.live()?;
        let accepted = filter_yaml_people(&yaml, &shared.validator)?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let old_data = data.clone();
        let mut next = old_data.clone();
        next.yaml = accepted.clone();
        ensure_unique_person_ids(&next.yaml, &next.storage)?;
        next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
        next.owned_entity_ids = owned_entity_ids(&next.entity_ids);

        let mut restore_ids = old_data.owned_entity_ids.clone();
        restore_ids.extend(next.owned_entity_ids.iter().cloned());
        let old_states = state_snapshots(&shared.runtime, &restore_ids);
        let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
        persist_collection(&shared.runtime, &next)?;
        if let Err(error) = publish_collection(&shared, &old_data, &next) {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states_for_publish(
                &shared.runtime,
                &restore_ids,
                &old_states,
                &next.owned_entity_ids,
            );
            return Err(error);
        }
        *data = next;
        Ok(accepted)
    }

    /// Signal that tracker setup is complete and recompute all person states.
    pub fn mark_started(&self) -> Result<()> {
        let shared = self.live()?;
        if shared.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(error) = refresh_all(&shared) {
            shared.started.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    /// Atomically snapshot the editable and YAML collections and subscribe to
    /// later private storage changes.
    pub fn snapshot_and_subscribe(
        &self,
    ) -> Result<(PersonCollectionSnapshot, broadcast::Receiver<Event>)> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let receiver = shared.runtime.subscribe_signals();
        Ok((
            PersonCollectionSnapshot {
                storage: data.storage.values().cloned().collect(),
                config: data.yaml.clone(),
            },
            receiver,
        ))
    }

    fn commit_storage_change(
        &self,
        shared: &Shared,
        data: &mut PersonData,
        old_data: PersonData,
        next: PersonData,
        person: &PersonConfig,
        change_type: &str,
    ) -> Result<()> {
        let old_entity_id = old_data.entity_ids.get(&person.id).cloned();
        let new_entity_id = next.entity_ids.get(&person.id).cloned();
        let mut affected = BTreeSet::new();
        affected.extend(old_entity_id.iter().cloned());
        affected.extend(new_entity_id.iter().cloned());
        let old_states = state_snapshots(&shared.runtime, &affected);
        let fallback_state = old_entity_id
            .as_deref()
            .and_then(|entity_id| old_states.get(entity_id));
        let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
        persist_collection(&shared.runtime, &next)?;
        if let Err(error) = publish_person_for_phase(
            &shared.runtime,
            person,
            true,
            shared.started.load(Ordering::Acquire),
            new_entity_id.as_deref(),
            fallback_state,
        ) {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states(&shared.runtime, &affected, &old_states);
            return Err(error);
        }
        if let Some(old_entity_id) = old_entity_id.as_deref()
            && Some(old_entity_id) != new_entity_id.as_deref()
            && !next.owned_entity_ids.contains(old_entity_id)
            && let Err(error) = shared
                .runtime
                .remove_state(old_entity_id, Context::default())
        {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states(&shared.runtime, &affected, &old_states);
            return Err(error);
        }
        *data = next;
        if let Err(error) = emit_collection_change(shared, change_type, &person.id, Some(person)) {
            *data = old_data;
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states(&shared.runtime, &affected, &old_states);
            return Err(error);
        }
        Ok(())
    }

    fn live(&self) -> Result<Arc<Shared>> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| Error::Service("person integration is shut down".into()))?;
        if shared.active.load(Ordering::Acquire) {
            Ok(shared)
        } else {
            Err(Error::Service("person integration is shut down".into()))
        }
    }

    fn ensure_active(shared: &Shared) -> Result<()> {
        if shared.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::Service("person integration is shut down".into()))
        }
    }
}

#[derive(Clone, Debug)]
struct PersonData {
    yaml: Vec<PersonConfig>,
    storage: BTreeMap<String, PersonConfig>,
    /// Active registry bindings keyed by stable person unique ID.
    entity_ids: BTreeMap<String, String>,
    /// Runtime states this integration previously published and may remove.
    owned_entity_ids: BTreeSet<String>,
}

struct Shared {
    runtime: Runtime,
    data: Mutex<PersonData>,
    validator: PersonUserValidator,
    entity_resolver: PersonEntityResolver,
    active: AtomicBool,
    started: AtomicBool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedPeople {
    version: u32,
    items: Vec<PersonConfig>,
    #[serde(default)]
    owned_entity_ids: Vec<String>,
}

impl Default for PersistedPeople {
    fn default() -> Self {
        Self {
            version: STORAGE_VERSION,
            items: Vec::new(),
            owned_entity_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PersonStateFields {
    state: String,
    source: Option<String>,
    latitude: Option<Value>,
    longitude: Option<Value>,
    gps_accuracy: Option<Value>,
    in_zones: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn start_integration(
    runtime: Runtime,
    yaml: Vec<PersonConfig>,
    validator: PersonUserValidator,
    entity_resolver: PersonEntityResolver,
) -> Result<PersonIntegration> {
    // The state snapshot and event subscription are atomic, so tracker updates
    // cannot be lost between initial state restoration and worker startup.
    let (initial_states, event_receiver) = runtime.subscribe_with_states();
    let old_metadata = runtime.get_data(STORAGE_KEY)?;
    let persisted = load_persisted_people(old_metadata.clone())?;
    let accepted_yaml = filter_yaml_people(&yaml, &validator)?;
    let mut storage = BTreeMap::new();
    for person in persisted.items {
        person.validate()?;
        if storage.insert(person.id.clone(), person).is_some() {
            return Err(Error::Storage("duplicate stored person ID".into()));
        }
    }
    ensure_unique_person_ids(&accepted_yaml, &storage)?;
    let entity_ids = resolve_entity_ids(&entity_resolver, &accepted_yaml, &storage)?;
    let owned_entity_ids = owned_entity_ids(&entity_ids);
    let previous_owned: BTreeSet<_> = persisted.owned_entity_ids.into_iter().collect();
    let data = PersonData {
        yaml: accepted_yaml,
        storage,
        entity_ids,
        owned_entity_ids,
    };
    let next = data.clone();
    let mut restore_ids = previous_owned.clone();
    restore_ids.extend(next.owned_entity_ids.iter().cloned());
    let old_states = state_snapshots(&runtime, &restore_ids);
    persist_collection(&runtime, &next)?;
    if let Err(error) = publish_collection_with_restore(&runtime, &next, &initial_states) {
        restore_metadata(&runtime, old_metadata.as_ref());
        restore_states_for_publish(&runtime, &restore_ids, &old_states, &next.owned_entity_ids);
        return Err(error);
    }
    let stale = previous_owned
        .difference(&next.owned_entity_ids)
        .cloned()
        .collect::<Vec<_>>();
    for entity_id in stale {
        if let Err(error) = runtime.remove_state(&entity_id, Context::default()) {
            restore_metadata(&runtime, old_metadata.as_ref());
            restore_states_for_publish(&runtime, &restore_ids, &old_states, &next.owned_entity_ids);
            return Err(error);
        }
    }

    let shared = Arc::new(Shared {
        runtime,
        data: Mutex::new(next),
        validator,
        entity_resolver,
        active: AtomicBool::new(true),
        started: AtomicBool::new(false),
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let manager = PersonManager {
        shared: Arc::downgrade(&shared),
    };
    let worker = tokio::spawn(run_event_worker(
        Arc::downgrade(&shared),
        event_receiver,
        shutdown_rx,
    ));
    Ok(PersonIntegration {
        manager,
        shared,
        shutdown_tx,
        worker: Some(worker),
    })
}

fn load_persisted_people(value: Option<Value>) -> Result<PersistedPeople> {
    let Some(value) = value else {
        return Ok(PersistedPeople::default());
    };
    let data: PersistedPeople = serde_json::from_value(value)
        .map_err(|error| Error::Storage(format!("invalid person metadata: {error}")))?;
    if data.version != STORAGE_VERSION {
        return Err(Error::Storage(format!(
            "unsupported person metadata version {}",
            data.version
        )));
    }
    for entity_id in &data.owned_entity_ids {
        if entity_id.strip_prefix("person.").is_none() {
            return Err(Error::Storage(format!(
                "invalid person-owned entity ID {entity_id:?}"
            )));
        }
        validate_runtime_entity_id(entity_id).map_err(|error| Error::Storage(error.to_string()))?;
    }
    Ok(data)
}

fn filter_yaml_people(
    yaml: &[PersonConfig],
    validator: &PersonUserValidator,
) -> Result<Vec<PersonConfig>> {
    let mut accepted = Vec::with_capacity(yaml.len());
    for person in yaml {
        person.validate()?;
        if let Some(user_id) = person.user_id.as_deref()
            && !validator(user_id)?
        {
            tracing::error!(person_id = %person.id, "skipping person with an invalid user_id");
            continue;
        }
        accepted.push(person.clone());
    }
    Ok(accepted)
}

fn validate_user_binding(
    shared: &Shared,
    data: &PersonData,
    user_id: Option<&str>,
    updating_id: Option<&str>,
) -> Result<()> {
    let Some(user_id) = user_id else {
        return Ok(());
    };
    if !(shared.validator)(user_id)? {
        return Err(Error::InvalidInput("User does not exist".into()));
    }
    if data.storage.values().any(|person| {
        Some(person.id.as_str()) != updating_id && person.user_id.as_deref() == Some(user_id)
    }) || data
        .yaml
        .iter()
        .any(|person| person.user_id.as_deref() == Some(user_id))
    {
        return Err(Error::InvalidInput("User already taken".into()));
    }
    Ok(())
}

fn ensure_unique_person_ids(
    yaml: &[PersonConfig],
    storage: &BTreeMap<String, PersonConfig>,
) -> Result<()> {
    let mut ids = BTreeSet::new();
    for person in yaml {
        person.validate()?;
        if !ids.insert(person.id.as_str()) {
            return Err(Error::InvalidInput(format!(
                "duplicate person ID {:?}",
                person.id
            )));
        }
    }
    for (id, person) in storage {
        person.validate()?;
        if id != &person.id || !ids.insert(id.as_str()) {
            return Err(Error::InvalidInput(format!("duplicate person ID {:?}", id)));
        }
    }
    Ok(())
}

fn resolve_entity_ids(
    resolver: &PersonEntityResolver,
    yaml: &[PersonConfig],
    storage: &BTreeMap<String, PersonConfig>,
) -> Result<BTreeMap<String, String>> {
    let mut bindings = BTreeMap::new();
    let mut unique_entity_ids = BTreeSet::new();
    for person in yaml.iter().chain(storage.values()) {
        if let Some(entity_id) = resolver(person)? {
            validate_runtime_entity_id(&entity_id)?;
            if !unique_entity_ids.insert(entity_id.clone()) {
                return Err(Error::InvalidInput(format!(
                    "duplicate person entity ID {entity_id:?}"
                )));
            }
            bindings.insert(person.id.clone(), entity_id);
        }
    }
    Ok(bindings)
}

fn owned_entity_ids(bindings: &BTreeMap<String, String>) -> BTreeSet<String> {
    bindings.values().cloned().collect()
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidInput("person name must not be empty".into()));
    }
    Ok(())
}

fn validate_person_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.chars().count() > 255
        || id.chars().any(char::is_control)
        || id.trim().is_empty()
    {
        return Err(Error::InvalidInput(format!(
            "invalid person unique ID {id:?}; expected 1 to 255 non-control characters"
        )));
    }
    Ok(())
}

fn validate_runtime_entity_id(entity_id: &str) -> Result<()> {
    let Some(object_id) = entity_id.strip_prefix("person.") else {
        return Err(Error::InvalidInput(format!(
            "person entity ID {entity_id:?} must belong to person"
        )));
    };
    if object_id.is_empty()
        || object_id.len() > 255
        || !object_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid person entity ID {entity_id:?}"
        )));
    }
    Ok(())
}

fn validate_trackers(trackers: &[String]) -> Result<()> {
    for tracker in trackers {
        let Some(object_id) = tracker.strip_prefix(TRACKER_ENTITY_PREFIX) else {
            return Err(Error::InvalidInput(format!(
                "person tracker {tracker:?} must belong to device_tracker"
            )));
        };
        if object_id.is_empty()
            || !object_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(Error::InvalidInput(format!(
                "invalid device_tracker entity ID {tracker:?}"
            )));
        }
    }
    Ok(())
}

fn deserialize_trackers<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(tracker) => {
            if tracker.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![tracker])
            }
        }
        Value::Array(values) => values
            .into_iter()
            .filter(|value| !is_falsy(value))
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    <D::Error as serde::de::Error>::custom(
                        "device_trackers entries must be entity ID strings",
                    )
                })
            })
            .collect(),
        value if is_falsy(&value) => Ok(Vec::new()),
        _ => Err(<D::Error as serde::de::Error>::custom(
            "device_trackers must be an entity ID string or a list of entity ID strings",
        )),
    }
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number == 0.0),
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
    }
}

fn generate_person_id(name: &str, data: &PersonData) -> String {
    let base = slugify(name);
    let mut candidate = base.clone();
    let mut attempt = 1;
    while data.storage.contains_key(&candidate)
        || data.yaml.iter().any(|person| person.id == candidate)
    {
        attempt += 1;
        candidate = format!("{base}_{attempt}");
    }
    candidate
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else if !slug.is_empty() && !separator {
            slug.push('_');
            separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "unknown".to_owned()
    } else {
        slug
    }
}

fn persist_collection(runtime: &Runtime, data: &PersonData) -> Result<()> {
    let persisted = PersistedPeople {
        version: STORAGE_VERSION,
        items: data.storage.values().cloned().collect(),
        owned_entity_ids: data.owned_entity_ids.iter().cloned().collect(),
    };
    let value = serde_json::to_value(persisted)
        .map_err(|error| Error::Storage(format!("serialize person metadata: {error}")))?;
    runtime.set_data(STORAGE_KEY, &value)
}

fn restore_metadata(runtime: &Runtime, old: Option<&Value>) {
    match old {
        Some(value) => {
            let _ = runtime.set_data(STORAGE_KEY, value);
        }
        None => {
            let _ = runtime.delete_data(STORAGE_KEY);
        }
    }
}

fn state_snapshots(runtime: &Runtime, entity_ids: &BTreeSet<String>) -> BTreeMap<String, State> {
    entity_ids
        .iter()
        .filter_map(|entity_id| {
            runtime
                .get_state(entity_id)
                .map(|state| (entity_id.clone(), state))
        })
        .collect()
}

fn restore_state(runtime: &Runtime, entity_id: &str, old: Option<&State>) {
    if let Some(state) = old {
        let _ = runtime.set_state(
            entity_id,
            &state.state,
            state.attributes.clone(),
            state.context.clone(),
            false,
        );
    } else {
        let _ = runtime.remove_state(entity_id, Context::default());
    }
}

fn restore_states(
    runtime: &Runtime,
    entity_ids: &BTreeSet<String>,
    old_states: &BTreeMap<String, State>,
) {
    for entity_id in entity_ids {
        restore_state(runtime, entity_id, old_states.get(entity_id));
    }
}

fn publish_collection_with_restore(
    runtime: &Runtime,
    data: &PersonData,
    initial_states: &[State],
) -> Result<()> {
    let states: BTreeMap<_, _> = initial_states
        .iter()
        .cloned()
        .map(|state| (state.entity_id.clone(), state))
        .collect();
    for person in data.yaml.iter().chain(data.storage.values()) {
        let editable = data.storage.contains_key(&person.id);
        let Some(entity_id) = data.entity_ids.get(&person.id) else {
            continue;
        };
        let previous = states.get(entity_id);
        let fields = restored_fields(previous, runtime);
        publish_person(runtime, entity_id, person, editable, &fields)?;
    }
    Ok(())
}

fn publish_person_for_phase(
    runtime: &Runtime,
    person: &PersonConfig,
    editable: bool,
    started: bool,
    entity_id: Option<&str>,
    fallback_state: Option<&State>,
) -> Result<()> {
    let Some(entity_id) = entity_id else {
        return Ok(());
    };
    let fields = if started {
        selected_tracker_fields(person, runtime)
    } else {
        let current_state = runtime.get_state(entity_id);
        restored_fields(current_state.as_ref().or(fallback_state), runtime)
    };
    publish_person(runtime, entity_id, person, editable, &fields)
}

fn publish_person(
    runtime: &Runtime,
    entity_id: &str,
    person: &PersonConfig,
    editable: bool,
    fields: &PersonStateFields,
) -> Result<()> {
    runtime.set_state(
        entity_id,
        &fields.state,
        state_attributes(person, editable, fields),
        Context::default(),
        false,
    )?;
    Ok(())
}

fn refresh_all(shared: &Shared) -> Result<()> {
    let data = lock(&shared.data);
    if !shared.active.load(Ordering::Acquire) {
        return Ok(());
    }
    for person in data.yaml.iter().chain(data.storage.values()) {
        let editable = data.storage.contains_key(&person.id);
        let Some(entity_id) = data.entity_ids.get(&person.id) else {
            continue;
        };
        let fields = selected_tracker_fields(person, &shared.runtime);
        publish_person(&shared.runtime, entity_id, person, editable, &fields)?;
    }
    Ok(())
}

fn refresh_for_tracker(shared: &Shared, tracker_id: &str) -> Result<()> {
    if !shared.started.load(Ordering::Acquire) || !shared.active.load(Ordering::Acquire) {
        return Ok(());
    }
    let data = lock(&shared.data);
    for person in data.yaml.iter().chain(data.storage.values()) {
        if !person.device_trackers.iter().any(|id| id == tracker_id) {
            continue;
        }
        let editable = data.storage.contains_key(&person.id);
        let Some(entity_id) = data.entity_ids.get(&person.id) else {
            continue;
        };
        let fields = selected_tracker_fields(person, &shared.runtime);
        publish_person(&shared.runtime, entity_id, person, editable, &fields)?;
    }
    Ok(())
}

fn selected_tracker_fields(person: &PersonConfig, runtime: &Runtime) -> PersonStateFields {
    let mut latest_connected = None;
    let mut latest_legacy_home = None;
    let mut latest_gps = None;
    let mut latest_not_home = None;

    for entity_id in &person.device_trackers {
        let Some(state) = runtime.get_state(entity_id) else {
            continue;
        };
        if state.state == STATE_UNKNOWN || state.state == STATE_UNAVAILABLE {
            continue;
        }
        if state
            .attributes
            .get("tracking_type")
            .is_some_and(|value| value.as_str() == Some("connection"))
            && state.attributes.get("in_zones").is_some_and(is_truthy)
        {
            update_latest(&mut latest_connected, state);
        } else if state
            .attributes
            .get("source_type")
            .is_some_and(|value| value.as_str() == Some("gps"))
        {
            update_latest(&mut latest_gps, state);
        } else if state.state == STATE_HOME {
            update_latest(&mut latest_legacy_home, state);
        } else {
            update_latest(&mut latest_not_home, state);
        }
    }

    let Some(state) = latest_connected
        .or(latest_legacy_home)
        .or(latest_gps)
        .or(latest_not_home)
    else {
        return PersonStateFields {
            state: STATE_UNKNOWN.to_owned(),
            ..PersonStateFields::default()
        };
    };
    fields_from_source_state(&state, runtime)
}

fn update_latest(latest: &mut Option<State>, current: State) {
    if latest
        .as_ref()
        .is_none_or(|previous| current.last_updated > previous.last_updated)
    {
        *latest = Some(current);
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Return whether a person state changed significantly for recorder purposes.
///
/// The pinned callback compares only state strings; attribute changes alone
/// are not significant.
pub fn is_significant_change(old_state: &str, new_state: &str) -> bool {
    new_state != old_state
}

fn restored_fields(previous: Option<&State>, runtime: &Runtime) -> PersonStateFields {
    previous.map_or_else(
        || PersonStateFields {
            state: STATE_UNKNOWN.to_owned(),
            ..PersonStateFields::default()
        },
        |state| {
            // The pinned entity parser calls `_parse_source_state` with the
            // restored person State itself, so its entity ID becomes `source`
            // until startup recomputes from a real tracker.
            fields_from_state(state, state.entity_id.clone(), runtime)
        },
    )
}

fn fields_from_source_state(state: &State, runtime: &Runtime) -> PersonStateFields {
    fields_from_state(state, state.entity_id.clone(), runtime)
}

fn fields_from_state(state: &State, source: String, runtime: &Runtime) -> PersonStateFields {
    let latitude = non_null_attr(&state.attributes, "latitude");
    let longitude = non_null_attr(&state.attributes, "longitude");
    let gps_accuracy = non_null_attr(&state.attributes, "gps_accuracy");
    let in_zones_attr = state
        .attributes
        .get("in_zones")
        .filter(|value| !value.is_null());
    let mut in_zones = string_list(in_zones_attr);
    let mut latitude = latitude;
    let mut longitude = longitude;

    // Legacy scanners report `home` without zone membership or coordinates.
    // Preserve modern empty lists and only borrow home coordinates when both
    // source coordinates are absent.
    if in_zones_attr.is_none()
        && state.state == STATE_HOME
        && let Some(home_zone) = runtime.get_state(HOME_ENTITY_ID)
    {
        in_zones = vec![HOME_ENTITY_ID.to_owned()];
        if latitude.is_none() && longitude.is_none() {
            latitude = non_null_attr(&home_zone.attributes, "latitude");
            longitude = non_null_attr(&home_zone.attributes, "longitude");
        }
    }

    PersonStateFields {
        state: state.state.clone(),
        source: Some(source),
        latitude,
        longitude,
        gps_accuracy,
        in_zones,
    }
}

fn non_null_attr(attributes: &Map<String, Value>, key: &str) -> Option<Value> {
    attributes
        .get(key)
        .filter(|value| !value.is_null())
        .cloned()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn state_attributes(
    person: &PersonConfig,
    editable: bool,
    fields: &PersonStateFields,
) -> Map<String, Value> {
    let mut attributes = Map::from_iter([
        ("editable".to_owned(), Value::Bool(editable)),
        ("id".to_owned(), Value::String(person.id.clone())),
        ("device_trackers".to_owned(), json!(person.device_trackers)),
        ("in_zones".to_owned(), json!(fields.in_zones)),
        (
            "friendly_name".to_owned(),
            Value::String(person.name.clone()),
        ),
    ]);
    if let Some(picture) = &person.picture {
        attributes.insert("entity_picture".to_owned(), Value::String(picture.clone()));
    }
    if let Some(user_id) = &person.user_id {
        attributes.insert("user_id".to_owned(), Value::String(user_id.clone()));
    }
    if let Some(source) = &fields.source {
        attributes.insert("source".to_owned(), Value::String(source.clone()));
    }
    if let Some(latitude) = &fields.latitude {
        attributes.insert("latitude".to_owned(), latitude.clone());
    }
    if let Some(longitude) = &fields.longitude {
        attributes.insert("longitude".to_owned(), longitude.clone());
    }
    if let Some(gps_accuracy) = &fields.gps_accuracy {
        attributes.insert("gps_accuracy".to_owned(), gps_accuracy.clone());
    }
    attributes
}

fn publish_collection(shared: &Shared, old_data: &PersonData, data: &PersonData) -> Result<()> {
    let started = shared.started.load(Ordering::Acquire);
    for person in data.yaml.iter().chain(data.storage.values()) {
        let editable = data.storage.contains_key(&person.id);
        let old_entity_id = old_data.entity_ids.get(&person.id);
        let fallback_state = old_entity_id.and_then(|id| shared.runtime.get_state(id));
        publish_person_for_phase(
            &shared.runtime,
            person,
            editable,
            started,
            data.entity_ids.get(&person.id).map(String::as_str),
            fallback_state.as_ref(),
        )?;
    }
    for old_id in old_data.owned_entity_ids.difference(&data.owned_entity_ids) {
        shared.runtime.remove_state(old_id, Context::default())?;
    }
    Ok(())
}

fn emit_collection_change(
    shared: &Shared,
    change_type: &str,
    id: &str,
    item: Option<&PersonConfig>,
) -> Result<()> {
    let item = item
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| Error::Storage(format!("serialize person collection item: {error}")))?;
    let change = json!({
        "change_type": change_type,
        "person_id": id,
        "item": item,
    });
    let data = Map::from_iter([("changes".to_owned(), Value::Array(vec![change]))]);
    shared
        .runtime
        .dispatch_signal(PERSON_COLLECTION_UPDATED_SIGNAL, data, Context::default())?;
    Ok(())
}

async fn run_event_worker(
    shared: Weak<Shared>,
    mut events: broadcast::Receiver<Event>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            received = events.recv() => {
                match received {
                    Ok(event) => {
                        let Some(shared) = shared.upgrade() else { break };
                        if !shared.active.load(Ordering::Acquire) {
                            break;
                        }
                        if let Err(error) = handle_runtime_event(&shared, &event) {
                            tracing::error!(%error, "person event handling failed");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(shared) = shared.upgrade()
                            && shared.started.load(Ordering::Acquire)
                            && let Err(error) = refresh_all(&shared)
                        {
                            tracing::error!(%error, "person state refresh after event lag failed");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

fn handle_runtime_event(shared: &Shared, event: &Event) -> Result<()> {
    match event.event_type.as_str() {
        "state_changed" => {
            if let Some(entity_id) = event.data.get("entity_id").and_then(Value::as_str) {
                refresh_for_tracker(shared, entity_id)?;
            }
        }
        "user_removed" => {
            if let Some(user_id) = event.data.get("user_id").and_then(Value::as_str) {
                unlink_user(shared, user_id)?;
            }
        }
        "entity_registry_updated" => reconcile_registry_bindings(shared)?,
        _ => {}
    }
    Ok(())
}

fn reconcile_registry_bindings(shared: &Shared) -> Result<()> {
    let mut data = lock(&shared.data);
    if !shared.active.load(Ordering::Acquire) {
        return Ok(());
    }
    let old_data = data.clone();
    let mut next = old_data.clone();
    next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
    next.owned_entity_ids = owned_entity_ids(&next.entity_ids);
    if next.entity_ids == old_data.entity_ids {
        return Ok(());
    }

    let mut affected = old_data.owned_entity_ids.clone();
    affected.extend(next.owned_entity_ids.iter().cloned());
    let old_states = state_snapshots(&shared.runtime, &affected);
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    persist_collection(&shared.runtime, &next)?;
    let started = shared.started.load(Ordering::Acquire);
    for person in next.yaml.iter().chain(next.storage.values()) {
        let editable = next.storage.contains_key(&person.id);
        let fallback_state = old_data
            .entity_ids
            .get(&person.id)
            .and_then(|entity_id| old_states.get(entity_id));
        if let Err(error) = publish_person_for_phase(
            &shared.runtime,
            person,
            editable,
            started,
            next.entity_ids.get(&person.id).map(String::as_str),
            fallback_state,
        ) {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states(&shared.runtime, &affected, &old_states);
            return Err(error);
        }
    }
    for entity_id in old_data.owned_entity_ids.difference(&next.owned_entity_ids) {
        if let Err(error) = shared.runtime.remove_state(entity_id, Context::default()) {
            restore_metadata(&shared.runtime, old_metadata.as_ref());
            restore_states(&shared.runtime, &affected, &old_states);
            return Err(error);
        }
    }
    *data = next;
    Ok(())
}

fn unlink_user(shared: &Shared, user_id: &str) -> Result<()> {
    let ids = {
        let data = lock(&shared.data);
        data.storage
            .values()
            .filter(|person| person.user_id.as_deref() == Some(user_id))
            .map(|person| person.id.clone())
            .collect::<Vec<_>>()
    };
    for id in ids {
        let current = {
            let data = lock(&shared.data);
            data.storage.get(&id).cloned()
        };
        let Some(current) = current else { continue };
        let mut update = PersonUpdate {
            user_id: FieldUpdate::Set(None),
            ..PersonUpdate::default()
        };
        // Upstream's UPDATE_SCHEMA defaults omitted device_trackers to [],
        // and user-removed handling does not pass the existing list through.
        update.device_trackers = None;
        apply_update_from_worker(shared, &id, current, update)?;
    }
    Ok(())
}

fn apply_update_from_worker(
    shared: &Shared,
    id: &str,
    current: PersonConfig,
    update: PersonUpdate,
) -> Result<()> {
    let mut data = lock(&shared.data);
    let current = data.storage.get(id).cloned().unwrap_or(current);
    let next_user_id = match update.user_id {
        FieldUpdate::Keep => current.user_id.clone(),
        FieldUpdate::Set(value) => value,
    };
    let person = PersonConfig {
        id: current.id.clone(),
        name: update.name.unwrap_or(current.name),
        user_id: next_user_id,
        device_trackers: update.device_trackers.unwrap_or_default(),
        picture: match update.picture {
            FieldUpdate::Keep => current.picture,
            FieldUpdate::Set(value) => value,
        },
    };
    let old_data = data.clone();
    let mut next = old_data.clone();
    next.storage.insert(id.to_owned(), person.clone());
    next.entity_ids = resolve_entity_ids(&shared.entity_resolver, &next.yaml, &next.storage)?;
    next.owned_entity_ids = owned_entity_ids(&next.entity_ids);
    commit_shared_update(shared, &mut data, old_data, next, &person)
}

fn commit_shared_update(
    shared: &Shared,
    data: &mut PersonData,
    old_data: PersonData,
    next: PersonData,
    person: &PersonConfig,
) -> Result<()> {
    let old_entity_id = old_data.entity_ids.get(&person.id).cloned();
    let new_entity_id = next.entity_ids.get(&person.id).cloned();
    let mut affected = BTreeSet::new();
    affected.extend(old_entity_id.iter().cloned());
    affected.extend(new_entity_id.iter().cloned());
    let old_states = state_snapshots(&shared.runtime, &affected);
    let fallback_state = old_entity_id
        .as_deref()
        .and_then(|entity_id| old_states.get(entity_id));
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    persist_collection(&shared.runtime, &next)?;
    if let Err(error) = publish_person_for_phase(
        &shared.runtime,
        person,
        true,
        shared.started.load(Ordering::Acquire),
        new_entity_id.as_deref(),
        fallback_state,
    ) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        restore_states(&shared.runtime, &affected, &old_states);
        return Err(error);
    }
    if let Some(old_entity_id) = old_entity_id.as_deref()
        && Some(old_entity_id) != new_entity_id.as_deref()
        && !next.owned_entity_ids.contains(old_entity_id)
        && let Err(error) = shared
            .runtime
            .remove_state(old_entity_id, Context::default())
    {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        restore_states(&shared.runtime, &affected, &old_states);
        return Err(error);
    }
    *data = next;
    if let Err(error) = emit_collection_change(shared, "updated", &person.id, Some(person)) {
        *data = old_data;
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        restore_states(&shared.runtime, &affected, &old_states);
        return Err(error);
    }
    Ok(())
}

fn restore_states_for_publish(
    runtime: &Runtime,
    old_owned: &BTreeSet<String>,
    old_states: &BTreeMap<String, State>,
    new_owned: &BTreeSet<String>,
) {
    for entity_id in new_owned.difference(old_owned) {
        restore_state(runtime, entity_id, None);
    }
    restore_states(runtime, old_owned, old_states);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use hass_core::{MemoryStore, Store};
    use hass_registry::{EntityRegistration, EntityUpdate, Registries};
    use serde_json::json;
    use std::time::Duration;

    fn runtime() -> (Runtime, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).expect("runtime");
        (runtime, store)
    }

    fn seeded_runtime(states: Vec<State>) -> (Runtime, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        for state in states {
            store.put_state(&state, None).expect("seed state");
        }
        let runtime = Runtime::new(store.clone()).expect("runtime");
        (runtime, store)
    }

    fn validator() -> PersonUserValidator {
        Arc::new(|user_id| Ok(user_id != "missing"))
    }

    fn person(
        id: &str,
        name: &str,
        device_trackers: &[&str],
        user_id: Option<&str>,
    ) -> PersonConfig {
        PersonConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            device_trackers: device_trackers
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            user_id: user_id.map(str::to_owned),
            picture: None,
        }
    }

    fn attrs(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn state(entity_id: &str, state: &str, attributes: Value, seconds: i64) -> State {
        let time = DateTime::<Utc>::from_timestamp(seconds, 0).expect("valid timestamp");
        State {
            entity_id: entity_id.to_owned(),
            state: state.to_owned(),
            attributes: attrs(attributes),
            last_changed: time,
            last_updated: time,
            last_reported: time,
            context: Context::default(),
        }
    }

    async fn start(runtime: Runtime, yaml: Vec<PersonConfig>) -> PersonIntegration {
        PersonIntegration::start(runtime, yaml, validator(), name_entity_resolver())
            .await
            .expect("person integration")
    }

    fn name_entity_resolver() -> PersonEntityResolver {
        Arc::new(|person| Ok(Some(format!("person.{}", slugify(&person.name)))))
    }

    fn registry_entity_resolver(registry: Registries) -> PersonEntityResolver {
        Arc::new(move |person| {
            let mut registration = EntityRegistration::new("person", "person", person.id.clone());
            registration.name = Some(person.name.clone());
            registration.original_name = Some(person.name.clone());
            registration.suggested_object_id = Some(person.name.clone());
            let entry = registry.get_or_create_entity(registration)?;
            Ok(entry.disabled_by.is_none().then_some(entry.entity_id))
        })
    }

    async fn wait_until(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if ready() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition should become true");
    }

    #[tokio::test]
    async fn connected_in_zone_beats_legacy_home_gps_and_other_state() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "device_tracker.connection",
                "not_home",
                attrs(json!({
                    "tracking_type": "connection",
                    "in_zones": ["zone.office"],
                    "latitude": 42.1,
                    "longitude": -83.1
                })),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.legacy",
                "home",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.gps",
                "away",
                attrs(json!({"source_type": "gps", "latitude": 1.0})),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.other",
                "office",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = start(
            runtime.clone(),
            vec![person(
                "alex",
                "Alex",
                &[
                    "device_tracker.connection",
                    "device_tracker.legacy",
                    "device_tracker.gps",
                    "device_tracker.other",
                ],
                None,
            )],
        )
        .await;

        integration.mark_started().unwrap();

        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.state, "not_home");
        assert_eq!(state.attributes["source"], "device_tracker.connection");
        assert_eq!(state.attributes["in_zones"], json!(["zone.office"]));
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_home_beats_gps_even_when_gps_is_newer() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "device_tracker.legacy",
                "home",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.gps",
                "away",
                attrs(json!({"source_type": "gps", "latitude": 43.0})),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = start(
            runtime.clone(),
            vec![person(
                "alex",
                "Alex",
                &["device_tracker.gps", "device_tracker.legacy"],
                None,
            )],
        )
        .await;

        integration.mark_started().unwrap();

        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.state, "home");
        assert_eq!(state.attributes["source"], "device_tracker.legacy");
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn latest_last_updated_wins_within_a_priority_group() {
        let first = state(
            "device_tracker.first",
            "away",
            json!({"source_type": "gps", "latitude": 1.0}),
            10,
        );
        let second = state(
            "device_tracker.second",
            "away",
            json!({"source_type": "gps", "latitude": 2.0}),
            20,
        );
        let (runtime, _) = seeded_runtime(vec![first, second]);
        let result = selected_tracker_fields(
            &person(
                "alex",
                "Alex",
                &["device_tracker.first", "device_tracker.second"],
                None,
            ),
            &runtime,
        );

        assert_eq!(result.source.as_deref(), Some("device_tracker.second"));
        assert_eq!(result.latitude, Some(json!(2.0)));
    }

    #[test]
    fn gps_beats_newer_non_gps_tracker_state() {
        let gps = state(
            "device_tracker.gps",
            "away",
            json!({"source_type": "gps", "latitude": 1.0}),
            10,
        );
        let other = state("device_tracker.other", "office", json!({}), 20);
        let (runtime, _) = seeded_runtime(vec![gps, other]);
        let result = selected_tracker_fields(
            &person(
                "alex",
                "Alex",
                &["device_tracker.other", "device_tracker.gps"],
                None,
            ),
            &runtime,
        );

        assert_eq!(result.source.as_deref(), Some("device_tracker.gps"));
    }

    #[test]
    fn connection_without_in_zones_is_not_in_the_top_priority_group() {
        let connection = state(
            "device_tracker.connection",
            "away",
            json!({"tracking_type": "connection", "in_zones": []}),
            20,
        );
        let gps = state(
            "device_tracker.gps",
            "home",
            json!({"source_type": "gps", "in_zones": ["zone.home"]}),
            10,
        );
        let (runtime, _) = seeded_runtime(vec![connection, gps]);
        let result = selected_tracker_fields(
            &person(
                "alex",
                "Alex",
                &["device_tracker.connection", "device_tracker.gps"],
                None,
            ),
            &runtime,
        );

        assert_eq!(result.source.as_deref(), Some("device_tracker.gps"));
    }

    #[test]
    fn equal_last_updated_timestamps_keep_the_first_configured_tracker() {
        let first = state(
            "device_tracker.first",
            "away",
            json!({"source_type": "gps", "latitude": 1.0}),
            10,
        );
        let second = state(
            "device_tracker.second",
            "away",
            json!({"source_type": "gps", "latitude": 2.0}),
            10,
        );
        let (runtime, _) = seeded_runtime(vec![first, second]);
        let result = selected_tracker_fields(
            &person(
                "alex",
                "Alex",
                &["device_tracker.first", "device_tracker.second"],
                None,
            ),
            &runtime,
        );

        assert_eq!(result.source.as_deref(), Some("device_tracker.first"));
        assert_eq!(result.latitude, Some(json!(1.0)));
    }

    #[tokio::test]
    async fn unknown_and_unavailable_trackers_are_ignored() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "device_tracker.unknown",
                "unknown",
                attrs(json!({"source_type": "gps", "latitude": 1.0})),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.unavailable",
                "unavailable",
                attrs(json!({"tracking_type": "connection", "in_zones": ["zone.home"]})),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = start(
            runtime.clone(),
            vec![person(
                "alex",
                "Alex",
                &["device_tracker.unknown", "device_tracker.unavailable"],
                None,
            )],
        )
        .await;

        integration.mark_started().unwrap();

        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.state, STATE_UNKNOWN);
        assert_eq!(state.attributes["in_zones"], json!([]));
        assert!(!state.attributes.contains_key("source"));
        assert!(!state.attributes.contains_key("latitude"));
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gps_attributes_are_copied_without_zone_geometry_recalculation() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "zone.office",
                "0",
                attrs(json!({"latitude": 40.0, "longitude": -75.0, "radius": 200.0})),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.phone",
                "home",
                attrs(json!({
                    "source_type": "gps",
                    "latitude": 40.0,
                    "longitude": -75.0,
                    "gps_accuracy": 8,
                    "in_zones": []
                })),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = start(
            runtime.clone(),
            vec![person("alex", "Alex", &["device_tracker.phone"], None)],
        )
        .await;

        integration.mark_started().unwrap();

        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.state, "home");
        assert_eq!(state.attributes["latitude"], 40.0);
        assert_eq!(state.attributes["longitude"], -75.0);
        assert_eq!(state.attributes["gps_accuracy"], 8);
        assert_eq!(state.attributes["in_zones"], json!([]));
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_home_uses_zone_home_membership_and_coordinates() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "zone.home",
                "0",
                attrs(json!({"latitude": 42.5, "longitude": -83.2})),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.router",
                "home",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = start(
            runtime.clone(),
            vec![person("alex", "Alex", &["device_tracker.router"], None)],
        )
        .await;

        integration.mark_started().unwrap();

        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.attributes["in_zones"], json!(["zone.home"]));
        assert_eq!(state.attributes["latitude"], 42.5);
        assert_eq!(state.attributes["longitude"], -83.2);
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn legacy_home_only_borrows_coordinates_when_both_are_missing() {
        let (runtime, _) = seeded_runtime(vec![
            state(
                "zone.home",
                "0",
                json!({"latitude": 42.5, "longitude": -83.2}),
                1,
            ),
            state("device_tracker.legacy", "home", json!({"latitude": 1.0}), 2),
        ]);
        let result = selected_tracker_fields(
            &person("alex", "Alex", &["device_tracker.legacy"], None),
            &runtime,
        );

        assert_eq!(result.in_zones, vec!["zone.home"]);
        assert_eq!(result.latitude, Some(json!(1.0)));
        assert_eq!(result.longitude, None);
    }

    #[test]
    fn explicit_empty_in_zones_does_not_get_legacy_home_fallback() {
        let (runtime, _) = seeded_runtime(vec![
            state(
                "zone.home",
                "0",
                json!({"latitude": 42.5, "longitude": -83.2}),
                1,
            ),
            state("device_tracker.modern", "home", json!({"in_zones": []}), 2),
        ]);
        let result = selected_tracker_fields(
            &person("alex", "Alex", &["device_tracker.modern"], None),
            &runtime,
        );

        assert!(result.in_zones.is_empty());
        assert_eq!(result.latitude, None);
        assert_eq!(result.longitude, None);
    }

    #[tokio::test]
    async fn restored_state_waits_for_start_then_uses_current_tracker_state() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "zone.home",
                "0",
                attrs(json!({"latitude": 42.5, "longitude": -83.2})),
                Context::default(),
                false,
            )
            .unwrap();
        let registry = Registries::new(runtime.clone()).unwrap();
        let entity_resolver = registry_entity_resolver(registry);
        entity_resolver(&person("alex", "Alex", &[], None)).unwrap();
        runtime
            .set_state(
                "device_tracker.current",
                "away",
                attrs(json!({
                    "source_type": "gps",
                    "latitude": 40.0,
                    "longitude": -75.0,
                    "gps_accuracy": 5,
                    "in_zones": ["zone.office"]
                })),
                Context {
                    user_id: Some("auth-user".to_owned()),
                    ..Context::default()
                },
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "person.alex",
                "home",
                attrs(json!({"gps_accuracy": 12, "source": "device_tracker.old"})),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = PersonIntegration::start(
            runtime.clone(),
            vec![PersonConfig {
                id: "alex".into(),
                name: "Alex Updated".into(),
                device_trackers: vec!["device_tracker.current".into()],
                user_id: Some("auth-user".into()),
                picture: None,
            }],
            validator(),
            entity_resolver,
        )
        .await
        .unwrap();

        let restored = runtime.get_state("person.alex").unwrap();
        assert_eq!(restored.state, "home");
        assert_eq!(restored.attributes["source"], "person.alex");
        assert_eq!(restored.attributes["gps_accuracy"], 12);
        assert_eq!(restored.attributes["in_zones"], json!(["zone.home"]));
        assert_eq!(restored.attributes["latitude"], 42.5);
        assert_eq!(restored.attributes["longitude"], -83.2);
        assert_eq!(restored.attributes["friendly_name"], "Alex Updated");
        assert_eq!(restored.attributes["user_id"], "auth-user");

        integration.mark_started().unwrap();

        let live = runtime.get_state("person.alex").unwrap();
        assert_eq!(live.state, "away");
        assert_eq!(live.attributes["source"], "device_tracker.current");
        assert_eq!(live.attributes["in_zones"], json!(["zone.office"]));
        assert_eq!(live.attributes["latitude"], 40.0);
        assert_eq!(live.attributes["gps_accuracy"], 5);
        assert_eq!(live.context.user_id, None);
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_yaml_user_is_skipped_without_dropping_other_people() {
        let (runtime, _) = runtime();
        let integration = start(
            runtime.clone(),
            vec![
                person("valid", "Valid", &[], Some("known")),
                person("invalid", "Invalid", &[], Some("missing")),
            ],
        )
        .await;

        let snapshot = integration.manager().snapshot_and_subscribe().unwrap().0;
        assert_eq!(
            snapshot.config,
            vec![person("valid", "Valid", &[], Some("known"))]
        );
        assert!(runtime.get_state("person.valid").is_some());
        assert!(runtime.get_state("person.invalid").is_none());
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shared_registry_resolves_name_ids_and_rejects_duplicate_unique_ids() {
        let (runtime, _) = runtime();
        let registry = Registries::new(runtime.clone()).unwrap();
        let integration = PersonIntegration::start(
            runtime.clone(),
            vec![
                person("unique-a", "Same Name", &[], None),
                person("unique-b", "Same Name", &[], None),
            ],
            validator(),
            registry_entity_resolver(registry.clone()),
        )
        .await
        .unwrap();
        let manager = integration.manager();
        let first = manager.entity_id("unique-a").unwrap().unwrap();
        let second = manager.entity_id("unique-b").unwrap().unwrap();
        assert_eq!(first, "person.same_name");
        assert_eq!(second, "person.same_name_2");
        assert_eq!(
            runtime.get_state(&first).unwrap().attributes["id"],
            "unique-a"
        );
        assert_eq!(
            runtime.get_state(&second).unwrap().attributes["id"],
            "unique-b"
        );

        integration.shutdown().await.unwrap();
        let duplicate = PersonIntegration::start(
            runtime,
            vec![
                person("duplicate", "First", &[], None),
                person("duplicate", "Second", &[], None),
            ],
            validator(),
            registry_entity_resolver(registry),
        )
        .await;
        assert_eq!(
            duplicate.err().expect("duplicate person IDs are rejected"),
            Error::InvalidInput("duplicate person ID \"duplicate\"".into())
        );
    }

    #[tokio::test]
    async fn registry_entity_id_stays_stable_across_rename_and_runtime_restart() {
        let (runtime, store) = runtime();
        let registry = Registries::new(runtime.clone()).unwrap();
        let resolver = registry_entity_resolver(registry);
        let integration = PersonIntegration::start(
            runtime.clone(),
            vec![person("stable-unique-id", "First Name", &[], None)],
            validator(),
            resolver,
        )
        .await
        .unwrap();
        let manager = integration.manager();
        let original_entity_id = manager.entity_id("stable-unique-id").unwrap().unwrap();
        assert_eq!(original_entity_id, "person.first_name");

        manager
            .reload_yaml(vec![person("stable-unique-id", "Changed Name", &[], None)])
            .unwrap();
        assert_eq!(
            manager.entity_id("stable-unique-id").unwrap().as_deref(),
            Some("person.first_name")
        );
        assert_eq!(
            runtime.get_state(&original_entity_id).unwrap().attributes["friendly_name"],
            "Changed Name"
        );
        integration.shutdown().await.unwrap();
        drop(runtime);

        let restarted_runtime = Runtime::new(store).unwrap();
        let restarted_registry = Registries::new(restarted_runtime.clone()).unwrap();
        let restarted = PersonIntegration::start(
            restarted_runtime.clone(),
            vec![person("stable-unique-id", "Changed Name", &[], None)],
            validator(),
            registry_entity_resolver(restarted_registry),
        )
        .await
        .unwrap();
        assert_eq!(
            restarted
                .manager()
                .entity_id("stable-unique-id")
                .unwrap()
                .as_deref(),
            Some("person.first_name")
        );
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registry_disable_removes_only_owned_state_and_reenable_restores_it() {
        let (runtime, _) = runtime();
        runtime
            .set_state(
                "person.foreign",
                "custom",
                attrs(json!({"friendly_name": "Foreign"})),
                Context::default(),
                false,
            )
            .unwrap();
        let registry = Registries::new(runtime.clone()).unwrap();
        let integration = PersonIntegration::start(
            runtime.clone(),
            vec![person("managed", "Managed", &[], None)],
            validator(),
            registry_entity_resolver(registry.clone()),
        )
        .await
        .unwrap();
        let manager = integration.manager();
        let entity_id = manager.entity_id("managed").unwrap().unwrap();
        registry
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();

        wait_until(|| manager.entity_id("managed").is_ok_and(|id| id.is_none())).await;
        assert!(runtime.get_state(&entity_id).is_none());
        assert_eq!(runtime.get_state("person.foreign").unwrap().state, "custom");

        registry
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            manager
                .entity_id("managed")
                .is_ok_and(|id| id.as_deref() == Some(entity_id.as_str()))
        })
        .await;
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, STATE_UNKNOWN);
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn person_tracker_fields_accept_single_or_list_and_filter_falsy_entries() {
        let single: PersonConfig = serde_json::from_value(json!({
            "id": "single",
            "name": "Single",
            "device_trackers": "device_tracker.phone"
        }))
        .unwrap();
        assert_eq!(single.device_trackers, vec!["device_tracker.phone"]);

        let list: PersonCreate = serde_json::from_value(json!({
            "name": "List",
            "device_trackers": [null, "", "device_tracker.phone", false, 0]
        }))
        .unwrap();
        assert_eq!(list.device_trackers, vec!["device_tracker.phone"]);

        let empty: PersonConfig = serde_json::from_value(json!({
            "id": "empty",
            "name": "Empty",
            "device_trackers": null
        }))
        .unwrap();
        assert!(empty.device_trackers.is_empty());
    }

    #[tokio::test]
    async fn stored_user_link_must_exist_and_be_unique_across_yaml_and_storage() {
        let (runtime, _) = runtime();
        let integration = start(
            runtime,
            vec![person("yaml_person", "YAML", &[], Some("yaml-user"))],
        )
        .await;
        let manager = integration.manager();

        let duplicate_yaml = manager.create(PersonCreate {
            name: "Editable duplicate".into(),
            user_id: Some("yaml-user".into()),
            ..PersonCreate::default()
        });
        assert_eq!(
            duplicate_yaml.unwrap_err(),
            Error::InvalidInput("User already taken".into())
        );
        let missing = manager.create(PersonCreate {
            name: "Missing user".into(),
            user_id: Some("missing".into()),
            ..PersonCreate::default()
        });
        assert_eq!(
            missing.unwrap_err(),
            Error::InvalidInput("User does not exist".into())
        );

        let stored = manager
            .create(PersonCreate {
                name: "Stored".into(),
                user_id: Some("stored-user".into()),
                device_trackers: vec!["device_tracker.phone".into()],
                ..PersonCreate::default()
            })
            .unwrap();
        let duplicate_storage = manager.create(PersonCreate {
            name: "Also stored".into(),
            user_id: Some("stored-user".into()),
            ..PersonCreate::default()
        });
        assert_eq!(
            duplicate_storage.unwrap_err(),
            Error::InvalidInput("User already taken".into())
        );
        let linked_yaml = manager.person_for_user("yaml-user").unwrap().unwrap();
        assert!(!linked_yaml.editable);
        assert_eq!(linked_yaml.person.id, "yaml_person");
        let linked_storage = manager.person_for_user("stored-user").unwrap().unwrap();
        assert!(linked_storage.editable);
        assert_eq!(linked_storage.person.id, stored.id);
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn updating_user_to_same_value_skips_validator_but_new_user_is_checked() {
        let (runtime, _) = runtime();
        let calls = Arc::new(Mutex::new(0usize));
        let validator_calls = calls.clone();
        let user_validator: PersonUserValidator = Arc::new(move |_| {
            *lock(&validator_calls) += 1;
            Ok(true)
        });
        let integration =
            PersonIntegration::start(runtime, vec![], user_validator, name_entity_resolver())
                .await
                .unwrap();
        let manager = integration.manager();
        let stored = manager
            .create(PersonCreate {
                name: "Sam".into(),
                user_id: Some("user-1".into()),
                ..PersonCreate::default()
            })
            .unwrap();
        let before = *lock(&calls);

        manager
            .update(
                &stored.id,
                PersonUpdate {
                    name: Some("Sam renamed".into()),
                    device_trackers: Some(vec!["device_tracker.phone".into()]),
                    ..PersonUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(*lock(&calls), before);

        manager
            .update(
                &stored.id,
                PersonUpdate {
                    user_id: FieldUpdate::Set(Some("user-2".into())),
                    device_trackers: Some(vec!["device_tracker.phone".into()]),
                    ..PersonUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(*lock(&calls), before + 1);
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tracker_lookup_helpers_cover_yaml_and_storage() {
        let (runtime, _) = runtime();
        let integration = start(
            runtime,
            vec![person("yaml", "YAML", &["device_tracker.phone"], None)],
        )
        .await;
        let manager = integration.manager();
        let stored = manager
            .create(PersonCreate {
                name: "Stored".into(),
                device_trackers: vec![
                    "device_tracker.phone".into(),
                    "device_tracker.tablet".into(),
                ],
                ..PersonCreate::default()
            })
            .unwrap();

        let mut people = manager.persons_with_entity("device_tracker.phone").unwrap();
        people.sort();
        assert_eq!(people, vec!["person.stored", "person.yaml"]);
        assert_eq!(
            manager
                .entities_in_person(&manager.entity_id(&stored.id).unwrap().unwrap())
                .unwrap(),
            vec!["device_tracker.phone", "device_tracker.tablet"]
        );
        assert!(
            manager
                .entities_in_person("sensor.other")
                .unwrap()
                .is_empty()
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn add_user_device_tracker_updates_only_a_linked_storage_person_once() {
        let (runtime, _) = runtime();
        let integration = start(runtime, vec![]).await;
        let manager = integration.manager();
        let stored = manager
            .create(PersonCreate {
                name: "Sam".into(),
                user_id: Some("known".into()),
                ..PersonCreate::default()
            })
            .unwrap();

        manager
            .add_user_device_tracker("known", "device_tracker.phone")
            .unwrap();
        manager
            .add_user_device_tracker("known", "device_tracker.phone")
            .unwrap();
        manager
            .add_user_device_tracker("unlinked", "device_tracker.tablet")
            .unwrap();

        assert_eq!(
            manager.get(&stored.id).unwrap().unwrap().device_trackers,
            vec!["device_tracker.phone"]
        );
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn only_person_state_string_changes_are_significant() {
        assert!(is_significant_change("home", "not_home"));
        assert!(!is_significant_change("home", "home"));
    }

    #[tokio::test]
    async fn storage_crud_updates_states_and_private_collection_feed() {
        let (runtime, _) = runtime();
        let integration = start(runtime.clone(), vec![person("yaml", "YAML", &[], None)]).await;
        let manager = integration.manager();
        let (snapshot, mut changes) = manager.snapshot_and_subscribe().unwrap();
        assert!(snapshot.storage.is_empty());
        assert_eq!(snapshot.config.len(), 1);

        let created = manager
            .create(PersonCreate {
                name: "Sam Smith".into(),
                device_trackers: vec!["device_tracker.phone".into()],
                ..PersonCreate::default()
            })
            .unwrap();
        assert_eq!(created.id, "sam_smith");
        let event = changes.recv().await.unwrap();
        assert_eq!(event.event_type, PERSON_COLLECTION_UPDATED_SIGNAL);
        assert_eq!(event.data["changes"][0]["change_type"], "added");
        assert_eq!(event.data["changes"][0]["person_id"], created.id);
        assert_eq!(event.data["changes"][0]["item"]["name"], "Sam Smith");
        let created_state = runtime.get_state("person.sam_smith").unwrap();
        assert_eq!(created_state.attributes["editable"], true);
        assert_eq!(
            created_state.attributes["device_trackers"],
            json!(["device_tracker.phone"])
        );

        let updated = manager
            .update(
                &created.id,
                PersonUpdate {
                    name: Some("Sam".into()),
                    device_trackers: Some(vec!["device_tracker.phone".into()]),
                    picture: FieldUpdate::Set(Some("/api/image/1".into())),
                    ..PersonUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(updated.name, "Sam");
        assert_eq!(
            changes.recv().await.unwrap().data["changes"][0]["change_type"],
            "updated"
        );
        let updated_entity_id = manager.entity_id(&created.id).unwrap().unwrap();
        assert_eq!(updated_entity_id, "person.sam");
        assert!(runtime.get_state("person.sam_smith").is_none());
        let updated_state = runtime.get_state(&updated_entity_id).unwrap();
        assert_eq!(updated_state.attributes["friendly_name"], "Sam");
        assert_eq!(updated_state.attributes["entity_picture"], "/api/image/1");

        let removed = manager.delete(&created.id).unwrap();
        assert_eq!(removed.id, created.id);
        assert_eq!(
            changes.recv().await.unwrap().data["changes"][0]["change_type"],
            "removed"
        );
        assert!(runtime.get_state(&updated_entity_id).is_none());
        assert!(manager.list().unwrap().is_empty());
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn storage_update_defaults_omitted_tracker_list_to_empty() {
        let (runtime, _) = runtime();
        let integration = start(runtime, vec![]).await;
        let manager = integration.manager();
        let created = manager
            .create(PersonCreate {
                name: "Sam".into(),
                device_trackers: vec!["device_tracker.phone".into()],
                ..PersonCreate::default()
            })
            .unwrap();

        let updated = manager
            .update(
                &created.id,
                PersonUpdate {
                    name: Some("Samuel".into()),
                    ..PersonUpdate::default()
                },
            )
            .unwrap();
        assert!(updated.device_trackers.is_empty());
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tracker_state_changes_update_person_and_emit_fresh_person_context() {
        let (runtime, _) = runtime();
        let integration = start(
            runtime.clone(),
            vec![person("alex", "Alex", &["device_tracker.phone"], None)],
        )
        .await;
        integration.mark_started().unwrap();
        runtime
            .set_state(
                "device_tracker.phone",
                "home",
                attrs(json!({"source_type": "gps", "in_zones": ["zone.home"]})),
                Context {
                    user_id: Some("operator".into()),
                    ..Context::default()
                },
                false,
            )
            .unwrap();

        wait_until(|| {
            runtime
                .get_state("person.alex")
                .is_some_and(|state| state.state == "home")
        })
        .await;
        let state = runtime.get_state("person.alex").unwrap();
        assert_eq!(state.attributes["in_zones"], json!(["zone.home"]));
        assert_eq!(state.context.user_id, None);
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn user_removed_unlinks_storage_and_matches_update_schema_tracker_default() {
        let (runtime, _) = runtime();
        let integration = start(runtime.clone(), vec![]).await;
        let manager = integration.manager();
        let created = manager
            .create(PersonCreate {
                name: "Sam".into(),
                user_id: Some("known".into()),
                device_trackers: vec!["device_tracker.phone".into()],
                ..PersonCreate::default()
            })
            .unwrap();
        runtime
            .fire_event(
                "user_removed",
                attrs(json!({"user_id": "known"})),
                Context::default(),
            )
            .unwrap();

        wait_until(|| {
            manager
                .get(&created.id)
                .ok()
                .flatten()
                .is_some_and(|person| person.user_id.is_none() && person.device_trackers.is_empty())
        })
        .await;
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn yaml_reload_removes_stale_states_and_preserves_storage_people() {
        let (runtime, _) = runtime();
        let integration = start(
            runtime.clone(),
            vec![person("old_yaml", "Old YAML", &[], None)],
        )
        .await;
        let manager = integration.manager();
        let stored = manager
            .create(PersonCreate {
                name: "Stored".into(),
                ..PersonCreate::default()
            })
            .unwrap();
        manager
            .reload_yaml(vec![person("new_yaml", "New YAML", &[], None)])
            .unwrap();

        assert!(runtime.get_state("person.old_yaml").is_none());
        assert!(runtime.get_state("person.new_yaml").is_some());
        assert!(
            runtime
                .get_state(&manager.entity_id(&stored.id).unwrap().unwrap())
                .is_some()
        );
        assert_eq!(manager.list().unwrap()[0].id, stored.id);
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn storage_and_runtime_states_survive_a_runtime_restart() {
        let (runtime, store) = runtime();
        let integration = start(runtime.clone(), vec![]).await;
        let manager = integration.manager();
        let stored = manager
            .create(PersonCreate {
                name: "Sam".into(),
                user_id: Some("known".into()),
                device_trackers: vec!["device_tracker.phone".into()],
                ..PersonCreate::default()
            })
            .unwrap();
        integration.shutdown().await.unwrap();
        drop(runtime);

        let restarted_runtime = Runtime::new(store).expect("reopened runtime");
        let restarted = start(restarted_runtime.clone(), vec![]).await;
        assert_eq!(restarted.manager().list().unwrap(), vec![stored.clone()]);
        assert_eq!(
            restarted_runtime
                .get_state("person.sam")
                .unwrap()
                .attributes["user_id"],
            "known"
        );
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_cleans_stale_owned_yaml_states_after_restart() {
        let (runtime, store) = runtime();
        let first = start(
            runtime.clone(),
            vec![person("removed_yaml", "Removed", &[], None)],
        )
        .await;
        assert!(runtime.get_state("person.removed").is_some());
        first.shutdown().await.unwrap();
        drop(runtime);

        let restarted_runtime = Runtime::new(store).unwrap();
        let restarted = start(restarted_runtime.clone(), vec![]).await;
        assert!(restarted_runtime.get_state("person.removed").is_none());
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_invalidates_manager_and_releases_runtime_store_lease() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = start(runtime.clone(), vec![]).await;
        let manager = integration.manager();

        integration.shutdown().await.unwrap();
        assert!(manager.list().is_err());
        drop(runtime);

        let reopened = Runtime::new(store).expect("store lease should be released");
        assert!(reopened.states().is_empty());
    }
}
