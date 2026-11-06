//! Native Home Assistant tag collection and scan metadata.
//!
//! A tag scan is represented by a public `tag_scanned` event and a persisted
//! editable collection entry. Collection deltas use a separate private signal
//! so API subscribers never expose storage implementation details on the
//! public event bus.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use hass_core::{Context, Error, Event, Result, Runtime};
use hass_entity::{EntityHandle, EntityLifecycle, EntitySnapshot, EntitySpec, EntityStatus};
use hass_registry::{EntityEntry, EntityUpdate, ProviderEntityInfo, Registries};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Home Assistant tag integration domain.
pub const DOMAIN: &str = "tag";
/// Public event fired when a tag is scanned.
pub const EVENT_TAG_SCANNED: &str = "tag_scanned";
/// Private signal carrying persisted tag collection changes.
pub const TAG_COLLECTION_UPDATED_SIGNAL: &str = "tag_collection_updated";
/// Current collection minor version. Version 1.2 moved names to the shared
/// entity registry and version 1.3 removed the duplicate `tag_id` field.
pub const STORAGE_MINOR_VERSION: u32 = 3;

const STORAGE_KEY: &str = "hass-tag.collection.v1";
const STORAGE_VERSION: u32 = 1;
const ENTITY_PROVIDER_ID: &str = "tag";
const MAX_SIGNAL_BYTES: usize = 1024 * 1024;

/// A tag timestamp that preserves Home Assistant's timezone-naive or
/// fixed-offset ISO datetime representation.
#[derive(Clone, Debug, PartialEq)]
pub struct TagDateTime(DateTimeValue);

#[derive(Clone, Debug, PartialEq)]
enum DateTimeValue {
    Offset(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
}

impl TagDateTime {
    /// Parse a timestamp accepted by the pinned tag collection datetime
    /// validator. Naive values remain naive; explicit offsets are preserved.
    pub fn parse(value: &str) -> Result<Self> {
        parse_source_datetime(value)
            .map(Self)
            .ok_or_else(|| Error::InvalidInput("invalid tag datetime".into()))
    }

    fn now_utc() -> Self {
        let mut now = Utc::now();
        let micros = now.nanosecond() / 1_000 * 1_000;
        now = now.with_nanosecond(micros).unwrap_or(now);
        Self(DateTimeValue::Offset(now.fixed_offset()))
    }

    fn to_storage_string(&self) -> String {
        match &self.0 {
            DateTimeValue::Offset(value) => {
                format_naive(value.naive_local(), false) + &offset_string(*value.offset())
            }
            DateTimeValue::Naive(value) => format_naive(*value, false),
        }
    }

    fn to_state_string(&self) -> String {
        match &self.0 {
            DateTimeValue::Offset(value) => {
                format_naive(value.naive_local(), true) + &offset_string(*value.offset())
            }
            DateTimeValue::Naive(value) => format_naive(*value, true),
        }
    }
}

/// A tag as exposed by list and collection-change operations.
///
/// Names come from the shared entity registry. A missing registry entry leaves
/// `name` absent, matching the source collection's list operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagConfig {
    /// Stable tag identifier.
    pub id: String,
    /// Effective name from the entity registry, if the registry entry exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional user description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Last scan time, serialized as an ISO 8601 UTC offset timestamp.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_datetime"
    )]
    pub last_scanned: Option<TagDateTime>,
    /// Device identifier that most recently scanned the tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// Fields accepted when creating a tag. An absent or empty `tag_id` requests a
/// generated UUID, as in the pinned collection schema.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagCreate {
    /// Optional caller-supplied tag identifier.
    #[serde(default)]
    pub tag_id: Option<String>,
    /// Optional nonempty entity name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional description; an empty string is valid.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional preexisting last-scan timestamp.
    #[serde(default, with = "optional_datetime")]
    pub last_scanned: Option<TagDateTime>,
    /// Optional device ID; an empty string is accepted for explicit CRUD.
    #[serde(default)]
    pub device_id: Option<String>,
}

/// Mutable fields accepted by tag update operations.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagUpdate {
    /// Optional nonempty entity name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional description; an empty string is valid.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional replacement last-scan timestamp.
    #[serde(default, with = "optional_datetime")]
    pub last_scanned: Option<TagDateTime>,
    /// Optional device ID; an empty string is accepted for explicit CRUD.
    #[serde(default)]
    pub device_id: Option<String>,
}

/// One generic collection delta. The API websocket protocol uses `tag_id` as
/// the collection item key and `item` as the resulting row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagCollectionChange {
    /// `added`, `updated`, or `removed`.
    pub change_type: String,
    /// Stable ID of the changed tag.
    pub tag_id: String,
    /// Current row, or the removed row for a removal.
    pub item: TagConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTag {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_datetime"
    )]
    last_scanned: Option<TagDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCollection {
    version: u32,
    minor_version: u32,
    items: Vec<StoredTag>,
}

struct TagData {
    tags: Vec<StoredTag>,
    entities: BTreeMap<String, EntityHandle>,
}

struct TagRegistryCheckpoint {
    active: Option<EntityEntry>,
    removed: Option<EntityEntry>,
    applied_name: Option<String>,
}

struct Shared {
    runtime: Runtime,
    registries: Registries,
    lifecycle: EntityLifecycle,
    data: Mutex<TagData>,
    active: AtomicBool,
}

/// Weak handle for tag collection operations.
#[derive(Clone)]
pub struct TagManager {
    shared: Weak<Shared>,
}

/// Owns the persisted tag collection and its shared entity-provider leases.
pub struct TagIntegration {
    manager: TagManager,
    shared: Arc<Shared>,
    cleanup_on_drop: bool,
}

impl TagIntegration {
    /// Load persisted tags, register their entity identities, and publish their
    /// restored states using the application's shared entity lifecycle.
    pub fn start(
        runtime: Runtime,
        registries: Registries,
        lifecycle: EntityLifecycle,
    ) -> Result<Self> {
        let loaded = runtime.get_data(STORAGE_KEY)?;
        let (tags, legacy_names, migrated) = load_collection(loaded)?;
        let shared = Arc::new(Shared {
            runtime,
            registries,
            lifecycle,
            data: Mutex::new(TagData {
                tags,
                entities: BTreeMap::new(),
            }),
            active: AtomicBool::new(true),
        });

        let manager = TagManager {
            shared: Arc::downgrade(&shared),
        };
        let mut data = lock(&shared.data);
        let tags = data.tags.clone();
        let result = (|| {
            let entities = register_entities(&shared, &data.entities, &tags, &legacy_names)?;
            data.entities = entities;
            for (id, name) in &legacy_names {
                set_registry_name(&shared.registries, id, name)?;
            }
            publish_all(&data.tags, &data.entities)?;
            if migrated {
                persist_collection(&shared.runtime, &data.tags)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            shared.active.store(false, Ordering::Release);
            let _ = shared
                .lifecycle
                .register_provider_batch(ENTITY_PROVIDER_ID, Vec::new());
            return Err(error);
        }
        drop(data);

        Ok(Self {
            manager,
            shared,
            cleanup_on_drop: true,
        })
    }

    /// Return a weak manager for API and provider callers.
    pub fn manager(&self) -> TagManager {
        self.manager.clone()
    }

    /// Deactivate weak handles and release all provider leases.
    pub fn shutdown(mut self) -> Result<()> {
        let _data = lock(&self.shared.data);
        self.shared.active.store(false, Ordering::Release);
        self.shared
            .lifecycle
            .register_provider_batch(ENTITY_PROVIDER_ID, Vec::new())?;
        self.cleanup_on_drop = false;
        Ok(())
    }
}

impl Drop for TagIntegration {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        let _data = lock(&self.shared.data);
        self.shared.active.store(false, Ordering::Release);
        let _ = self
            .shared
            .lifecycle
            .register_provider_batch(ENTITY_PROVIDER_ID, Vec::new());
    }
}

impl TagManager {
    /// List persisted tags in collection insertion order and resolve names
    /// from the shared entity registry at read time.
    pub fn list(&self) -> Result<Vec<TagConfig>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(configs(&shared.registries, &data.tags))
    }

    /// Get one persisted tag by ID.
    pub fn get(&self, id: &str) -> Result<Option<TagConfig>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let Some(tag) = data.tags.iter().find(|tag| tag.id == id) else {
            return Ok(None);
        };
        Ok(Some(config(&shared.registries, tag)))
    }

    /// Create a tag and its entity-registry-backed entity.
    pub fn create(&self, create: TagCreate) -> Result<TagConfig> {
        let shared = self.live()?;
        validate_create(&create)?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;

        let id = create
            .tag_id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if data.tags.iter().any(|tag| tag.id == id) {
            return Err(Error::HomeAssistant(format!(
                "Tag with ID {id} already exists."
            )));
        }
        let tag = StoredTag {
            id: id.clone(),
            description: create.description,
            last_scanned: create.last_scanned,
            device_id: create.device_id,
        };
        let mut next = data.tags.clone();
        next.push(tag.clone());
        let proposed_name = create.name.clone();
        let item = config_with_name(&tag, proposed_name.clone());
        let signal = signal_data(collection_change("added", &id, item.clone()))?;
        let checkpoint = TagRegistryCheckpoint {
            active: registry_entry(&shared.registries, &id),
            removed: shared
                .registries
                .removed_provider_entity(DOMAIN, DOMAIN, &id),
            applied_name: proposed_name.clone(),
        };

        persist_collection(&shared.runtime, &next)?;
        let entities = match register_entities(
            &shared,
            &data.entities,
            &next,
            &proposed_name
                .clone()
                .map(|name| BTreeMap::from([(id.clone(), name)]))
                .unwrap_or_default(),
        ) {
            Ok(entities) => entities,
            Err(error) => {
                return Err(rollback_create(
                    &shared,
                    &mut data,
                    &tag,
                    &checkpoint,
                    None,
                    error,
                ));
            }
        };
        let publication = (|| {
            if let Some(name) = proposed_name.as_deref() {
                set_registry_name(&shared.registries, &id, name)?;
            }
            publish_all(&next, &entities)
        })();
        if let Err(error) = publication {
            return Err(rollback_create(
                &shared,
                &mut data,
                &tag,
                &checkpoint,
                Some(&entities),
                error,
            ));
        }
        data.tags = next;
        data.entities = entities;
        dispatch_collection_change(&shared.runtime, signal)?;
        Ok(item)
    }

    /// Update one persisted tag. The entity registry is the authority for a
    /// custom name; a missing registry entry makes a name update not found.
    pub fn update(&self, id: &str, update: TagUpdate) -> Result<TagConfig> {
        let shared = self.live()?;
        validate_update(&update)?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let Some(old) = data.tags.iter().find(|tag| tag.id == id).cloned() else {
            return Err(Error::NotFound(format!("tag {id}")));
        };

        let mut next_tag = old.clone();
        if let Some(description) = update.description {
            next_tag.description = Some(description);
        }
        if let Some(last_scanned) = update.last_scanned {
            next_tag.last_scanned = Some(last_scanned);
        }
        if let Some(device_id) = update.device_id {
            next_tag.device_id = Some(device_id);
        }
        let mut next = data.tags.clone();
        let Some(index) = next.iter().position(|tag| tag.id == id) else {
            return Err(Error::NotFound(format!("tag {id}")));
        };
        next[index] = next_tag.clone();
        let item = config_with_name(
            &next_tag,
            update
                .name
                .clone()
                .or_else(|| effective_name(&shared.registries, id)),
        );
        let signal = signal_data(collection_change("updated", id, item.clone()))?;
        let prior_entry = registry_entry(&shared.registries, id);
        let publication = (|| {
            if let Some(name) = update.name.as_deref() {
                set_registry_name(&shared.registries, id, name)?;
            }
            persist_collection(&shared.runtime, &next)?;
            publish_tag(&next_tag, data.entities.get(id))
        })();
        if let Err(mut error) = publication {
            if update.name.is_some()
                && let Some(entry) = prior_entry.as_ref()
            {
                error = rollback_step(
                    error,
                    "restore registry name",
                    restore_registry_name(&shared.registries, entry, update.name.as_deref()),
                );
            }
            error = rollback_step(
                error,
                "restore entity state",
                publish_tag(&old, data.entities.get(id)),
            );
            return Err(restore_collection(&shared.runtime, &data.tags, error));
        }
        data.tags = next;
        dispatch_collection_change(&shared.runtime, signal)?;
        Ok(config(
            &shared.registries,
            data.tags
                .iter()
                .find(|tag| tag.id == id)
                .expect("updated tag"),
        ))
    }

    /// Delete one tag, its provider lease, and its entity-registry entry.
    pub fn delete(&self, id: &str) -> Result<()> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let Some(index) = data.tags.iter().position(|tag| tag.id == id) else {
            return Err(Error::NotFound(format!("tag {id}")));
        };
        let old = data.tags[index].clone();
        let prior_entry = registry_entry(&shared.registries, id);
        let item = config(&shared.registries, &old);
        let signal = signal_data(collection_change("removed", id, item))?;
        let mut next = data.tags.clone();
        next.remove(index);
        persist_collection(&shared.runtime, &next)?;

        // Remove the registry identity while its lease is still attached. A
        // provider reconciliation releases registered entities as restored /
        // unavailable, so detaching first would leave that state orphaned.
        let removal = (|| {
            if let Some(entry) = registry_entry(&shared.registries, id) {
                shared.registries.remove_entity(&entry.entity_id)?;
            }
            publish_tag(&old, data.entities.get(id))?;
            register_entities(&shared, &data.entities, &next, &BTreeMap::new())
        })();
        let entities = match removal {
            Ok(entities) => entities,
            Err(mut error) => {
                let mut previous = data.entities.clone();
                if let Some(entry) = prior_entry.as_ref() {
                    error = rollback_step(
                        error,
                        "restore removed registry identity",
                        shared.registries.restore_removed_provider_entity(entry),
                    );
                    // This operation removed the identity. Explicit rollback
                    // may recreate it; unrelated reconciliations may not.
                    previous.remove(id);
                }
                match reconcile_entities(&shared, &previous, &data.tags, &BTreeMap::new()) {
                    Ok(entities) => data.entities = entities,
                    Err(rollback) => {
                        error = rollback_step(error, "restore entity lease", Err(rollback))
                    }
                }
                error = rollback_step(
                    error,
                    "restore entity state",
                    publish_all(&data.tags, &data.entities),
                );
                return Err(restore_collection(&shared.runtime, &data.tags, error));
            }
        };
        data.tags = next;
        data.entities = entities;
        dispatch_collection_change(&shared.runtime, signal)
    }

    /// Handle a scan from a provider caller. The public event is intentionally
    /// fired before metadata persistence, matching `async_scan_tag`; callers
    /// may observe the scan event even if the subsequent collection write
    /// fails. No HTTP, QR, or hardware transport is implied by this method.
    pub fn scan(&self, tag_id: &str, device_id: Option<&str>, context: Context) -> Result<()> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;

        let event_name = effective_name(&shared.registries, tag_id);
        let mut event_data = Map::new();
        event_data.insert("tag_id".into(), Value::String(tag_id.to_owned()));
        event_data.insert("name".into(), event_name.map_or(Value::Null, Value::String));
        event_data.insert(
            "device_id".into(),
            device_id.map_or(Value::Null, |value| Value::String(value.to_owned())),
        );
        shared
            .runtime
            .fire_event(EVENT_TAG_SCANNED, event_data, context)?;

        let now = TagDateTime::now_utc();
        let change_type;
        let stored_id;
        let mut next = data.tags.clone();
        if let Some(current) = next.iter_mut().find(|tag| tag.id == tag_id) {
            current.last_scanned = Some(now.clone());
            // Upstream scan updates omit a falsey device ID; a scan without a
            // device therefore preserves an existing stored device ID.
            if let Some(device_id) = device_id.filter(|value| !value.is_empty()) {
                current.device_id = Some(device_id.to_owned());
            }
            stored_id = tag_id.to_owned();
            change_type = "updated";
        } else {
            let id = if tag_id.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                tag_id.to_owned()
            };
            let tag = StoredTag {
                id: id.clone(),
                description: None,
                last_scanned: Some(now.clone()),
                device_id: device_id
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            };
            next.push(tag);
            stored_id = id;
            change_type = "added";
        }

        let Some(updated) = next.iter().find(|tag| tag.id == stored_id) else {
            unreachable!("scan inserted or updated a tag")
        };
        let proposed_name = if change_type == "added" {
            None
        } else {
            effective_name(&shared.registries, &stored_id)
        };
        let signal = signal_data(collection_change(
            change_type,
            &stored_id,
            config_with_name(updated, proposed_name),
        ))?;
        persist_collection(&shared.runtime, &next)?;

        if change_type == "added" {
            let entities = match register_entities(&shared, &data.entities, &next, &BTreeMap::new())
            {
                Ok(entities) => entities,
                Err(error) => {
                    if let Ok(entities) =
                        reconcile_entities(&shared, &data.entities, &data.tags, &BTreeMap::new())
                    {
                        data.entities = entities;
                    }
                    return Err(restore_collection(&shared.runtime, &data.tags, error));
                }
            };
            if let Err(error) = publish_all(&next, &entities) {
                if let Ok(entities) =
                    reconcile_entities(&shared, &data.entities, &data.tags, &BTreeMap::new())
                {
                    data.entities = entities;
                }
                return Err(restore_collection(&shared.runtime, &data.tags, error));
            }
            data.entities = entities;
        } else {
            if let Err(error) = publish_tag(updated, data.entities.get(&stored_id)) {
                return Err(restore_collection(&shared.runtime, &data.tags, error));
            }
        }
        data.tags = next;
        dispatch_collection_change(&shared.runtime, signal)
    }

    /// Atomically take the initial collection snapshot and subscribe to private
    /// changes that occur after it.
    pub fn snapshot_and_subscribe(&self) -> Result<(Vec<TagConfig>, broadcast::Receiver<Event>)> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let receiver = shared.runtime.subscribe_signals();
        Ok((configs(&shared.registries, &data.tags), receiver))
    }

    fn live(&self) -> Result<Arc<Shared>> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| Error::Service("tag integration is closed".into()))?;
        Self::ensure_active(&shared)?;
        Ok(shared)
    }

    fn ensure_active(shared: &Shared) -> Result<()> {
        if shared.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::Service("tag integration is closed".into()))
        }
    }
}

fn validate_create(create: &TagCreate) -> Result<()> {
    if create.name.as_ref().is_some_and(String::is_empty) {
        return Err(Error::InvalidInput("tag name must be nonempty".into()));
    }
    Ok(())
}

fn validate_update(update: &TagUpdate) -> Result<()> {
    if update.name.as_ref().is_some_and(String::is_empty) {
        return Err(Error::InvalidInput("tag name must be nonempty".into()));
    }
    Ok(())
}

fn collection_change(change_type: &str, id: &str, item: TagConfig) -> TagCollectionChange {
    TagCollectionChange {
        change_type: change_type.to_owned(),
        tag_id: id.to_owned(),
        item,
    }
}

fn signal_data(change: TagCollectionChange) -> Result<Map<String, Value>> {
    let changes = serde_json::to_value([change]).map_err(|_| {
        Error::InvalidInput("tag collection change is not JSON serializable".into())
    })?;
    let mut data = Map::new();
    data.insert("changes".into(), changes);
    let encoded = serde_json::to_vec(&data).map_err(|_| {
        Error::InvalidInput("tag collection change is not JSON serializable".into())
    })?;
    if encoded.len() > MAX_SIGNAL_BYTES {
        return Err(Error::InvalidInput(format!(
            "tag collection change exceeds {MAX_SIGNAL_BYTES} encoded bytes"
        )));
    }
    Ok(data)
}

fn dispatch_collection_change(runtime: &Runtime, data: Map<String, Value>) -> Result<()> {
    runtime.dispatch_signal(TAG_COLLECTION_UPDATED_SIGNAL, data, Context::default())?;
    Ok(())
}

fn configs(registries: &Registries, tags: &[StoredTag]) -> Vec<TagConfig> {
    let entries = tag_registry_entries(registries);
    tags.iter()
        .map(|tag| config_with_entry(tag, entries.get(&tag.id)))
        .collect()
}

fn config(registries: &Registries, tag: &StoredTag) -> TagConfig {
    config_with_entry(tag, registry_entry(registries, &tag.id).as_ref())
}

fn config_with_name(tag: &StoredTag, name: Option<String>) -> TagConfig {
    TagConfig {
        id: tag.id.clone(),
        name,
        description: tag.description.clone(),
        last_scanned: tag.last_scanned.clone(),
        device_id: tag.device_id.clone(),
    }
}

fn config_with_entry(tag: &StoredTag, entry: Option<&EntityEntry>) -> TagConfig {
    config_with_name(tag, entry.and_then(effective_entry_name))
}

fn effective_name(registries: &Registries, tag_id: &str) -> Option<String> {
    registry_entry(registries, tag_id)
        .as_ref()
        .and_then(effective_entry_name)
}

fn effective_entry_name(entry: &EntityEntry) -> Option<String> {
    entry
        .name
        .as_ref()
        .filter(|name| !name.is_empty())
        .cloned()
        .or_else(|| entry.original_name.clone())
}

fn registry_entry(registries: &Registries, tag_id: &str) -> Option<EntityEntry> {
    registries.entities().into_iter().find(|entry| {
        entry.domain == DOMAIN && entry.platform == DOMAIN && entry.unique_id == tag_id
    })
}

fn tag_registry_entries(registries: &Registries) -> BTreeMap<String, EntityEntry> {
    registries
        .entities()
        .into_iter()
        .filter(|entry| entry.domain == DOMAIN && entry.platform == DOMAIN)
        .map(|entry| (entry.unique_id.clone(), entry))
        .collect()
}

fn set_registry_name(registries: &Registries, tag_id: &str, name: &str) -> Result<()> {
    let entry = registry_entry(registries, tag_id)
        .ok_or_else(|| Error::NotFound(format!("tag {tag_id}")))?;
    if entry.name.as_deref() != Some(name) {
        registries.update_entity(
            &entry.entity_id,
            EntityUpdate {
                name: Some(Some(name.to_owned())),
                ..EntityUpdate::default()
            },
        )?;
    }
    Ok(())
}

fn restore_registry_name(
    registries: &Registries,
    previous: &EntityEntry,
    applied: Option<&str>,
) -> Result<()> {
    if let Some(applied) = applied {
        registries.restore_provider_entity_name_if_unchanged(previous, applied)?;
    }
    Ok(())
}

fn rollback_step(error: Error, step: &str, result: Result<()>) -> Error {
    match result {
        Ok(()) => error,
        Err(rollback) => Error::Storage(format!("{error}; {step} failed: {rollback}")),
    }
}

fn rollback_create(
    shared: &Shared,
    data: &mut TagData,
    tag: &StoredTag,
    checkpoint: &TagRegistryCheckpoint,
    staged: Option<&BTreeMap<String, EntityHandle>>,
    mut error: Error,
) -> Error {
    // Keep a lease attached until the newly created registry identity has
    // been removed, so cleanup can compare its exact last-owned state.
    let mut recovered = None;
    if checkpoint.active.is_none()
        && staged.is_none()
        && registry_entry(&shared.registries, &tag.id).is_some()
    {
        let mut candidates = data.tags.clone();
        candidates.push(tag.clone());
        match register_entities(shared, &data.entities, &candidates, &BTreeMap::new()) {
            Ok(entities) => recovered = Some(entities),
            Err(rollback) => {
                error = rollback_step(error, "recover staged entity lease", Err(rollback))
            }
        }
    }
    let staged = staged.or(recovered.as_ref());
    if checkpoint.active.is_none() {
        let replacement = registry_entry(&shared.registries, &tag.id);
        if let Some(entry) = replacement.as_ref() {
            error = rollback_step(
                error,
                "remove created registry entry",
                shared.registries.remove_entity(&entry.entity_id),
            );
        }
        error = rollback_step(
            error,
            "remove created entity state",
            publish_tag(tag, staged.and_then(|entities| entities.get(&tag.id))),
        );
        if let Some(previous) = checkpoint.removed.as_ref() {
            if let Some(replacement) = replacement.as_ref() {
                error = rollback_step(
                    error,
                    "restore previous registry tombstone",
                    shared
                        .registries
                        .restore_provider_entity_tombstone(previous, replacement),
                );
            } else if shared
                .registries
                .removed_provider_entity(DOMAIN, DOMAIN, &tag.id)
                .as_ref()
                != Some(previous)
            {
                error = rollback_step(
                    error,
                    "restore previous registry tombstone",
                    Err(Error::Storage(
                        "provider tombstone changed without an operation-owned replacement".into(),
                    )),
                );
            }
        }
    } else if let Some(previous) = checkpoint.active.as_ref() {
        error = rollback_step(
            error,
            "restore registry name",
            restore_registry_name(
                &shared.registries,
                previous,
                checkpoint.applied_name.as_deref(),
            ),
        );
    }
    match reconcile_entities(shared, &data.entities, &data.tags, &BTreeMap::new()) {
        Ok(entities) => data.entities = entities,
        Err(rollback) => error = rollback_step(error, "restore entity leases", Err(rollback)),
    }
    error = rollback_step(
        error,
        "restore entity states",
        publish_all(&data.tags, &data.entities),
    );
    restore_collection(&shared.runtime, &data.tags, error)
}

fn register_entities(
    shared: &Shared,
    previous: &BTreeMap<String, EntityHandle>,
    tags: &[StoredTag],
    migrated_names: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, EntityHandle>> {
    // Preserve removed leases until the collection item is deleted or the
    // provider is explicitly reloaded. Unrelated collection additions must
    // not recreate an entity removed through its registry.
    let removed: BTreeMap<_, _> = previous
        .iter()
        .filter(|(id, handle)| {
            tags.iter().any(|tag| &tag.id == *id)
                && matches!(
                    handle.status(),
                    EntityStatus::Removed { .. } | EntityStatus::Released { .. }
                )
                && registry_entry(&shared.registries, id).is_none()
        })
        .map(|(id, handle)| (id.clone(), handle.clone()))
        .collect();
    let ids: Vec<String> = tags
        .iter()
        .filter(|tag| !removed.contains_key(&tag.id))
        .map(|tag| tag.id.clone())
        .collect();
    let entries = tag_registry_entries(&shared.registries);
    let specs = ids
        .iter()
        .map(|id| {
            let entry = entries.get(id);
            let name = entry
                .and_then(effective_entry_name)
                .or_else(|| migrated_names.get(id).cloned());
            let suggested_object_id = entry
                .and_then(|entry| entry.suggested_object_id.clone())
                .or(name)
                .unwrap_or_else(|| id.clone());
            let original_name = entry
                .and_then(|entry| entry.original_name.clone())
                .unwrap_or_else(|| format!("Tag {id}"));
            let info = ProviderEntityInfo {
                original_name: Some(original_name),
                suggested_object_id: Some(suggested_object_id.clone()),
                ..ProviderEntityInfo::default()
            };
            EntitySpec::new(DOMAIN, DOMAIN, ENTITY_PROVIDER_ID, id.clone())
                .with_unrecorded_attributes(["tag_id"])
                .with_unique_id(id.clone())
                .with_suggested_object_id(suggested_object_id)
                .with_info(info)
        })
        .collect();
    let handles = shared
        .lifecycle
        .register_provider_batch(ENTITY_PROVIDER_ID, specs)?;
    let mut result = removed;
    for (id, handle) in ids.into_iter().zip(handles) {
        result.insert(id, handle);
    }
    Ok(result)
}

fn reconcile_entities(
    shared: &Shared,
    previous: &BTreeMap<String, EntityHandle>,
    tags: &[StoredTag],
    names: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, EntityHandle>> {
    register_entities(shared, previous, tags, names)
}

fn publish_all(tags: &[StoredTag], entities: &BTreeMap<String, EntityHandle>) -> Result<()> {
    for tag in tags {
        publish_tag(tag, entities.get(&tag.id))?;
    }
    Ok(())
}

fn publish_tag(tag: &StoredTag, handle: Option<&EntityHandle>) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    if matches!(
        handle.status(),
        EntityStatus::Released { .. } | EntityStatus::Shutdown { .. }
    ) {
        return Ok(());
    }
    let mut attributes = Map::new();
    attributes.insert("tag_id".into(), Value::String(tag.id.clone()));
    attributes.insert(
        "last_scanned_by_device_id".into(),
        tag.device_id
            .as_ref()
            .map_or(Value::Null, |device_id| Value::String(device_id.clone())),
    );
    let state = tag.last_scanned.clone().map(|time| time.to_state_string());
    let _ = handle.publish(EntitySnapshot {
        state,
        available: true,
        attributes,
        context: Context::default(),
    })?;
    Ok(())
}

fn persist_collection(runtime: &Runtime, tags: &[StoredTag]) -> Result<()> {
    let persisted = PersistedCollection {
        version: STORAGE_VERSION,
        minor_version: STORAGE_MINOR_VERSION,
        items: tags.to_vec(),
    };
    let value = serde_json::to_value(persisted)
        .map_err(|_| Error::Storage("tag collection is not JSON serializable".into()))?;
    runtime.set_data(STORAGE_KEY, &value)
}

fn restore_collection(runtime: &Runtime, previous: &[StoredTag], original_error: Error) -> Error {
    match persist_collection(runtime, previous) {
        Ok(()) => original_error,
        Err(rollback_error) => Error::Storage(format!(
            "tag collection update failed: {original_error}; restoring prior data failed: {rollback_error}"
        )),
    }
}

fn load_collection(
    value: Option<Value>,
) -> Result<(Vec<StoredTag>, BTreeMap<String, String>, bool)> {
    let Some(value) = value else {
        return Ok((Vec::new(), BTreeMap::new(), false));
    };
    let outer = value
        .as_object()
        .ok_or_else(|| Error::Storage("invalid persisted tag collection".into()))?;
    let version = parse_version(outer.get("version"), "version", 1)?;
    let minor = parse_version(outer.get("minor_version"), "minor_version", 0)?;
    if outer
        .get("key")
        .is_some_and(|key| key.as_str() != Some(DOMAIN))
    {
        return Err(Error::Storage(
            "persisted tag collection key is invalid".into(),
        ));
    }
    if outer.keys().any(|key| {
        !matches!(
            key.as_str(),
            "version" | "minor_version" | "key" | "data" | "items"
        )
    }) {
        return Err(Error::Storage(
            "persisted tag collection has unknown fields".into(),
        ));
    }
    let source_envelope = outer.get("data").is_some();
    let body = outer.get("data").unwrap_or(&value);
    if source_envelope
        && body
            .as_object()
            .is_none_or(|body| body.keys().any(|key| key != "items"))
    {
        return Err(Error::Storage(
            "persisted tag collection data has unknown fields".into(),
        ));
    }
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Storage("persisted tag collection has no items array".into()))?;
    if version != STORAGE_VERSION {
        return Err(Error::Storage(format!(
            "unsupported tag collection version {version}"
        )));
    }
    if minor > STORAGE_MINOR_VERSION {
        return Err(Error::Storage(format!(
            "unsupported tag collection minor version {minor}"
        )));
    }

    let mut tags = Vec::new();
    let mut names = BTreeMap::new();
    let mut changed = source_envelope || minor != STORAGE_MINOR_VERSION;
    for item in items {
        let mut item = item
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Storage("persisted tag item must be an object".into()))?;
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::Storage("persisted tag item has an invalid id".into()))?;
        if let Some(name) = item.remove("name") {
            changed = true;
            let name = name
                .as_str()
                .ok_or_else(|| Error::Storage("persisted tag name must be a string".into()))?;
            if !name.is_empty() {
                names.insert(id.clone(), name.to_owned());
            }
        }
        if item.remove("tag_id").is_some() {
            changed = true;
        }
        if item.remove("migrated").is_some() {
            changed = true;
        }
        if minor < 3 {
            changed = true;
        }
        let tag: StoredTag = serde_json::from_value(Value::Object(item))
            .map_err(|error| Error::Storage(format!("invalid persisted tag: {error}")))?;
        if tags.iter().any(|existing: &StoredTag| existing.id == id) {
            return Err(Error::Storage("duplicate persisted tag ID".into()));
        }
        tags.push(tag);
    }
    Ok((tags, names, changed))
}

fn parse_version(value: Option<&Value>, field: &str, default: u32) -> Result<u32> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| Error::Storage(format!("persisted tag {field} must be an integer")))
}

fn parse_source_datetime(value: &str) -> Option<DateTimeValue> {
    let (local, offset) = split_datetime_offset(value)?;
    let local = parse_naive_datetime(local)?;
    let micros = local.nanosecond() / 1_000;
    let local = local.with_nanosecond(micros * 1_000)?;
    match offset {
        Some(offset) => {
            let offset = parse_offset(offset)?;
            Some(DateTimeValue::Offset(
                local.and_local_timezone(offset).single()?,
            ))
        }
        None => Some(DateTimeValue::Naive(local)),
    }
}

fn split_datetime_offset(value: &str) -> Option<(&str, Option<&str>)> {
    let separator = value.find(['T', ' '])?;
    let time_start = separator + 1;
    let time = value.get(time_start..)?;
    if time.ends_with('Z') {
        return Some((&value[..value.len() - 1], Some("Z")));
    }
    if let Some((index, _)) = time.char_indices().find(|(_, ch)| matches!(ch, '+' | '-')) {
        let offset_start = time_start + index;
        return Some((&value[..offset_start], value.get(offset_start..)));
    }
    Some((value, None))
}

fn parse_naive_datetime(value: &str) -> Option<NaiveDateTime> {
    let (date, time) = value.split_once(['T', ' '])?;
    let mut date_parts = date.split('-');
    let year_value = date_parts.next()?;
    if year_value.len() != 4 {
        return None;
    }
    let year = parse_decimal(year_value, 4)?;
    let month = parse_decimal(date_parts.next()?, 2)?;
    let day = parse_decimal(date_parts.next()?, 2)?;
    if date_parts.next().is_some() {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year as i32, month, day)?;

    let mut time_parts = time.split(':');
    let hour = parse_decimal(time_parts.next()?, 2)?;
    let minute = parse_decimal(time_parts.next()?, 2)?;
    let second_and_fraction = time_parts.next();
    if time_parts.next().is_some() {
        return None;
    }
    let (second, micros) = if let Some(value) = second_and_fraction {
        if let Some((second, fraction)) = value.split_once('.') {
            if fraction.is_empty()
                || fraction.len() > 12
                || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let significant = &fraction[..fraction.len().min(6)];
            let parsed = parse_decimal(significant, 6)?;
            let scaled = parsed * 10_u32.pow((6 - significant.len()) as u32);
            (parse_decimal(second, 2)?, scaled)
        } else {
            (parse_decimal(value, 2)?, 0)
        }
    } else {
        (0, 0)
    };
    Some(NaiveDateTime::new(
        date,
        NaiveTime::from_hms_micro_opt(hour, minute, second, micros)?,
    ))
}

fn parse_decimal(value: &str, max_digits: usize) -> Option<u32> {
    if value.is_empty() || value.len() > max_digits || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_offset(value: &str) -> Option<FixedOffset> {
    if value == "Z" {
        return FixedOffset::east_opt(0);
    }
    let (sign, raw) = value.split_at(1);
    let digits: String = raw.chars().filter(|ch| *ch != ':').collect();
    if !matches!(digits.len(), 2 | 4 | 6) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits.get(..2)?.parse().ok()?;
    let minutes: i32 = if digits.len() >= 4 {
        digits.get(2..4)?.parse().ok()?
    } else {
        0
    };
    let seconds: i32 = if digits.len() == 6 {
        digits.get(4..6)?.parse().ok()?
    } else {
        0
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    let magnitude = hours.checked_mul(3_600)? + minutes * 60 + seconds;
    let seconds = match sign {
        "+" => magnitude,
        "-" => -magnitude,
        _ => return None,
    };
    FixedOffset::east_opt(seconds)
}

fn format_naive(value: NaiveDateTime, milliseconds: bool) -> String {
    let mut output = value.format("%Y-%m-%dT%H:%M:%S").to_string();
    let micros = value.nanosecond() / 1_000;
    if milliseconds {
        output.push_str(&format!(".{:03}", micros / 1_000));
    } else if micros != 0 {
        output.push_str(&format!(".{micros:06}"));
    }
    output
}

fn offset_string(offset: FixedOffset) -> String {
    let seconds = offset.local_minus_utc();
    let sign = if seconds < 0 { '-' } else { '+' };
    let magnitude = seconds.abs();
    let hours = magnitude / 3_600;
    let minutes = magnitude % 3_600 / 60;
    let seconds = magnitude % 60;
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

mod optional_datetime {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(
        value: &Option<TagDateTime>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&value.to_storage_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> std::result::Result<Option<TagDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<String>::deserialize(deserializer)?;
        value
            .map(|value| TagDateTime::parse(&value).map_err(serde::de::Error::custom))
            .transpose()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{EventHistoryQuery, HistoryQuery, MemoryStore, PurgeResult, State, Store};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    fn setup() -> (Runtime, Registries, EntityLifecycle) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        (runtime, registries, lifecycle)
    }

    fn create(id: &str) -> TagCreate {
        TagCreate {
            tag_id: Some(id.into()),
            ..TagCreate::default()
        }
    }

    #[test]
    fn datetime_validation_preserves_source_naive_and_offset_forms() {
        let naive = TagDateTime::parse("2024-2-9 3:04").unwrap();
        assert_eq!(naive.to_storage_string(), "2024-02-09T03:04:00");
        assert_eq!(naive.to_state_string(), "2024-02-09T03:04:00.000");

        let offset = TagDateTime::parse("2024-02-09T03:04:05.123456+0230").unwrap();
        assert_eq!(
            offset.to_storage_string(),
            "2024-02-09T03:04:05.123456+02:30"
        );
        assert_eq!(offset.to_state_string(), "2024-02-09T03:04:05.123+02:30");

        let utc = TagDateTime::parse("2024-02-09T03:04:05Z").unwrap();
        assert_eq!(utc.to_storage_string(), "2024-02-09T03:04:05+00:00");
        assert!(TagDateTime::parse("2024-02-30T03:04:05Z").is_err());
    }

    #[tokio::test]
    async fn scan_fires_public_event_with_context_before_creating_persisted_tag() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        let mut events = runtime.subscribe();
        let mut private = runtime.subscribe_signals();
        let context = Context {
            id: "scan-context".into(),
            parent_id: Some("parent".into()),
            user_id: Some("user".into()),
        };

        manager
            .scan("tag-1", Some("scanner-1"), context.clone())
            .unwrap();

        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, EVENT_TAG_SCANNED);
        assert_eq!(event.data["tag_id"], "tag-1");
        assert_eq!(event.data["name"], Value::Null);
        assert_eq!(event.data["device_id"], "scanner-1");
        assert_eq!(event.context, context);
        let config = manager.get("tag-1").unwrap().unwrap();
        assert_eq!(config.name.as_deref(), Some("Tag tag-1"));
        assert_eq!(config.device_id.as_deref(), Some("scanner-1"));
        assert!(config.last_scanned.is_some());
        let entity = registries
            .entities()
            .into_iter()
            .find(|entry| entry.unique_id == "tag-1")
            .unwrap();
        let state = runtime.get_state(&entity.entity_id).unwrap();
        assert!(state.state.ends_with("+00:00"));
        assert_eq!(state.attributes["tag_id"], "tag-1");
        assert_eq!(state.attributes["last_scanned_by_device_id"], "scanner-1");
        let signal = private.try_recv().unwrap();
        assert_eq!(signal.event_type, TAG_COLLECTION_UPDATED_SIGNAL);
        assert_eq!(signal.data["changes"][0]["change_type"], "added");
        drop(integration);
        while let Ok(event) = events.try_recv() {
            assert_ne!(event.event_type, TAG_COLLECTION_UPDATED_SIGNAL);
        }
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scan_without_device_preserves_existing_device_and_name_is_registry_backed() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        let created = manager
            .create(TagCreate {
                tag_id: Some("named-id".into()),
                name: Some("Kitchen tag".into()),
                device_id: Some("first-scanner".into()),
                ..TagCreate::default()
            })
            .unwrap();
        assert_eq!(created.name.as_deref(), Some("Kitchen tag"));
        manager
            .update(
                "named-id",
                TagUpdate {
                    name: Some("Renamed tag".into()),
                    ..TagUpdate::default()
                },
            )
            .unwrap();
        let mut events = runtime.subscribe();
        manager
            .scan("named-id", Some(""), Context::default())
            .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, EVENT_TAG_SCANNED);
        assert_eq!(event.data["name"], "Renamed tag");
        assert_eq!(event.data["device_id"], "");
        let updated = manager.get("named-id").unwrap().unwrap();
        assert_eq!(updated.name.as_deref(), Some("Renamed tag"));
        assert_eq!(updated.device_id.as_deref(), Some("first-scanner"));
        let entry = registries
            .entities()
            .into_iter()
            .find(|entry| entry.unique_id == "named-id")
            .unwrap();
        assert_eq!(entry.name.as_deref(), Some("Renamed tag"));
        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_minor_11_collection_migrates_names_and_duplicate_tag_id() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .set_data(
                STORAGE_KEY,
                &json!({
                    "key":"tag",
                    "version":1,
                    "minor_version":1,
                    "data":{"items":[{
                        "id":"legacy-tag",
                        "tag_id":"legacy-tag",
                        "name":"Kitchen tag",
                        "last_scanned":"2024-02-29T13:00:00+00:00"
                    }]}
                }),
            )
            .unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        assert_eq!(
            manager.list().unwrap()[0].name.as_deref(),
            Some("Kitchen tag")
        );
        let stored = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
        assert_eq!(stored["minor_version"], STORAGE_MINOR_VERSION);
        assert!(stored["items"][0].get("tag_id").is_none());
        assert!(stored["items"][0].get("name").is_none());
        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_drop_invalidates_weak_handles_and_releases_entities() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager.create(create("short-lived")).unwrap();
        integration.shutdown().unwrap();
        assert!(manager.list().is_err());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_and_subscribe_delivers_only_private_collection_deltas() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let manager = integration.manager();
        let (initial, mut updates) = manager.snapshot_and_subscribe().unwrap();
        assert!(initial.is_empty());
        let mut public_events = runtime.subscribe();
        manager.create(create("feed-tag")).unwrap();
        let delta = updates.try_recv().unwrap();
        assert_eq!(delta.event_type, TAG_COLLECTION_UPDATED_SIGNAL);
        assert_eq!(delta.data["changes"][0]["tag_id"], "feed-tag");
        while let Ok(event) = public_events.try_recv() {
            assert_ne!(event.event_type, TAG_COLLECTION_UPDATED_SIGNAL);
        }
        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collection_list_and_persistence_keep_source_insertion_order() {
        let (runtime, registries, lifecycle) = setup();
        runtime
            .set_data(
                STORAGE_KEY,
                &json!({
                    "key": DOMAIN,
                    "version": STORAGE_VERSION,
                    "minor_version": STORAGE_MINOR_VERSION,
                    "data": {"items": [
                        {"id":"z-tag-first"},
                        {"id":"a-tag-second"}
                    ]}
                }),
            )
            .unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager
            .scan("scanned-tag", None, Context::default())
            .unwrap();

        let listed = manager.list().unwrap();
        assert_eq!(
            listed.iter().map(|tag| tag.id.as_str()).collect::<Vec<_>>(),
            ["z-tag-first", "a-tag-second", "scanned-tag"]
        );
        let persisted = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
        assert_eq!(persisted["items"][0]["id"], "z-tag-first");
        assert_eq!(persisted["items"][1]["id"], "a-tag-second");
        assert_eq!(persisted["items"][2]["id"], "scanned-tag");

        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unrelated_collection_changes_do_not_recreate_removed_registry_entities() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager.create(create("removed")).unwrap();
        manager.create(create("other")).unwrap();
        let entity_id = registry_entry(&registries, "removed").unwrap().entity_id;
        registries.remove_entity(&entity_id).unwrap();
        manager.scan("removed", None, Context::default()).unwrap();
        assert!(runtime.get_state(&entity_id).is_none());
        manager.create(create("new")).unwrap();
        assert!(registry_entry(&registries, "removed").is_none());
        manager.delete("other").unwrap();
        assert!(registry_entry(&registries, "removed").is_none());
        assert!(runtime.get_state(&entity_id).is_none());
        assert!(manager.get("removed").unwrap().unwrap().name.is_none());
        manager.delete("removed").unwrap();
        manager.create(create("removed")).unwrap();
        assert!(registry_entry(&registries, "removed").is_some());
        integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_create_keeps_collection_and_emits_no_delta() {
        let (runtime, registries, lifecycle) = setup();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager.create(create("existing-tag")).unwrap();
        let mut signals = runtime.subscribe_signals();
        let error = manager.create(create("existing-tag")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Tag with ID existing-tag already exists.")
        );
        assert!(signals.try_recv().is_err());
        assert_eq!(manager.list().unwrap().len(), 1);
        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tag_collection_write_failure_does_not_retract_public_scan_event() {
        let store = Arc::new(FailTagMetadataStore::new());
        let runtime = Runtime::new(store.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let manager = integration.manager();
        let mut events = runtime.subscribe();
        store.fail_tag_metadata.store(true, Ordering::Release);

        assert!(
            manager
                .scan("failed-write", Some("scanner"), Context::default())
                .is_err()
        );

        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, EVENT_TAG_SCANNED);
        assert_eq!(event.data["tag_id"], "failed-write");
        assert!(manager.list().unwrap().is_empty());
        store.fail_tag_metadata.store(false, Ordering::Release);
        drop(integration);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_create_removes_only_its_new_registry_identity() {
        for fail_name in [true, false] {
            let store = Arc::new(FailTagMetadataStore::new());
            let runtime = Runtime::new(store.clone()).unwrap();
            let registries = Registries::new(runtime.clone()).unwrap();
            let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
            let integration =
                TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone())
                    .unwrap();
            let manager = integration.manager();
            let mut signals = runtime.subscribe_signals();
            if fail_name {
                store.fail_registry_write_at.store(2, Ordering::Release);
            } else {
                store.fail_state_once.store(true, Ordering::Release);
            }
            assert!(
                manager
                    .create(TagCreate {
                        name: Some("Failed name".into()),
                        ..create("failed")
                    })
                    .is_err()
            );
            assert!(manager.list().unwrap().is_empty());
            assert!(registry_entry(&registries, "failed").is_none());
            assert!(
                runtime
                    .states()
                    .iter()
                    .all(|state| !state.entity_id.starts_with("tag."))
            );
            assert_eq!(
                runtime.get_data(STORAGE_KEY).unwrap().unwrap()["items"],
                json!([])
            );
            assert!(signals.try_recv().is_err());
            manager.create(create("retry")).unwrap();
            integration.shutdown().unwrap();
            lifecycle.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_create_preserves_a_preexisting_registry_entry_and_raw_name() {
        let store = Arc::new(FailTagMetadataStore::new());
        let runtime = Runtime::new(store.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let original = registries
            .get_or_create_entity(hass_registry::EntityRegistration::new(
                "tag", "tag", "existing",
            ))
            .unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        store.fail_state_once.store(true, Ordering::Release);
        assert!(
            integration
                .manager()
                .create(TagCreate {
                    name: Some("Failed name".into()),
                    ..create("existing")
                })
                .is_err()
        );
        let restored = registry_entry(&registries, "existing").unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.name, original.name);
        assert!(integration.manager().list().unwrap().is_empty());
        integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_create_restores_a_prior_tombstone_and_preserves_occupied_state() {
        let store = Arc::new(FailTagMetadataStore::new());
        let runtime = Runtime::new(store.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager
            .create(TagCreate {
                name: Some("Original name".into()),
                ..create("reuse")
            })
            .unwrap();
        manager.delete("reuse").unwrap();
        let tombstone = registries
            .removed_provider_entity(DOMAIN, DOMAIN, "reuse")
            .unwrap();
        let foreign = runtime
            .set_state(
                &tombstone.entity_id,
                "foreign",
                serde_json::Map::from_iter([("owner".into(), json!("foreign"))]),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        let metadata = runtime.get_data(STORAGE_KEY).unwrap();
        let mut signals = runtime.subscribe_signals();
        store.fail_state_once.store(true, Ordering::Release);
        assert!(
            manager
                .create(TagCreate {
                    name: Some("Failed replacement name".into()),
                    ..create("reuse")
                })
                .is_err()
        );
        assert!(registry_entry(&registries, "reuse").is_none());
        assert_eq!(
            registries
                .removed_provider_entity(DOMAIN, DOMAIN, "reuse")
                .as_ref(),
            Some(&tombstone)
        );
        assert_eq!(
            runtime.get_state(&tombstone.entity_id),
            Some(foreign.clone())
        );
        assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), metadata);
        assert!(manager.list().unwrap().is_empty());
        assert!(signals.try_recv().is_err());
        manager.create(create("reuse")).unwrap();
        assert_eq!(runtime.get_state(&tombstone.entity_id), Some(foreign));
        integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_update_restores_raw_name_collection_and_state() {
        for fail_metadata in [true, false] {
            let store = Arc::new(FailTagMetadataStore::new());
            let runtime = Runtime::new(store.clone()).unwrap();
            let registries = Registries::new(runtime.clone()).unwrap();
            let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
            let integration =
                TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone())
                    .unwrap();
            let manager = integration.manager();
            manager.create(create("existing")).unwrap();
            let original_entry = registry_entry(&registries, "existing").unwrap();
            let original_state = runtime.get_state(&original_entry.entity_id).unwrap();
            let metadata = runtime.get_data(STORAGE_KEY).unwrap();
            let mut signals = runtime.subscribe_signals();
            store
                .fail_tag_metadata
                .store(fail_metadata, Ordering::Release);
            store
                .fail_state_once
                .store(!fail_metadata, Ordering::Release);
            assert!(
                manager
                    .update(
                        "existing",
                        TagUpdate {
                            name: Some("Changed name".into()),
                            description: Some("changed".into()),
                            ..TagUpdate::default()
                        }
                    )
                    .is_err()
            );
            assert_eq!(
                registry_entry(&registries, "existing").unwrap().name,
                original_entry.name
            );
            assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), metadata);
            let restored = runtime.get_state(&original_entry.entity_id).unwrap();
            assert_eq!(restored.state, original_state.state);
            assert_eq!(restored.attributes, original_state.attributes);
            assert!(
                manager
                    .get("existing")
                    .unwrap()
                    .unwrap()
                    .description
                    .is_none()
            );
            assert!(signals.try_recv().is_err());
            store.fail_tag_metadata.store(false, Ordering::Release);
            integration.shutdown().unwrap();
            lifecycle.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_delete_restores_the_same_registry_identity_and_live_state() {
        let store = Arc::new(FailTagMetadataStore::new());
        let runtime = Runtime::new(store.clone()).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let manager = integration.manager();
        manager
            .create(TagCreate {
                name: Some("Keep name".into()),
                ..create("existing")
            })
            .unwrap();
        let original = registry_entry(&registries, "existing").unwrap();
        let original_state = runtime.get_state(&original.entity_id).unwrap();
        let metadata = runtime.get_data(STORAGE_KEY).unwrap();
        let mut signals = runtime.subscribe_signals();
        store.fail_remove_once.store(true, Ordering::Release);
        assert!(manager.delete("existing").is_err());
        let restored = registry_entry(&registries, "existing").unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.entity_id, original.entity_id);
        assert_eq!(restored.name, original.name);
        let state = runtime.get_state(&original.entity_id).unwrap();
        assert_eq!(state.state, original_state.state);
        assert_eq!(state.attributes, original_state.attributes);
        assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), metadata);
        assert_eq!(manager.list().unwrap().len(), 1);
        assert!(signals.try_recv().is_err());
        manager.delete("existing").unwrap();
        assert!(registry_entry(&registries, "existing").is_none());
        assert!(runtime.get_state(&original.entity_id).is_none());
        integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    struct FailTagMetadataStore {
        inner: MemoryStore,
        fail_tag_metadata: AtomicBool,
        fail_state_once: AtomicBool,
        fail_remove_once: AtomicBool,
        fail_registry_write_at: std::sync::atomic::AtomicUsize,
    }

    impl FailTagMetadataStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::default(),
                fail_tag_metadata: AtomicBool::new(false),
                fail_state_once: AtomicBool::new(false),
                fail_remove_once: AtomicBool::new(false),
                fail_registry_write_at: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Store for FailTagMetadataStore {
        fn load_states(&self) -> Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
            if self.fail_state_once.swap(false, Ordering::AcqRel) {
                return Err(Error::Storage("injected state publication failure".into()));
            }
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
            if self.fail_remove_once.swap(false, Ordering::AcqRel) {
                return Err(Error::Storage("injected state removal failure".into()));
            }
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> Result<()> {
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            if key == "core.registries"
                && self.fail_registry_write_at.try_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |value| value.checked_sub(1),
                ) == Ok(1)
            {
                return Err(Error::Storage("injected registry write failure".into()));
            }
            if key == STORAGE_KEY && self.fail_tag_metadata.load(Ordering::Acquire) {
                return Err(Error::Storage("injected tag metadata write failure".into()));
            }
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

        fn history_query(&self, query: &HistoryQuery) -> Result<Vec<hass_core::HistorySeries>> {
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
}
