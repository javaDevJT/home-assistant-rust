//! Shared native entity lifecycle for Home Assistant Rust integrations.
//!
//! One [`EntityLifecycle`] owns the shared runtime/registry view and the
//! registry-event worker. Providers register a batch of stable identities,
//! retain weak-backed [`EntityHandle`] leases, and publish immutable snapshots
//! without caching entity IDs. Registry renames, disables and removals are
//! observed through the same handles used by state publication and services.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hass_core::{Context, Error, Event, Runtime, State, StateChange, StateRecordingPolicy};
use hass_registry::{
    EntityEntry, EntityRegistration, ProviderEntityInfo, ProviderEntityRegistration, Registries,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// Result type shared with the native runtime.
pub type Result<T> = hass_core::Result<T>;

const GENERATED_ID_KEY: &str = "hass-entity.generated-ids.v1";
const REGISTRY_RECEIPTS_KEY: &str = "hass-entity.registry-receipts.v1";
const RESTORE_DATA_KEY: &str = "hass-entity.restore-state.v1";
const MAX_RESTORE_AGE_SECONDS: u64 = 10 * 24 * 60 * 60;

/// Configuration for one entity supplied by a provider.
///
/// `provider_id` and `provider_key` form a namespaced ownership receipt. The
/// pair must remain stable across reloads. `prior_entity_id` is intended for a
/// trusted, provider-owned pre-lifecycle receipt during migration; it is used
/// only for entities without a registry `unique_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct EntitySpec {
    /// Home Assistant entity domain, such as `sensor`.
    pub domain: String,
    /// Integration/provider name used in registry identity.
    pub platform: String,
    /// Optional Home Assistant registry unique ID.
    pub unique_id: Option<String>,
    /// Config entry that owns this provider entity, if any.
    pub config_entry_id: Option<String>,
    /// Config subentry that owns this provider entity, if any.
    pub config_subentry_id: Option<String>,
    /// Device registry entry associated with this provider entity, if any.
    pub device_id: Option<String>,
    /// Area registry entry associated with this provider entity, if any.
    pub area_id: Option<String>,
    /// Namespace for provider-owned receipts.
    pub provider_id: String,
    /// Stable ID within `provider_id`.
    pub provider_key: String,
    /// Suggested object ID for a newly allocated entity ID.
    pub suggested_object_id: Option<String>,
    /// Previously allocated ID owned by this provider, if migrating receipts.
    pub prior_entity_id: Option<String>,
    /// Current provider metadata, separate from user registry overrides.
    pub info: ProviderEntityInfo,
    /// State attributes retained live but excluded from recorded history.
    pub recording_policy: StateRecordingPolicy,
    /// Initial registry availability; existing user decisions are preserved.
    pub enabled_by_default: bool,
}

impl EntitySpec {
    /// Create a provider-scoped entity specification.
    #[must_use]
    pub fn new(
        domain: impl Into<String>,
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            domain: domain.into(),
            platform: platform.into(),
            unique_id: None,
            config_entry_id: None,
            config_subentry_id: None,
            device_id: None,
            area_id: None,
            provider_id: provider_id.into(),
            provider_key: provider_key.into(),
            suggested_object_id: None,
            prior_entity_id: None,
            info: ProviderEntityInfo::default(),
            recording_policy: StateRecordingPolicy::default(),
            enabled_by_default: true,
        }
    }

    /// Set the registry unique ID.
    #[must_use]
    pub fn with_unique_id(mut self, unique_id: impl Into<String>) -> Self {
        self.unique_id = Some(unique_id.into());
        self
    }

    /// Associate this entity with a config entry.
    #[must_use]
    pub fn with_config_entry_id(mut self, config_entry_id: impl Into<String>) -> Self {
        self.config_entry_id = Some(config_entry_id.into());
        self
    }

    /// Associate this entity with a config subentry.
    #[must_use]
    pub fn with_config_subentry_id(mut self, config_subentry_id: impl Into<String>) -> Self {
        self.config_subentry_id = Some(config_subentry_id.into());
        self
    }

    /// Associate this entity with a device registry entry.
    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Associate this entity with an area registry entry.
    #[must_use]
    pub fn with_area_id(mut self, area_id: impl Into<String>) -> Self {
        self.area_id = Some(area_id.into());
        self
    }

    /// Set a generated entity ID suggestion.
    #[must_use]
    pub fn with_suggested_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.suggested_object_id = Some(object_id.into());
        self
    }

    /// Set a previously persisted entity ID receipt for migration.
    #[must_use]
    pub fn with_prior_entity_id(mut self, entity_id: impl Into<String>) -> Self {
        self.prior_entity_id = Some(entity_id.into());
        self
    }

    /// Set provider-owned original metadata.
    #[must_use]
    pub fn with_info(mut self, info: ProviderEntityInfo) -> Self {
        self.info = info;
        self
    }

    /// Disable a newly created registry identity until a user enables it.
    /// Existing and restored registry entries keep their current setting.
    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.enabled_by_default = enabled;
        self
    }

    /// Omit these attributes from recorded history while keeping them in live
    /// state and state-change events.
    #[must_use]
    pub fn with_unrecorded_attributes(
        mut self,
        attributes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.recording_policy = self.recording_policy.with_unrecorded_attributes(attributes);
        self
    }
}

/// Current provider state for one entity.
///
/// An absent `state` means `unknown` while `available` is true. When
/// `available` is false, the lifecycle publishes `unavailable` and omits
/// provider state attributes. The snapshot context is preserved in the core
/// state and its event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EntitySnapshot {
    /// Current provider state. `None` maps to Home Assistant `unknown`.
    pub state: Option<String>,
    /// Provider availability.
    pub available: bool,
    /// Current provider attributes, including dynamic attributes such as
    /// `friendly_name` or `icon`.
    pub attributes: Map<String, Value>,
    /// Causal context for this state publication.
    pub context: Context,
}

impl Default for EntitySnapshot {
    fn default() -> Self {
        Self {
            state: None,
            available: true,
            attributes: Map::new(),
            context: Context::default(),
        }
    }
}

impl EntitySnapshot {
    /// Create an available state snapshot with no extra attributes.
    #[must_use]
    pub fn new(state: impl Into<String>) -> Self {
        Self {
            state: Some(state.into()),
            ..Self::default()
        }
    }

    /// Create an available snapshot whose state is `unknown`.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Create an unavailable snapshot.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::default()
        }
    }
}

/// A provider state publication with attributes that remain live while the
/// entity is unavailable.
///
/// Ordinary [`EntitySnapshot::attributes`] are published only while the
/// snapshot is available. Use [`Self::with_live_attribute`] for attributes
/// that must continue to reflect provider metadata during unavailability.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityPublication {
    /// The provider state and ordinary availability-scoped attributes.
    pub snapshot: EntitySnapshot,
    live_attributes: BTreeMap<String, LiveAttributeUpdate>,
}

impl From<EntitySnapshot> for EntityPublication {
    fn from(snapshot: EntitySnapshot) -> Self {
        Self {
            snapshot,
            live_attributes: BTreeMap::new(),
        }
    }
}

impl EntityPublication {
    /// Add or remove one live attribute.
    ///
    /// `Some(Value::Null)` publishes an explicit JSON `null`. `None` removes
    /// the attribute and suppresses its provider fallback until the next
    /// publication changes it. Registry user overrides still take priority.
    #[must_use]
    pub fn with_live_attribute(mut self, name: impl Into<String>, value: Option<Value>) -> Self {
        self.live_attributes.insert(
            name.into(),
            value.map_or(LiveAttributeUpdate::Remove, LiveAttributeUpdate::Set),
        );
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum LiveAttributeUpdate {
    Set(Value),
    Remove,
}

/// Current lifecycle status for a provider entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityStatus {
    /// Registered and enabled.
    Active { entity_id: String },
    /// Present in the registry but disabled by a user or configuration.
    Disabled { entity_id: String },
    /// Removed from the registry. The ID is the last observed ID.
    Removed { entity_id: String },
    /// Provider lease was released. The ID is the last observed ID.
    Released { entity_id: String },
    /// The shared lifecycle manager has shut down. The ID is the last
    /// observed ID.
    Shutdown { entity_id: String },
}

impl EntityStatus {
    /// Return the last observed entity ID, if any.
    #[must_use]
    pub fn entity_id(&self) -> &str {
        match self {
            Self::Active { entity_id }
            | Self::Disabled { entity_id }
            | Self::Removed { entity_id }
            | Self::Released { entity_id }
            | Self::Shutdown { entity_id } => entity_id,
        }
    }

    /// Whether this entity currently accepts state publications and service
    /// targeting.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

/// Why a handle's lifecycle watch value changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityLifecycleReason {
    /// The provider lease was registered.
    Registered,
    /// The entity's registry ID changed.
    Renamed,
    /// The registry entry became disabled.
    Disabled,
    /// A disabled registry entry became enabled.
    Enabled,
    /// The entity registry entry was removed.
    Removed,
    /// Provider-owned metadata or user overrides changed.
    RegistryUpdated,
    /// The provider released its lease.
    Released,
    /// The shared lifecycle manager shut down.
    Shutdown,
}

/// A coalescing lifecycle notification exposed by [`EntityHandle::changes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityLifecycleChange {
    /// Latest state of this handle.
    pub status: EntityStatus,
    /// Most recent lifecycle transition.
    pub reason: EntityLifecycleReason,
    /// Previous ID when a rename was observed.
    pub previous_entity_id: Option<String>,
}

/// One shared runtime/registry lifecycle manager.
///
/// Clone this value to share a single registry listener and ownership table
/// between integration providers. Call [`Self::shutdown`] when the application
/// is stopping; provider unloads should release their individual handles while
/// the manager remains open.
#[derive(Clone)]
pub struct EntityLifecycle {
    inner: Arc<EntityLifecycleInner>,
}

impl fmt::Debug for EntityLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntityLifecycle")
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .field("entities", &lock(&self.inner.records).len())
            .finish()
    }
}

impl EntityLifecycle {
    /// Start one shared lifecycle manager using the application's shared
    /// runtime and registry instances. Must be called from a Tokio runtime.
    pub fn new(runtime: Runtime, registries: Registries) -> Result<Self> {
        let tokio = tokio::runtime::Handle::try_current().map_err(|error| {
            Error::Service(format!(
                "entity lifecycle requires a Tokio runtime: {error}"
            ))
        })?;
        let generated_ids = load_map(&runtime, GENERATED_ID_KEY, "generated entity IDs")?;
        let registry_receipts = load_registry_receipts(&runtime)?;
        let restore_data = load_restore_data(&runtime)?;
        let (_states, events) = runtime.subscribe_with_states();
        let (cancel, cancel_rx) = watch::channel(false);
        let inner = Arc::new(EntityLifecycleInner {
            runtime,
            registries,
            records: Mutex::new(BTreeMap::new()),
            generated_ids: Mutex::new(generated_ids),
            registry_receipts: Mutex::new(registry_receipts),
            restore_data: Mutex::new(restore_data),
            pending_releases: Mutex::new(BTreeMap::new()),
            next_lease_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            cancel,
            task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let task = tokio.spawn(registry_event_loop(weak, events, cancel_rx));
        *lock(&inner.task) = Some(task);
        Ok(Self { inner })
    }

    /// Register a preflighted batch and return weak-backed leases in the same
    /// order as the input specifications.
    ///
    /// Registry identities are created/refreshed atomically. Entities without
    /// a unique ID use persisted `(provider_id, provider_key)` ID receipts.
    pub fn register_batch(&self, specs: Vec<EntitySpec>) -> Result<Vec<EntityHandle>> {
        self.inner.register_batch(specs)
    }

    /// Reconcile every entity owned by one provider namespace with the supplied
    /// active configuration. An empty batch releases that provider's old leases,
    /// makes its registered entities unavailable, and removes only generated
    /// states covered by persisted provider receipts.
    ///
    /// `provider_id` is an ownership namespace, independent of each entity's
    /// registry `platform`. Providers with separate config entries should use
    /// distinct IDs; registered identities are durably scoped to that namespace.
    pub fn register_provider_batch(
        &self,
        provider_id: &str,
        specs: Vec<EntitySpec>,
    ) -> Result<Vec<EntityHandle>> {
        self.inner.reconcile_provider(provider_id, specs)
    }

    /// Stop and join the registry worker. The application-level shutdown keeps
    /// live states intact for the next runtime restore; provider unload should
    /// use [`EntityHandle::release`] while the manager is still open.
    pub async fn shutdown(&self) -> Result<()> {
        let first_shutdown = !self.inner.closed.swap(true, Ordering::AcqRel);
        if first_shutdown {
            self.inner.cancel.send_replace(true);
        }
        let mut first_error = None;
        {
            let mut records = lock(&self.inner.records);
            for record in records.values_mut() {
                if let Some(snapshot) = EntityLifecycleInner::snapshot_for_record(record)
                    && let Err(error) = self.inner.save_restore_snapshot(record, snapshot)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
                if first_shutdown {
                    self.inner.emit_change(
                        record,
                        EntityStatus::Shutdown {
                            entity_id: record.entity_id.clone(),
                        },
                        EntityLifecycleReason::Shutdown,
                        None,
                    );
                }
            }
        }
        let task = lock(&self.inner.task).take();
        if let Some(task) = task {
            task.await.map_err(|error| {
                Error::Service(format!("joining entity lifecycle worker failed: {error}"))
            })?;
        }
        let drop_errors = self.inner.retry_pending_releases(true);
        match (first_error, drop_errors.len()) {
            (Some(error), 0) => Err(error),
            (Some(error), _) => Err(Error::Service(format!(
                "shutdown persistence failed: {error}; dropped entity lease cleanup failed: {}",
                drop_errors.join("; ")
            ))),
            (None, 0) => Ok(()),
            (None, 1) => Err(Error::Service(format!(
                "dropped entity lease cleanup failed: {}",
                drop_errors[0]
            ))),
            (None, _) => Err(Error::Service(format!(
                "dropped entity lease cleanup failed: {}",
                drop_errors.join("; ")
            ))),
        }
    }
}

/// Provider lease for one entity.
///
/// Handles are cloneable and contain only a weak reference to the shared
/// manager. The last handle clone releases the provider lease. `entity_id` is
/// resolved from the current registry entry each time, so callers never need a
/// second registry-ID cache.
#[derive(Clone)]
pub struct EntityHandle {
    lease: Arc<EntityLease>,
}

impl fmt::Debug for EntityHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntityHandle")
            .field("provider_id", &self.lease.provider_id)
            .field("provider_key", &self.lease.provider_key)
            .field("registry_id", &self.lease.registry_id)
            .field("status", &self.status())
            .finish()
    }
}

impl EntityHandle {
    /// Return the current lifecycle status, resolving registry changes
    /// synchronously when possible.
    #[must_use]
    pub fn status(&self) -> EntityStatus {
        self.lease
            .manager
            .upgrade()
            .and_then(|inner| inner.current_status(&self.lease.receipt_key, self.lease.id))
            .unwrap_or_else(|| self.lease.changes.borrow().status.clone())
    }

    /// Return the current entity ID only while the entity is enabled.
    #[must_use]
    pub fn current_entity_id(&self) -> Option<String> {
        match self.status() {
            EntityStatus::Active { entity_id } => Some(entity_id),
            EntityStatus::Disabled { .. }
            | EntityStatus::Removed { .. }
            | EntityStatus::Released { .. }
            | EntityStatus::Shutdown { .. } => None,
        }
    }

    /// Alias for [`Self::current_entity_id`].
    #[must_use]
    pub fn entity_id(&self) -> Option<String> {
        self.current_entity_id()
    }

    /// Stable entity registry entry ID, if this provider entity has a unique
    /// ID.
    #[must_use]
    pub fn registry_id(&self) -> Option<&str> {
        self.lease.registry_id.as_deref()
    }

    /// Subscribe to coalesced lifecycle changes for this entity.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<EntityLifecycleChange> {
        self.lease.changes.clone()
    }

    /// Publish a provider snapshot using the handle's current registry identity.
    ///
    /// Returns `Ok(None)` when a registry entry is disabled, removed, or the
    /// lease has been released. A removed entry is never recreated by publish.
    pub fn publish(&self, snapshot: EntitySnapshot) -> Result<Option<StateChange>> {
        self.publish_publication(EntityPublication::from(snapshot))
    }

    /// Publish provider state with live attributes that remain visible while
    /// the entity is unavailable.
    ///
    /// Returns `Ok(None)` when a registry entry is disabled, removed, or the
    /// lease has been released. A removed entry is never recreated by publish.
    pub fn publish_publication(
        &self,
        publication: EntityPublication,
    ) -> Result<Option<StateChange>> {
        let Some(inner) = self.lease.manager.upgrade() else {
            return Err(Error::Service("entity lifecycle manager is closed".into()));
        };
        inner.publish(&self.lease.receipt_key, self.lease.id, publication)
    }

    /// Release this provider lease. Registered enabled entities become
    /// unavailable with `restored: true`; unregistered generated IDs have
    /// their owned runtime state removed. The last published snapshot is saved
    /// under stable provider or registry identity for reentry.
    pub fn release(&self) -> Result<()> {
        let Some(inner) = self.lease.manager.upgrade() else {
            self.lease.released.store(true, Ordering::Release);
            return Ok(());
        };
        if self.lease.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        match inner.release(&self.lease.receipt_key, self.lease.id) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.lease.released.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

struct EntityLease {
    manager: Weak<EntityLifecycleInner>,
    receipt_key: String,
    provider_id: String,
    provider_key: String,
    registry_id: Option<String>,
    id: u64,
    changes: watch::Receiver<EntityLifecycleChange>,
    released: AtomicBool,
}

impl Drop for EntityLease {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(inner) = self.manager.upgrade() else {
            return;
        };
        if !inner.closed.load(Ordering::Acquire)
            && let Err(error) = inner.release(&self.receipt_key, self.id)
        {
            inner.abandon_failed_release(&self.receipt_key, self.id, error);
        }
    }
}

struct EntityRecord {
    identity_key: String,
    provider_id: String,
    registry_id: Option<String>,
    restore_key: String,
    entity_id: String,
    info: ProviderEntityInfo,
    recording_policy: StateRecordingPolicy,
    latest: Option<EntitySnapshot>,
    live_attributes: BTreeMap<String, LiveAttributeUpdate>,
    owned_state: Option<State>,
    status: EntityStatus,
    lease_id: u64,
    changes: watch::Sender<EntityLifecycleChange>,
}

struct DetachedProviderRecords {
    records: BTreeMap<String, EntityRecord>,
    owned_states: BTreeMap<String, State>,
}

struct PendingRelease {
    receipt_key: String,
    record: EntityRecord,
    retry_status: EntityStatus,
    attempts: u32,
    retry_at: tokio::time::Instant,
    last_error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RestoredSnapshot {
    snapshot: EntitySnapshot,
    saved_at: u64,
    #[serde(default)]
    owned_state: Option<State>,
    #[serde(default)]
    live_attributes: Option<BTreeMap<String, LiveAttributeUpdate>>,
}

struct SnapshotWriteOptions<'a> {
    live_attributes: &'a BTreeMap<String, LiveAttributeUpdate>,
    restored: bool,
    expected: Option<&'a State>,
}

struct EntityLifecycleInner {
    runtime: Runtime,
    registries: Registries,
    records: Mutex<BTreeMap<String, EntityRecord>>,
    generated_ids: Mutex<BTreeMap<String, String>>,
    registry_receipts: Mutex<BTreeMap<String, BTreeSet<String>>>,
    restore_data: Mutex<BTreeMap<String, RestoredSnapshot>>,
    pending_releases: Mutex<BTreeMap<(String, u64), PendingRelease>>,
    next_lease_id: AtomicU64,
    closed: AtomicBool,
    cancel: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl EntityLifecycleInner {
    fn reconcile_provider(
        self: &Arc<Self>,
        provider_id: &str,
        specs: Vec<EntitySpec>,
    ) -> Result<Vec<EntityHandle>> {
        if provider_id.trim().is_empty() {
            return Err(Error::InvalidInput("provider ID cannot be empty".into()));
        }
        preflight_specs(&specs)?;
        if specs.iter().any(|spec| spec.provider_id != provider_id) {
            return Err(Error::InvalidInput(format!(
                "all entity specs must use provider ID {provider_id}"
            )));
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Service("entity lifecycle manager is closed".into()));
        }

        let active_receipts = specs.iter().map(receipt_key).collect::<HashSet<_>>();
        let active_identities = specs.iter().map(identity_key).collect::<HashSet<_>>();
        let active_registry_identities = specs
            .iter()
            .filter(|spec| spec.unique_id.is_some())
            .map(identity_key)
            .collect::<BTreeSet<_>>();
        let previous_registry_identities =
            self.begin_provider_registry_reconcile(provider_id, &active_registry_identities)?;
        let detached = self.detach_provider_records(provider_id);
        let (registration, registry_committed) = self.register_batch_with_commit(specs);
        let handles = match registration {
            Ok(handles) => handles,
            Err(error) => {
                if registry_committed {
                    self.release_detached_provider_records(detached);
                    return Err(error);
                }
                self.restore_detached_provider_records(detached);
                if !registry_committed
                    && let Err(rollback_error) = self
                        .set_provider_registry_receipt(provider_id, &previous_registry_identities)
                {
                    return Err(Error::Storage(format!(
                        "provider registration failed: {error}; restoring registry ownership receipt failed: {rollback_error}"
                    )));
                }
                return Err(error);
            }
        };
        let detached_owned_states = self.release_detached_provider_records(detached);
        let active_ids = handles
            .iter()
            .map(|handle| handle.status().entity_id().to_owned())
            .collect::<HashSet<_>>();

        self.cleanup_stale_registry_entities(
            provider_id,
            &previous_registry_identities,
            &active_identities,
            &detached_owned_states,
        )?;
        self.cleanup_stale_generated_receipts(
            provider_id,
            &active_receipts,
            &active_ids,
            &detached_owned_states,
        )?;
        self.set_provider_registry_receipt(provider_id, &active_registry_identities)?;
        Ok(handles)
    }

    fn begin_provider_registry_reconcile(
        &self,
        provider_id: &str,
        active_identities: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        let mut receipts = lock(&self.registry_receipts);
        let previous = receipts.get(provider_id).cloned().unwrap_or_default();
        let pending = previous.union(active_identities).cloned().collect();
        if pending != previous {
            self.write_provider_registry_receipt_locked(&mut receipts, provider_id, &pending)?;
        }
        Ok(previous)
    }

    fn set_provider_registry_receipt(
        &self,
        provider_id: &str,
        identities: &BTreeSet<String>,
    ) -> Result<()> {
        let mut receipts = lock(&self.registry_receipts);
        self.write_provider_registry_receipt_locked(&mut receipts, provider_id, identities)
    }

    fn write_provider_registry_receipt_locked(
        &self,
        receipts: &mut BTreeMap<String, BTreeSet<String>>,
        provider_id: &str,
        identities: &BTreeSet<String>,
    ) -> Result<()> {
        let previous = receipts.clone();
        if identities.is_empty() {
            receipts.remove(provider_id);
        } else {
            receipts.insert(provider_id.to_owned(), identities.clone());
        }
        if *receipts == previous {
            return Ok(());
        }
        let serialized = serde_json::to_value(&*receipts).map_err(|error| {
            Error::Storage(format!("serialize entity registry receipts: {error}"))
        })?;
        if let Err(error) = self.runtime.set_data(REGISTRY_RECEIPTS_KEY, &serialized) {
            *receipts = previous;
            return Err(error);
        }
        Ok(())
    }

    fn detach_provider_records(&self, provider_id: &str) -> DetachedProviderRecords {
        let mut records = lock(&self.records);
        let mut detached = BTreeMap::new();
        let mut owned_states = BTreeMap::new();
        let keys = records
            .iter()
            .filter(|(_, record)| record.provider_id == provider_id)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(record) = records.remove(&key) {
                if let Some(state) = record.owned_state.clone() {
                    owned_states.insert(record.identity_key.clone(), state);
                }
                detached.insert(key, record);
            }
        }
        DetachedProviderRecords {
            records: detached,
            owned_states,
        }
    }

    fn restore_detached_provider_records(&self, detached: DetachedProviderRecords) {
        let mut records = lock(&self.records);
        for (key, mut record) in detached.records {
            if let std::collections::btree_map::Entry::Vacant(entry) = records.entry(key) {
                entry.insert(record);
            } else {
                let id = record.entity_id.clone();
                self.emit_change(
                    &mut record,
                    EntityStatus::Released { entity_id: id },
                    EntityLifecycleReason::Released,
                    None,
                );
            }
        }
    }

    fn release_detached_provider_records(
        &self,
        mut detached: DetachedProviderRecords,
    ) -> BTreeMap<String, State> {
        for record in detached.records.values_mut() {
            let id = record.entity_id.clone();
            self.emit_change(
                record,
                EntityStatus::Released { entity_id: id },
                EntityLifecycleReason::Released,
                None,
            );
        }
        detached.owned_states
    }

    fn cleanup_stale_registry_entities(
        &self,
        provider_id: &str,
        owned_registry_identities: &BTreeSet<String>,
        active_identities: &HashSet<String>,
        detached_owned_states: &BTreeMap<String, State>,
    ) -> Result<()> {
        let stale = self
            .registries
            .entities()
            .into_iter()
            .filter(|entry| owned_registry_identities.contains(&registry_identity_key(entry)))
            .filter(|entry| !active_identities.contains(&registry_identity_key(entry)))
            .collect::<Vec<_>>();

        for entry in stale {
            let Some(state) = self.runtime.get_state(&entry.entity_id) else {
                continue;
            };
            let identity = registry_identity_key(&entry);
            let expected = detached_owned_states
                .get(&identity)
                .cloned()
                .or_else(|| self.restored_owned_state(&entry.id));
            if expected.as_ref() != Some(&state) {
                continue;
            }
            let info = provider_info_from_entry(&entry);
            let key = identity;
            let (changes, _) = watch::channel(EntityLifecycleChange {
                status: EntityStatus::Active {
                    entity_id: entry.entity_id.clone(),
                },
                reason: EntityLifecycleReason::Registered,
                previous_entity_id: None,
            });
            let mut record = EntityRecord {
                identity_key: key,
                provider_id: provider_id.to_owned(),
                registry_id: Some(entry.id.clone()),
                restore_key: entry.id.clone(),
                entity_id: entry.entity_id.clone(),
                info,
                recording_policy: StateRecordingPolicy::default(),
                latest: Some(snapshot_from_state(&state)),
                live_attributes: self.restored_live_attributes(&entry.id),
                owned_state: Some(state.clone()),
                status: if entry.disabled_by.is_some() {
                    EntityStatus::Disabled {
                        entity_id: entry.entity_id.clone(),
                    }
                } else {
                    EntityStatus::Active {
                        entity_id: entry.entity_id.clone(),
                    }
                },
                lease_id: 0,
                changes,
            };
            if entry.disabled_by.is_some() {
                if state.attributes.get("restored") != Some(&Value::Bool(true)) {
                    self.save_restore_snapshot(&record, snapshot_from_state(&state))?;
                }
                self.clear_owned_state(&mut record)?;
                continue;
            }
            if state.state == "unavailable"
                && state.attributes.get("restored") == Some(&Value::Bool(true))
            {
                continue;
            }
            let snapshot = snapshot_from_state(&state);
            self.save_restore_snapshot(&record, snapshot.clone())?;
            record.latest = Some(snapshot.clone());
            let unavailable = EntitySnapshot {
                state: None,
                available: false,
                attributes: Map::new(),
                context: snapshot.context,
            };
            let attributes = self.compose_attributes(
                &unavailable,
                Some(&entry),
                &record.info,
                &record.live_attributes,
                true,
            );
            let Some(state_change) = self.runtime.set_state_if_unchanged_with_recording_policy(
                &entry.entity_id,
                Some(&state),
                "unavailable",
                attributes,
                unavailable.context,
                false,
                &record.recording_policy,
            )?
            else {
                continue;
            };
            record.owned_state = Some(state_change.state);
        }
        Ok(())
    }

    fn cleanup_stale_generated_receipts(
        &self,
        provider_id: &str,
        active_receipts: &HashSet<String>,
        active_ids: &HashSet<String>,
        detached_owned_states: &BTreeMap<String, State>,
    ) -> Result<()> {
        let reserved_ids = self.registries.provider_reserved_entity_ids();
        let mut generated_ids = lock(&self.generated_ids);
        let stale = generated_ids
            .iter()
            .filter_map(|(receipt, entity_id)| {
                let (_, _, owner, _) =
                    serde_json::from_str::<(String, String, String, String)>(receipt).ok()?;
                (owner == provider_id && !active_receipts.contains(receipt))
                    .then(|| (receipt.clone(), entity_id.clone()))
            })
            .collect::<Vec<_>>();

        for (receipt, entity_id) in &stale {
            if active_ids.contains(entity_id) || reserved_ids.contains(entity_id) {
                continue;
            }
            if let Some(state) = self.runtime.get_state(entity_id) {
                let expected = detached_owned_states
                    .get(receipt)
                    .cloned()
                    .or_else(|| self.restored_owned_state(receipt));
                match expected {
                    Some(expected) if expected.entity_id == *entity_id => {
                        self.runtime
                            .remove_state_if_unchanged(&expected, expected.context.clone())?;
                    }
                    Some(_) => {}
                    None => {
                        self.runtime
                            .remove_state_if_unchanged(&state, state.context.clone())?;
                    }
                }
            }
            self.remove_restore_snapshot(receipt)?;
        }
        if !stale.is_empty() {
            let previous = generated_ids.clone();
            for (receipt, _) in stale {
                generated_ids.remove(&receipt);
            }
            let serialized = serde_json::to_value(&*generated_ids)
                .map_err(|error| Error::Storage(format!("serialize entity IDs: {error}")))?;
            if let Err(error) = self.runtime.set_data(GENERATED_ID_KEY, &serialized) {
                *generated_ids = previous;
                return Err(error);
            }
        }
        Ok(())
    }

    fn remove_restore_snapshot(&self, restore_key: &str) -> Result<()> {
        let mut restore_data = lock(&self.restore_data);
        let Some(previous) = restore_data.remove(restore_key) else {
            return Ok(());
        };
        if let Err(error) = self.persist_restore_data(&restore_data) {
            restore_data.insert(restore_key.to_owned(), previous);
            return Err(error);
        }
        Ok(())
    }

    fn register_batch(self: &Arc<Self>, specs: Vec<EntitySpec>) -> Result<Vec<EntityHandle>> {
        self.register_batch_with_commit(specs).0
    }

    fn register_batch_with_commit(
        self: &Arc<Self>,
        specs: Vec<EntitySpec>,
    ) -> (Result<Vec<EntityHandle>>, bool) {
        let mut registry_committed = false;
        let result = (|| {
            if self.closed.load(Ordering::Acquire) {
                return Err(Error::Service("entity lifecycle manager is closed".into()));
            }
            if specs.is_empty() {
                return Ok(Vec::new());
            }

            preflight_specs(&specs)?;
            let mut records = lock(&self.records);
            let live = records
                .values()
                .filter(|record| {
                    !matches!(
                        record.status,
                        EntityStatus::Removed { .. }
                            | EntityStatus::Released { .. }
                            | EntityStatus::Shutdown { .. }
                    )
                })
                .map(|record| record.identity_key.clone())
                .collect::<HashSet<_>>();
            for spec in &specs {
                let identity = identity_key(spec);
                if live.contains(&identity) {
                    return Err(Error::InvalidInput(format!(
                        "entity lease is already active for {}/{}",
                        spec.domain, spec.provider_key
                    )));
                }
            }

            let receipt_keys = specs.iter().map(receipt_key).collect::<Vec<_>>();
            let mut generated_ids = lock(&self.generated_ids);
            let original_generated = generated_ids.clone();
            let registry_ids = self
                .registries
                .provider_reserved_entity_ids()
                .into_iter()
                .collect::<BTreeSet<_>>();
            let runtime_ids = self
                .runtime
                .states()
                .into_iter()
                .map(|state| state.entity_id)
                .collect::<BTreeSet<_>>();
            let mut generated_candidates = BTreeMap::new();
            let mut reserved_generated_ids = BTreeSet::new();
            for (spec, key) in specs.iter().zip(&receipt_keys) {
                if spec.unique_id.is_some() {
                    continue;
                }
                let candidate = if let Some(existing) = generated_ids.get(key) {
                    validate_entity_id_for_domain(existing, &spec.domain)?;
                    if registry_ids.contains(existing) {
                        return Err(Error::InvalidInput(format!(
                            "generated entity ID {existing} is now registered by another entity"
                        )));
                    }
                    existing.clone()
                } else if let Some(previous) = spec.prior_entity_id.as_deref() {
                    validate_entity_id_for_domain(previous, &spec.domain)?;
                    if registry_ids.contains(previous) {
                        return Err(Error::InvalidInput(format!(
                            "prior entity ID {previous} is already registered"
                        )));
                    }
                    previous.to_owned()
                } else {
                    let base = generated_base(spec);
                    let mut candidate = format!("{}.{}", spec.domain, base);
                    let mut suffix = 2_u64;
                    while registry_ids.contains(&candidate)
                        || runtime_ids.contains(&candidate)
                        || reserved_generated_ids.contains(&candidate)
                        || generated_ids
                            .iter()
                            .any(|(receipt, entity_id)| receipt != key && entity_id == &candidate)
                    {
                        candidate = format!("{}.{}_{}", spec.domain, base, suffix);
                        suffix += 1;
                    }
                    candidate
                };
                if generated_ids
                    .iter()
                    .any(|(receipt, entity_id)| receipt != key && entity_id == &candidate)
                {
                    return Err(Error::InvalidInput(format!(
                        "generated entity ID {candidate} is already owned by another provider receipt"
                    )));
                }
                if !reserved_generated_ids.insert(candidate.clone()) {
                    return Err(Error::InvalidInput(format!(
                        "multiple provider entities selected {candidate}"
                    )));
                }
                generated_candidates.insert(key.clone(), candidate.clone());
                generated_ids.insert(key.clone(), candidate);
            }

            let mut registrations = Vec::new();
            for spec in &specs {
                let Some(unique_id) = spec.unique_id.as_deref() else {
                    continue;
                };
                let mut info = spec.info.clone();
                if info.suggested_object_id.is_none() {
                    info.suggested_object_id = spec.suggested_object_id.clone();
                }
                let mut registration =
                    EntityRegistration::new(spec.domain.clone(), spec.platform.clone(), unique_id);
                registration.suggested_object_id = info.suggested_object_id.clone();
                registration.config_entry_id = spec.config_entry_id.clone();
                registration.config_subentry_id = spec.config_subentry_id.clone();
                registration.device_id = spec.device_id.clone();
                registration.area_id = spec.area_id.clone();
                registration.entity_category = info.entity_category.clone();
                registration.unit_of_measurement = info.unit_of_measurement.clone();
                registration.translation_key = info.translation_key.clone();
                registration.supported_features = info.supported_features;
                registration.has_entity_name = info.has_entity_name;
                registration.disabled_by = (!spec.enabled_by_default).then(|| "integration".into());
                registrations.push(ProviderEntityRegistration::new(registration, info));
            }
            let reserved_entity_ids = reserved_generated_ids
                .iter()
                .cloned()
                .collect::<HashSet<_>>();

            // Persist generated-ID ownership receipts before registry entries are
            // changed. If this write fails, the registry remains untouched.
            if *generated_ids != original_generated {
                let persisted = serde_json::to_value(&*generated_ids)
                    .map_err(|error| Error::Storage(format!("serialize entity IDs: {error}")))?;
                if let Err(error) = self.runtime.set_data(GENERATED_ID_KEY, &persisted) {
                    *generated_ids = original_generated;
                    return Err(error);
                }
            }
            drop(generated_ids);

            let registry_identities = registrations
                .iter()
                .map(|item| {
                    let registration = &item.registration;
                    registry_identity_key_parts(
                        &registration.domain,
                        &registration.platform,
                        &registration.unique_id,
                    )
                })
                .collect::<HashSet<_>>();
            let registry_before =
                registry_snapshot_for_identities(self.registries.entities(), &registry_identities);
            let provider_entries = match self
                .registries
                .get_or_create_provider_entities_reserving(registrations, &reserved_entity_ids)
            {
                Ok(entries) => {
                    registry_committed = true;
                    entries
                }
                Err(error) => {
                    // The registry transaction normally rolls itself back when an
                    // event write fails. If that compensation also failed, retain
                    // the provider receipt for the committed registry snapshot.
                    registry_committed = registry_snapshot_for_identities(
                        self.registries.entities(),
                        &registry_identities,
                    ) != registry_before;
                    self.restore_generated_receipts(original_generated)?;
                    return Err(error);
                }
            };
            let entries_by_identity = provider_entries
                .into_iter()
                .map(|entry| {
                    (
                        (
                            entry.domain.clone(),
                            entry.platform.clone(),
                            entry.unique_id.clone(),
                        ),
                        entry,
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let mut handles = Vec::with_capacity(specs.len());
            let mut to_restore = Vec::new();
            for (spec, key) in specs.into_iter().zip(receipt_keys) {
                let identity = identity_key(&spec);
                let info = effective_info(&spec);
                let (registry_id, entity_id) = if spec.unique_id.is_some() {
                    let entry = entries_by_identity
                        .get(&(
                            spec.domain.clone(),
                            spec.platform.clone(),
                            spec.unique_id.clone().unwrap_or_default(),
                        ))
                        .cloned()
                        .ok_or_else(|| {
                            Error::Service("registered entity was not returned after commit".into())
                        })?;
                    (Some(entry.id.clone()), entry.entity_id.clone())
                } else {
                    let entity_id = generated_candidates.get(&key).cloned().ok_or_else(|| {
                        Error::Service("generated entity ID was not allocated".into())
                    })?;
                    (None, entity_id)
                };
                let status = if registry_id.as_ref().is_some_and(|id| {
                    self.registries
                        .provider_entity(id)
                        .is_some_and(|entry| entry.disabled_by.is_some())
                }) {
                    EntityStatus::Disabled {
                        entity_id: entity_id.clone(),
                    }
                } else {
                    EntityStatus::Active {
                        entity_id: entity_id.clone(),
                    }
                };
                let should_restore = !matches!(status, EntityStatus::Disabled { .. });
                let (change_tx, change_rx) = watch::channel(EntityLifecycleChange {
                    status: status.clone(),
                    reason: EntityLifecycleReason::Registered,
                    previous_entity_id: None,
                });
                let token = self.next_lease_id.fetch_add(1, Ordering::Relaxed);
                let restore_key = registry_id.clone().unwrap_or_else(|| key.clone());
                let existing_state = self.runtime.get_state(&entity_id);
                let mut record = EntityRecord {
                    identity_key: identity,
                    provider_id: spec.provider_id.clone(),
                    registry_id: registry_id.clone(),
                    restore_key,
                    entity_id: entity_id.clone(),
                    info,
                    recording_policy: spec.recording_policy.clone(),
                    latest: None,
                    live_attributes: BTreeMap::new(),
                    owned_state: existing_state.clone(),
                    status,
                    lease_id: token,
                    changes: change_tx,
                };
                let restored = self.restored_snapshot(&record.restore_key);
                if let Some(restored) = restored {
                    record.latest = Some(restored);
                    record.live_attributes = self.restored_live_attributes(&record.restore_key);
                } else if let Some(state) = existing_state {
                    record.latest = Some(snapshot_from_state(&state));
                }
                records.insert(key.clone(), record);
                let lease = Arc::new(EntityLease {
                    manager: Arc::downgrade(self),
                    receipt_key: key,
                    provider_id: spec.provider_id,
                    provider_key: spec.provider_key,
                    registry_id,
                    id: token,
                    changes: change_rx,
                    released: AtomicBool::new(false),
                });
                let handle = EntityHandle { lease };
                if should_restore {
                    to_restore.push(handle.clone());
                }
                handles.push(handle);
            }
            drop(records);

            // Restore a pre-unload snapshot before the provider's first fresh
            // publication. The provider can immediately replace it with a new
            // snapshot after registration returns.
            for handle in &to_restore {
                self.restore_for_handle(handle)?;
            }
            Ok(handles)
        })();
        (result, registry_committed)
    }

    fn restore_generated_receipts(&self, previous: BTreeMap<String, String>) -> Result<()> {
        let serialized = serde_json::to_value(&previous)
            .map_err(|error| Error::Storage(format!("serialize entity IDs: {error}")))?;
        self.runtime.set_data(GENERATED_ID_KEY, &serialized)?;
        *lock(&self.generated_ids) = previous;
        Ok(())
    }

    fn restored_snapshot(&self, restore_key: &str) -> Option<EntitySnapshot> {
        lock(&self.restore_data)
            .get(restore_key)
            .map(|stored| stored.snapshot.clone())
    }

    fn restored_live_attributes(&self, restore_key: &str) -> BTreeMap<String, LiveAttributeUpdate> {
        lock(&self.restore_data)
            .get(restore_key)
            .and_then(|stored| stored.live_attributes.clone())
            .unwrap_or_default()
    }

    fn restored_owned_state(&self, restore_key: &str) -> Option<State> {
        lock(&self.restore_data)
            .get(restore_key)
            .and_then(|stored| stored.owned_state.clone())
    }

    fn restore_for_handle(&self, handle: &EntityHandle) -> Result<()> {
        let mut records = lock(&self.records);
        let Some(record) = records.get_mut(&handle.lease.receipt_key) else {
            return Ok(());
        };
        if record.lease_id != handle.lease.id || self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(snapshot) = self.restored_snapshot(&record.restore_key) else {
            return Ok(());
        };
        let registry_entry = match record.registry_id.as_deref() {
            Some(id) => self.registries.provider_entity(id),
            None => None,
        };
        if registry_entry
            .as_ref()
            .is_some_and(|entry| entry.disabled_by.is_some())
        {
            return Ok(());
        }
        if record.registry_id.is_some() && registry_entry.is_none() {
            self.emit_change(
                record,
                EntityStatus::Removed {
                    entity_id: record.entity_id.clone(),
                },
                EntityLifecycleReason::Removed,
                None,
            );
            return Ok(());
        }
        if let Some(entry) = registry_entry.as_ref() {
            record.entity_id = entry.entity_id.clone();
        }
        record.latest = Some(snapshot.clone());
        record.live_attributes = self.restored_live_attributes(&record.restore_key);
        let current = self.runtime.get_state(&record.entity_id);
        let should_restore = current
            .as_ref()
            .is_none_or(|state| state.attributes.get("restored") == Some(&Value::Bool(true)));
        if should_restore {
            let Some(state_change) = self.write_snapshot_to_id_if_unchanged(
                record,
                &record.entity_id,
                &snapshot,
                registry_entry.as_ref(),
                SnapshotWriteOptions {
                    live_attributes: &record.live_attributes,
                    restored: false,
                    expected: current.as_ref(),
                },
            )?
            else {
                record.owned_state = None;
                return Ok(());
            };
            record.owned_state = Some(state_change.state);
        }
        self.consume_restored_snapshot(&record.restore_key)?;
        Ok(())
    }

    fn consume_restored_snapshot(&self, restore_key: &str) -> Result<()> {
        let mut restore_data = lock(&self.restore_data);
        if restore_data.remove(restore_key).is_some() {
            self.persist_restore_data(&restore_data)?;
        }
        Ok(())
    }

    fn persist_restore_data(
        &self,
        restore_data: &BTreeMap<String, RestoredSnapshot>,
    ) -> Result<()> {
        let serialized = serde_json::to_value(restore_data)
            .map_err(|error| Error::Storage(format!("serialize entity restore states: {error}")))?;
        self.runtime.set_data(RESTORE_DATA_KEY, &serialized)
    }

    fn save_restore_snapshot(&self, record: &EntityRecord, snapshot: EntitySnapshot) -> Result<()> {
        let mut restore_data = lock(&self.restore_data);
        let old = restore_data.insert(
            record.restore_key.clone(),
            RestoredSnapshot {
                snapshot,
                saved_at: now_seconds(),
                owned_state: record.owned_state.clone(),
                live_attributes: Some(record.live_attributes.clone()),
            },
        );
        if let Err(error) = self.persist_restore_data(&restore_data) {
            match old {
                Some(old) => {
                    restore_data.insert(record.restore_key.clone(), old);
                }
                None => {
                    restore_data.remove(&record.restore_key);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn release(self: &Arc<Self>, receipt_key: &str, lease_id: u64) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut records = lock(&self.records);
        let Some(record) = records.get_mut(receipt_key) else {
            return Ok(());
        };
        if record.lease_id != lease_id {
            return Ok(());
        }
        self.release_record(record)?;
        records.remove(receipt_key);
        Ok(())
    }

    fn release_record(&self, record: &mut EntityRecord) -> Result<()> {
        if matches!(
            record.status,
            EntityStatus::Removed { .. } | EntityStatus::Released { .. }
        ) {
            let id = record.entity_id.clone();
            self.emit_change(
                record,
                EntityStatus::Released { entity_id: id },
                EntityLifecycleReason::Released,
                None,
            );
            return Ok(());
        }

        let entry = record
            .registry_id
            .as_deref()
            .and_then(|id| self.registries.provider_entity(id));
        if record.registry_id.is_some() && entry.is_none() {
            self.deactivate_record(
                record,
                EntityStatus::Removed {
                    entity_id: record.entity_id.clone(),
                },
                EntityLifecycleReason::Removed,
            )?;
            return Ok(());
        }
        if entry
            .as_ref()
            .is_some_and(|entry| entry.disabled_by.is_some())
        {
            self.deactivate_record(
                record,
                EntityStatus::Disabled {
                    entity_id: entry.as_ref().expect("checked above").entity_id.clone(),
                },
                EntityLifecycleReason::Disabled,
            )?;
        } else if let Some(entry) = entry.as_ref() {
            let snapshot = Self::snapshot_for_record(record);
            if let Some(snapshot) = snapshot {
                self.save_restore_snapshot(record, snapshot)?;
                if let Some(expected) = record
                    .owned_state
                    .as_ref()
                    .filter(|state| state.entity_id == record.entity_id)
                {
                    let unavailable = EntitySnapshot {
                        state: None,
                        available: false,
                        attributes: Map::new(),
                        context: record
                            .latest
                            .as_ref()
                            .map(|value| value.context.clone())
                            .unwrap_or_default(),
                    };
                    let attributes = self.compose_attributes(
                        &unavailable,
                        Some(entry),
                        &record.info,
                        &record.live_attributes,
                        true,
                    );
                    let state_change = self.runtime.set_state_if_unchanged_with_recording_policy(
                        &record.entity_id,
                        Some(expected),
                        "unavailable",
                        attributes,
                        unavailable.context,
                        false,
                        &record.recording_policy,
                    )?;
                    record.owned_state = state_change.map(|change| change.state);
                } else {
                    record.owned_state = None;
                }
            }
        } else {
            if let Some(snapshot) = Self::snapshot_for_record(record) {
                self.save_restore_snapshot(record, snapshot)?;
            }
            self.clear_owned_state(record)?;
        }
        let id = record.entity_id.clone();
        self.emit_change(
            record,
            EntityStatus::Released { entity_id: id },
            EntityLifecycleReason::Released,
            None,
        );
        Ok(())
    }

    fn abandon_failed_release(&self, receipt_key: &str, lease_id: u64, error: Error) {
        let mut records = lock(&self.records);
        let Some(record) = records.get(receipt_key) else {
            return;
        };
        if record.lease_id != lease_id {
            return;
        }
        let Some(mut record) = records.remove(receipt_key) else {
            return;
        };
        let retry_status = record.status.clone();
        let id = record.entity_id.clone();
        self.emit_change(
            &mut record,
            EntityStatus::Released { entity_id: id },
            EntityLifecycleReason::Released,
            None,
        );
        lock(&self.pending_releases).insert(
            (receipt_key.to_owned(), lease_id),
            PendingRelease {
                receipt_key: receipt_key.to_owned(),
                record,
                retry_status,
                attempts: 0,
                retry_at: tokio::time::Instant::now() + pending_release_delay(0),
                last_error: error.to_string(),
            },
        );
    }

    fn retry_pending_releases(&self, force: bool) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let keys = lock(&self.pending_releases)
            .iter()
            .filter(|(_, pending)| force || pending.retry_at <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        for key in keys {
            let records = lock(&self.records);
            let mut pending = lock(&self.pending_releases);
            let Some(mut item) = pending.remove(&key) else {
                continue;
            };
            if records.contains_key(&item.receipt_key) {
                continue;
            }
            if !force && item.retry_at > tokio::time::Instant::now() {
                pending.insert(key, item);
                continue;
            }
            item.record.status = item.retry_status.clone();
            match self.release_record(&mut item.record) {
                Ok(()) => {}
                Err(error) => {
                    item.attempts = item.attempts.saturating_add(1);
                    item.retry_at =
                        tokio::time::Instant::now() + pending_release_delay(item.attempts);
                    item.last_error = error.to_string();
                    pending.insert(key, item);
                }
            }
            drop(pending);
            drop(records);
        }

        lock(&self.pending_releases)
            .values()
            .map(|pending| {
                format!(
                    "{} (identity {}): {}",
                    pending.receipt_key, pending.record.identity_key, pending.last_error
                )
            })
            .collect()
    }

    fn snapshot_for_record(record: &EntityRecord) -> Option<EntitySnapshot> {
        record
            .owned_state
            .as_ref()
            .filter(|state| state.attributes.get("restored") != Some(&Value::Bool(true)))
            .map(snapshot_from_state)
            .or_else(|| record.latest.clone())
    }

    fn clear_owned_state(&self, record: &mut EntityRecord) -> Result<()> {
        let Some(expected) = record.owned_state.take() else {
            return Ok(());
        };
        if let Err(error) = self
            .runtime
            .remove_state_if_unchanged(&expected, expected.context.clone())
        {
            record.owned_state = Some(expected);
            return Err(error);
        }
        Ok(())
    }

    fn publish(
        self: &Arc<Self>,
        receipt_key: &str,
        lease_id: u64,
        publication: EntityPublication,
    ) -> Result<Option<StateChange>> {
        let EntityPublication {
            snapshot,
            live_attributes,
        } = publication;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Service("entity lifecycle manager is closed".into()));
        }
        let mut records = lock(&self.records);
        let Some(record) = records.get_mut(receipt_key) else {
            return Err(Error::Service("entity lease has been released".into()));
        };
        if record.lease_id != lease_id {
            return Err(Error::Service("entity lease is stale".into()));
        }
        if matches!(
            record.status,
            EntityStatus::Removed { .. }
                | EntityStatus::Released { .. }
                | EntityStatus::Shutdown { .. }
        ) {
            return Ok(None);
        }
        let entry = record
            .registry_id
            .as_deref()
            .and_then(|id| self.registries.provider_entity(id));
        if record.registry_id.is_some() && entry.is_none() {
            self.deactivate_record(
                record,
                EntityStatus::Removed {
                    entity_id: record.entity_id.clone(),
                },
                EntityLifecycleReason::Removed,
            )?;
            return Ok(None);
        }
        if entry
            .as_ref()
            .is_some_and(|entry| entry.disabled_by.is_some())
        {
            self.deactivate_record(
                record,
                EntityStatus::Disabled {
                    entity_id: entry.as_ref().expect("checked above").entity_id.clone(),
                },
                EntityLifecycleReason::Disabled,
            )?;
            return Ok(None);
        }
        if let Some(entry) = entry.as_ref() {
            self.move_state_if_renamed(record, entry)?;
        }
        let current_id = entry
            .as_ref()
            .map(|entry| entry.entity_id.as_str())
            .unwrap_or(record.entity_id.as_str())
            .to_owned();
        let previous_status = record.status.clone();
        let status = EntityStatus::Active {
            entity_id: current_id.clone(),
        };
        if !previous_status.is_active() {
            let reason = if matches!(previous_status, EntityStatus::Disabled { .. }) {
                EntityLifecycleReason::Enabled
            } else {
                EntityLifecycleReason::RegistryUpdated
            };
            self.emit_change(record, status, reason, None);
        } else if record.entity_id != current_id {
            let old_id = record.entity_id.clone();
            self.emit_change(record, status, EntityLifecycleReason::Renamed, Some(old_id));
        }
        record.entity_id = current_id;
        let state_change =
            self.write_snapshot(record, &snapshot, entry.as_ref(), &live_attributes, false)?;
        record.latest = Some(snapshot);
        record.live_attributes = live_attributes;
        record.owned_state = Some(state_change.state.clone());
        Ok(Some(state_change))
    }

    fn deactivate_record(
        &self,
        record: &mut EntityRecord,
        status: EntityStatus,
        reason: EntityLifecycleReason,
    ) -> Result<()> {
        if record.status.is_active() {
            if let Some(snapshot) = Self::snapshot_for_record(record) {
                self.save_restore_snapshot(record, snapshot.clone())?;
                record.latest = Some(snapshot);
            }
            self.clear_owned_state(record)?;
        }
        self.emit_change(record, status, reason, None);
        Ok(())
    }

    fn move_state_if_renamed(&self, record: &mut EntityRecord, entry: &EntityEntry) -> Result<()> {
        if record.entity_id == entry.entity_id {
            return Ok(());
        }
        let old_id = record.entity_id.clone();
        let old_state = record.owned_state.clone();
        let snapshot = record
            .latest
            .clone()
            .or_else(|| old_state.as_ref().map(snapshot_from_state));
        let new_state = if let Some(snapshot) = snapshot.as_ref() {
            let Some(change) = self.write_snapshot_to_id_if_unchanged(
                record,
                &entry.entity_id,
                snapshot,
                Some(entry),
                SnapshotWriteOptions {
                    live_attributes: &record.live_attributes,
                    restored: false,
                    expected: None,
                },
            )?
            else {
                return Err(Error::InvalidInput(format!(
                    "renamed entity ID {} already has a runtime state",
                    entry.entity_id
                )));
            };
            Some(change.state)
        } else {
            None
        };
        if let Some(old_state) = old_state.as_ref()
            && let Err(error) = self
                .runtime
                .remove_state_if_unchanged(old_state, old_state.context.clone())
        {
            if let Some(new_state) = new_state.as_ref()
                && let Err(rollback_error) = self
                    .runtime
                    .remove_state_if_unchanged(new_state, new_state.context.clone())
            {
                record.owned_state = Some(new_state.clone());
                return Err(Error::Storage(format!(
                    "removing renamed state {old_id} failed: {error}; removing new state {} failed: {rollback_error}",
                    new_state.entity_id
                )));
            }
            return Err(error);
        }
        if let Some(snapshot) = snapshot {
            record.latest = Some(snapshot);
        }
        record.owned_state = new_state;
        let previous = record.entity_id.clone();
        record.entity_id = entry.entity_id.clone();
        self.emit_change(
            record,
            EntityStatus::Active {
                entity_id: entry.entity_id.clone(),
            },
            EntityLifecycleReason::Renamed,
            Some(previous),
        );
        Ok(())
    }

    fn write_snapshot(
        &self,
        record: &EntityRecord,
        snapshot: &EntitySnapshot,
        registry_entry: Option<&EntityEntry>,
        live_attributes: &BTreeMap<String, LiveAttributeUpdate>,
        restored: bool,
    ) -> Result<StateChange> {
        self.write_snapshot_to_id(
            record,
            &record.entity_id,
            snapshot,
            registry_entry,
            live_attributes,
            restored,
        )
    }

    fn write_snapshot_to_id(
        &self,
        record: &EntityRecord,
        entity_id: &str,
        snapshot: &EntitySnapshot,
        registry_entry: Option<&EntityEntry>,
        live_attributes: &BTreeMap<String, LiveAttributeUpdate>,
        restored: bool,
    ) -> Result<StateChange> {
        let state = if !snapshot.available {
            "unavailable"
        } else {
            snapshot.state.as_deref().unwrap_or("unknown")
        };
        let attributes = self.compose_attributes(
            snapshot,
            registry_entry,
            &record.info,
            live_attributes,
            restored,
        );
        self.runtime.set_state_with_recording_policy(
            entity_id,
            state,
            attributes,
            snapshot.context.clone(),
            false,
            &record.recording_policy,
        )
    }

    fn write_snapshot_to_id_if_unchanged(
        &self,
        record: &EntityRecord,
        entity_id: &str,
        snapshot: &EntitySnapshot,
        registry_entry: Option<&EntityEntry>,
        options: SnapshotWriteOptions<'_>,
    ) -> Result<Option<StateChange>> {
        let state = if !snapshot.available {
            "unavailable"
        } else {
            snapshot.state.as_deref().unwrap_or("unknown")
        };
        let attributes = self.compose_attributes(
            snapshot,
            registry_entry,
            &record.info,
            options.live_attributes,
            options.restored,
        );
        self.runtime.set_state_if_unchanged_with_recording_policy(
            entity_id,
            options.expected,
            state,
            attributes,
            snapshot.context.clone(),
            false,
            &record.recording_policy,
        )
    }

    fn compose_attributes(
        &self,
        snapshot: &EntitySnapshot,
        registry_entry: Option<&EntityEntry>,
        info: &ProviderEntityInfo,
        live_attributes: &BTreeMap<String, LiveAttributeUpdate>,
        restored: bool,
    ) -> Map<String, Value> {
        let mut attributes = Map::new();
        if let Some(capabilities) = registry_entry
            .and_then(|entry| entry.capabilities.as_ref())
            .or(info.capabilities.as_ref())
            .and_then(Value::as_object)
        {
            attributes.extend(capabilities.clone());
        }
        if snapshot.available {
            attributes.extend(snapshot.attributes.clone());
        }
        for (name, update) in live_attributes {
            match update {
                LiveAttributeUpdate::Set(value) => {
                    attributes.insert(name.clone(), value.clone());
                }
                LiveAttributeUpdate::Remove => {
                    attributes.remove(name);
                }
            }
        }

        let name_removed = matches!(
            live_attributes.get("friendly_name"),
            Some(LiveAttributeUpdate::Remove)
        );
        let name_is_live = matches!(
            live_attributes.get("friendly_name"),
            Some(LiveAttributeUpdate::Set(_))
        );
        let name_override = registry_entry.and_then(|entry| non_empty(entry.name.as_deref()));
        let original_name = (!name_removed)
            .then(|| {
                registry_entry
                    .and_then(|entry| entry.original_name.as_deref())
                    .or(info.original_name.as_deref())
            })
            .flatten();
        insert_override_or_fallback(
            &mut attributes,
            "friendly_name",
            name_override,
            original_name,
            snapshot.available || name_is_live,
        );

        let icon_removed = matches!(
            live_attributes.get("icon"),
            Some(LiveAttributeUpdate::Remove)
        );
        let icon_is_live = matches!(
            live_attributes.get("icon"),
            Some(LiveAttributeUpdate::Set(_))
        );
        let icon_override = registry_entry.and_then(|entry| non_empty(entry.icon.as_deref()));
        let original_icon = (!icon_removed)
            .then(|| {
                registry_entry
                    .and_then(|entry| entry.original_icon.as_deref())
                    .or(info.original_icon.as_deref())
            })
            .flatten();
        insert_override_or_fallback(
            &mut attributes,
            "icon",
            icon_override,
            original_icon,
            snapshot.available || icon_is_live,
        );

        let class_removed = matches!(
            live_attributes.get("device_class"),
            Some(LiveAttributeUpdate::Remove)
        );
        let class_is_live = matches!(
            live_attributes.get("device_class"),
            Some(LiveAttributeUpdate::Set(_))
        );
        let class_override =
            registry_entry.and_then(|entry| non_empty(entry.device_class.as_deref()));
        let original_class = (!class_removed)
            .then(|| {
                registry_entry
                    .and_then(|entry| entry.original_device_class.as_deref())
                    .or(info.original_device_class.as_deref())
            })
            .flatten();
        insert_override_or_fallback(
            &mut attributes,
            "device_class",
            class_override,
            original_class,
            snapshot.available || class_is_live,
        );

        let unit_removed = matches!(
            live_attributes.get("unit_of_measurement"),
            Some(LiveAttributeUpdate::Remove)
        );
        if !unit_removed && !attributes.contains_key("unit_of_measurement") {
            let unit = registry_entry
                .and_then(|entry| entry.unit_of_measurement.as_deref())
                .or(info.unit_of_measurement.as_deref());
            if let Some(unit) = unit {
                attributes.insert("unit_of_measurement".into(), Value::String(unit.into()));
            }
        }
        let features_removed = matches!(
            live_attributes.get("supported_features"),
            Some(LiveAttributeUpdate::Remove)
        );
        if !features_removed
            && info.supported_features != 0
            && !attributes.contains_key("supported_features")
        {
            attributes.insert(
                "supported_features".into(),
                Value::from(info.supported_features),
            );
        }
        if restored {
            attributes.insert("restored".into(), Value::Bool(true));
        }
        attributes
    }

    fn emit_change(
        &self,
        record: &mut EntityRecord,
        status: EntityStatus,
        reason: EntityLifecycleReason,
        previous_entity_id: Option<String>,
    ) {
        record.status = status.clone();
        record.changes.send_replace(EntityLifecycleChange {
            status,
            reason,
            previous_entity_id,
        });
    }

    fn current_status(&self, receipt_key: &str, lease_id: u64) -> Option<EntityStatus> {
        let records = lock(&self.records);
        let record = records.get(receipt_key)?;
        if record.lease_id != lease_id {
            return Some(EntityStatus::Released {
                entity_id: record.entity_id.clone(),
            });
        }
        if self.closed.load(Ordering::Acquire) {
            return Some(EntityStatus::Shutdown {
                entity_id: record.entity_id.clone(),
            });
        }
        let Some(registry_id) = record.registry_id.as_deref() else {
            return Some(record.status.clone());
        };
        match self.registries.provider_entity(registry_id) {
            None => Some(EntityStatus::Removed {
                entity_id: record.entity_id.clone(),
            }),
            Some(entry) if entry.disabled_by.is_some() => Some(EntityStatus::Disabled {
                entity_id: entry.entity_id,
            }),
            Some(_) if matches!(&record.status, EntityStatus::Removed { .. }) => {
                Some(record.status.clone())
            }
            Some(entry) => Some(EntityStatus::Active {
                entity_id: entry.entity_id,
            }),
        }
    }

    fn reconcile_all(&self, context: &Context) -> Result<()> {
        let mut records = lock(&self.records);
        let mut first_error = None;
        for record in records.values_mut() {
            if record.registry_id.is_none() {
                continue;
            }
            if let Err(error) = self.reconcile_record(record) {
                first_error.get_or_insert(error);
            }
        }
        drop(records);
        // A provider release removes its live record, but its registered
        // unavailable placeholder still follows registry disable changes.
        // This is also needed when a config entry disables registries after
        // unloading its providers, as the pinned entity registry does.
        for entry in self.registries.entities() {
            if entry.disabled_by.is_none() {
                continue;
            }
            let Some(state) = self.runtime.get_state(&entry.entity_id) else {
                continue;
            };
            if state.attributes.get("restored") != Some(&Value::Bool(true)) {
                continue;
            }
            if let Err(error) = self
                .runtime
                .remove_state_if_unchanged(&state, context.clone())
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn reconcile_record(&self, record: &mut EntityRecord) -> Result<()> {
        if matches!(&record.status, EntityStatus::Removed { .. }) {
            return Ok(());
        }
        let registry_id = record
            .registry_id
            .as_deref()
            .expect("registry record checked by caller");
        let Some(entry) = self.registries.provider_entity(registry_id) else {
            self.deactivate_record(
                record,
                EntityStatus::Removed {
                    entity_id: record.entity_id.clone(),
                },
                EntityLifecycleReason::Removed,
            )?;
            return Ok(());
        };
        if entry.disabled_by.is_some() {
            let id = entry.entity_id.clone();
            self.deactivate_record(
                record,
                EntityStatus::Disabled { entity_id: id },
                EntityLifecycleReason::Disabled,
            )?;
            record.entity_id = entry.entity_id;
            return Ok(());
        }

        let old_status = record.status.clone();
        let old_id = record.entity_id.clone();
        if old_id != entry.entity_id {
            if record.status.is_active() {
                self.move_state_if_renamed(record, &entry)?;
            } else {
                record.entity_id = entry.entity_id.clone();
            }
        }

        match old_status {
            EntityStatus::Disabled { .. } => {
                let snapshot = self
                    .restored_snapshot(&record.restore_key)
                    .or_else(|| record.latest.clone());
                if let Some(snapshot) = snapshot {
                    record.latest = Some(snapshot.clone());
                    let state_change = if old_id != record.entity_id {
                        self.write_snapshot_to_id_if_unchanged(
                            record,
                            &record.entity_id,
                            &snapshot,
                            Some(&entry),
                            SnapshotWriteOptions {
                                live_attributes: &record.live_attributes,
                                restored: false,
                                expected: None,
                            },
                        )?
                        .ok_or_else(|| {
                            Error::InvalidInput(format!(
                                "renamed entity ID {} already has a runtime state",
                                record.entity_id
                            ))
                        })?
                    } else {
                        self.write_snapshot(
                            record,
                            &snapshot,
                            Some(&entry),
                            &record.live_attributes,
                            false,
                        )?
                    };
                    record.owned_state = Some(state_change.state);
                    self.consume_restored_snapshot(&record.restore_key)?;
                }
                self.emit_change(
                    record,
                    EntityStatus::Active {
                        entity_id: entry.entity_id,
                    },
                    EntityLifecycleReason::Enabled,
                    (old_id != record.entity_id).then_some(old_id),
                );
            }
            EntityStatus::Active { .. } if old_id != record.entity_id => {
                self.emit_change(
                    record,
                    EntityStatus::Active {
                        entity_id: record.entity_id.clone(),
                    },
                    EntityLifecycleReason::Renamed,
                    Some(old_id),
                );
            }
            EntityStatus::Active { .. } => {
                if let Some(snapshot) = record.latest.clone() {
                    let state_change = self.write_snapshot(
                        record,
                        &snapshot,
                        Some(&entry),
                        &record.live_attributes,
                        false,
                    )?;
                    record.owned_state = Some(state_change.state);
                }
                self.emit_change(
                    record,
                    EntityStatus::Active {
                        entity_id: entry.entity_id,
                    },
                    EntityLifecycleReason::RegistryUpdated,
                    None,
                );
            }
            EntityStatus::Removed { .. }
            | EntityStatus::Released { .. }
            | EntityStatus::Shutdown { .. } => {}
        }
        Ok(())
    }
}

async fn registry_event_loop(
    inner: Weak<EntityLifecycleInner>,
    mut events: broadcast::Receiver<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut retry = tokio::time::interval(Duration::from_millis(100));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return;
                }
                continue;
            }
            _ = retry.tick() => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let _ = inner.retry_pending_releases(false);
                continue;
            }
            event = events.recv() => event,
        };
        match event {
            Ok(event) if is_entity_registry_event(&event) => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let _ = inner.reconcile_all(&event.context);
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let _ = inner.reconcile_all(&Context::default());
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn is_entity_registry_event(event: &Event) -> bool {
    event.event_type == "entity_registry_updated"
}

fn preflight_specs(specs: &[EntitySpec]) -> Result<()> {
    let mut receipts = HashSet::new();
    let mut identities = HashSet::new();
    let mut prior_ids = HashSet::new();
    for spec in specs {
        validate_component(&spec.domain, "entity domain")?;
        if spec.platform.trim().is_empty() {
            return Err(Error::InvalidInput(
                "entity platform cannot be empty".into(),
            ));
        }
        if spec.provider_id.trim().is_empty() || spec.provider_key.trim().is_empty() {
            return Err(Error::InvalidInput(
                "provider ID and provider key cannot be empty".into(),
            ));
        }
        if spec
            .unique_id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(Error::InvalidInput(
                "entity unique ID cannot be empty".into(),
            ));
        }
        if let Some(prior) = spec.prior_entity_id.as_deref() {
            if spec.unique_id.is_some() {
                return Err(Error::InvalidInput(
                    "prior entity IDs are only valid without a unique ID".into(),
                ));
            }
            validate_entity_id_for_domain(prior, &spec.domain)?;
            if !prior_ids.insert(prior) {
                return Err(Error::InvalidInput(format!(
                    "duplicate prior entity ID {prior} in provider batch"
                )));
            }
        }
        if spec
            .info
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| !capabilities.is_object())
        {
            return Err(Error::InvalidInput(
                "entity capabilities must be an object".into(),
            ));
        }
        let identity = identity_key(spec);
        if !identities.insert(identity) {
            return Err(Error::InvalidInput(format!(
                "duplicate entity identity in provider batch for {}",
                spec.domain
            )));
        }
        let receipt = receipt_key(spec);
        if !receipts.insert(receipt) {
            return Err(Error::InvalidInput(format!(
                "duplicate provider receipt {}/{}",
                spec.provider_id, spec.provider_key
            )));
        }
    }
    Ok(())
}

fn identity_key(spec: &EntitySpec) -> String {
    match spec.unique_id.as_deref() {
        Some(unique_id) => registry_identity_key_parts(&spec.domain, &spec.platform, unique_id),
        None => receipt_key(spec),
    }
}

fn registry_identity_key(entry: &EntityEntry) -> String {
    registry_identity_key_parts(&entry.domain, &entry.platform, &entry.unique_id)
}

fn registry_snapshot_for_identities(
    entries: Vec<EntityEntry>,
    identities: &HashSet<String>,
) -> BTreeMap<String, EntityEntry> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let identity = registry_identity_key(&entry);
            identities.contains(&identity).then_some((identity, entry))
        })
        .collect()
}

fn registry_identity_key_parts(domain: &str, platform: &str, unique_id: &str) -> String {
    serde_json::to_string(&("registry", domain, platform, unique_id))
        .expect("serializing string tuple cannot fail")
}

fn receipt_key(spec: &EntitySpec) -> String {
    serde_json::to_string(&(
        spec.domain.as_str(),
        spec.platform.as_str(),
        spec.provider_id.as_str(),
        spec.provider_key.as_str(),
    ))
    .expect("serializing string tuple cannot fail")
}

fn effective_info(spec: &EntitySpec) -> ProviderEntityInfo {
    let mut info = spec.info.clone();
    if info.suggested_object_id.is_none() {
        info.suggested_object_id = spec.suggested_object_id.clone();
    }
    info
}

fn generated_base(spec: &EntitySpec) -> String {
    let fallback = spec
        .suggested_object_id
        .as_deref()
        .or(spec.info.suggested_object_id.as_deref())
        .or(spec.info.original_name.as_deref())
        .unwrap_or(&spec.provider_key);
    let base = slugify(fallback);
    if base.is_empty() {
        "entity".into()
    } else {
        base
    }
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut previous_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !output.is_empty() {
            output.push('_');
            previous_separator = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    output
}

fn validate_entity_id_for_domain(entity_id: &str, domain: &str) -> Result<()> {
    let Some((entity_domain, object_id)) = entity_id.split_once('.') else {
        return Err(Error::InvalidInput(format!(
            "invalid entity ID {entity_id:?}: expected domain.object_id"
        )));
    };
    if entity_domain != domain || object_id.contains('.') {
        return Err(Error::InvalidInput(format!(
            "entity ID {entity_id:?} must use domain {domain}"
        )));
    }
    validate_component(entity_domain, "entity domain")?;
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

fn insert_override_or_fallback(
    attributes: &mut Map<String, Value>,
    key: &str,
    override_value: Option<&str>,
    original_value: Option<&str>,
    preserve_dynamic: bool,
) {
    if let Some(value) = override_value {
        attributes.insert(key.into(), Value::String(value.into()));
    } else if preserve_dynamic && attributes.contains_key(key) {
        // A live template may expose a changing name/icon/class. That dynamic
        // attribute remains authoritative until the user sets an override.
    } else if let Some(value) = original_value.filter(|value| !value.is_empty()) {
        attributes.insert(key.into(), Value::String(value.into()));
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn snapshot_from_state(state: &State) -> EntitySnapshot {
    EntitySnapshot {
        state: Some(state.state.clone()),
        available: state.state != "unavailable",
        attributes: state.attributes.clone(),
        context: state.context.clone(),
    }
}

fn provider_info_from_entry(entry: &EntityEntry) -> ProviderEntityInfo {
    ProviderEntityInfo {
        original_name: entry.original_name.clone(),
        original_icon: entry.original_icon.clone(),
        original_device_class: entry.original_device_class.clone(),
        suggested_object_id: entry.suggested_object_id.clone(),
        entity_category: entry.entity_category.clone(),
        has_entity_name: entry.has_entity_name,
        unit_of_measurement: entry.unit_of_measurement.clone(),
        supported_features: entry.supported_features,
        translation_key: entry.translation_key.clone(),
        capabilities: entry.capabilities.clone(),
    }
}

fn load_map(runtime: &Runtime, key: &str, label: &str) -> Result<BTreeMap<String, String>> {
    match runtime.get_data(key)? {
        Some(value) => serde_json::from_value(value)
            .map_err(|error| Error::Storage(format!("invalid {label} metadata: {error}"))),
        None => Ok(BTreeMap::new()),
    }
}

fn load_registry_receipts(runtime: &Runtime) -> Result<BTreeMap<String, BTreeSet<String>>> {
    match runtime.get_data(REGISTRY_RECEIPTS_KEY)? {
        Some(value) => serde_json::from_value(value).map_err(|error| {
            Error::Storage(format!("invalid entity registry receipt metadata: {error}"))
        }),
        None => Ok(BTreeMap::new()),
    }
}

fn load_restore_data(runtime: &Runtime) -> Result<BTreeMap<String, RestoredSnapshot>> {
    let mut data: BTreeMap<String, RestoredSnapshot> = match runtime.get_data(RESTORE_DATA_KEY)? {
        Some(value) => serde_json::from_value(value)
            .map_err(|error| Error::Storage(format!("invalid entity restore metadata: {error}")))?,
        None => BTreeMap::new(),
    };
    let now = now_seconds();
    data.retain(|_, state| now.saturating_sub(state.saved_at) <= MAX_RESTORE_AGE_SECONDS);
    Ok(data)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn pending_release_delay(attempts: u32) -> Duration {
    Duration::from_millis(100_u64.saturating_mul(1_u64 << attempts.min(8)))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{
        EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, Runtime, Store,
    };
    use hass_registry::{EntityUpdate, Registries};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn runtime() -> (Runtime, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        (runtime, store)
    }

    #[derive(Default)]
    struct FaultInjectionStore {
        inner: MemoryStore,
        fail_next_put_state: AtomicBool,
        fail_next_remove_state: AtomicBool,
        fail_next_set_data: AtomicBool,
        fail_all_set_data: AtomicBool,
        fail_next_append_event: AtomicBool,
        fail_registry_rollback: AtomicBool,
    }

    impl Store for FaultInjectionStore {
        fn load_states(&self) -> hass_core::Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> hass_core::Result<()> {
            if self.fail_next_put_state.swap(false, Ordering::SeqCst) {
                return Err(Error::Storage("injected state write failure".into()));
            }
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> hass_core::Result<()> {
            if self.fail_next_remove_state.swap(false, Ordering::SeqCst) {
                return Err(Error::Storage("injected state removal failure".into()));
            }
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> hass_core::Result<()> {
            if self.fail_next_append_event.swap(false, Ordering::SeqCst) {
                if self.fail_registry_rollback.swap(false, Ordering::SeqCst) {
                    self.fail_next_set_data.store(true, Ordering::SeqCst);
                }
                return Err(Error::Storage("injected event append failure".into()));
            }
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
            if self.fail_all_set_data.load(Ordering::SeqCst)
                || self.fail_next_set_data.swap(false, Ordering::SeqCst)
            {
                return Err(Error::Storage("injected metadata write failure".into()));
            }
            self.inner.set_data(key, value)
        }

        fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
            self.inner.delete_data(key)
        }

        fn history(
            &self,
            entity_id: &str,
            start: chrono::DateTime<chrono::Utc>,
            end: chrono::DateTime<chrono::Utc>,
        ) -> hass_core::Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }

        fn history_query(&self, query: &HistoryQuery) -> hass_core::Result<Vec<HistorySeries>> {
            self.inner.history_query(query)
        }

        fn event_history_query(&self, query: &EventHistoryQuery) -> hass_core::Result<Vec<Event>> {
            self.inner.event_history_query(query)
        }

        fn purge_before(
            &self,
            cutoff: chrono::DateTime<chrono::Utc>,
        ) -> hass_core::Result<PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn repack(&self) -> hass_core::Result<()> {
            self.inner.repack()
        }
    }

    fn fault_runtime() -> (Runtime, Arc<FaultInjectionStore>) {
        let store = Arc::new(FaultInjectionStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        (runtime, store)
    }

    fn info() -> ProviderEntityInfo {
        ProviderEntityInfo {
            original_name: Some("Temperature".into()),
            original_icon: Some("mdi:thermometer".into()),
            original_device_class: Some("temperature".into()),
            suggested_object_id: Some("room_temperature".into()),
            unit_of_measurement: Some("°C".into()),
            ..ProviderEntityInfo::default()
        }
    }

    fn registered_spec(provider_id: &str) -> EntitySpec {
        EntitySpec::new("sensor", "fixture", provider_id, "temperature")
            .with_unique_id("temperature-1")
            .with_suggested_object_id("room_temperature")
            .with_info(info())
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle transition did not complete");
    }

    #[tokio::test]
    async fn registry_rename_disable_remove_and_publish_suppression() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let original_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("on")).unwrap().unwrap();

        let renamed_id = "sensor.room_temperature_renamed";
        registries
            .update_entity(
                &original_id,
                EntityUpdate {
                    new_entity_id: Some(renamed_id.into()),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(&original_id).is_none()
                && runtime.get_state(renamed_id).is_some()
                && handle.current_entity_id().as_deref() == Some(renamed_id)
        })
        .await;
        assert_eq!(runtime.get_state(renamed_id).unwrap().state, "on");

        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(renamed_id).is_none()
                && matches!(handle.status(), EntityStatus::Disabled { .. })
        })
        .await;
        assert!(
            handle
                .publish(EntitySnapshot::new("must-not-publish"))
                .unwrap()
                .is_none()
        );
        assert_eq!(registries.entities().len(), 1);

        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime
                .get_state(renamed_id)
                .is_some_and(|state| state.state == "on")
                && matches!(handle.status(), EntityStatus::Active { .. })
        })
        .await;

        registries.remove_entity(renamed_id).unwrap();
        wait_until(|| {
            runtime.get_state(renamed_id).is_none()
                && matches!(handle.status(), EntityStatus::Removed { .. })
        })
        .await;
        assert!(
            handle
                .publish(EntitySnapshot::new("must-not-recreate"))
                .unwrap()
                .is_none()
        );
        assert!(registries.entities().is_empty());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dynamic_attributes_user_overrides_and_provider_refresh_follow_ha_fallbacks() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        let mut dynamic = EntitySnapshot::new("20");
        dynamic
            .attributes
            .insert("friendly_name".into(), json!("Dynamic name"));
        dynamic.attributes.insert("icon".into(), json!("mdi:chart"));
        dynamic
            .attributes
            .insert("device_class".into(), json!("custom-class"));
        dynamic
            .attributes
            .insert("unit_of_measurement".into(), json!("dynamic-unit"));
        handle.publish(dynamic.clone()).unwrap();

        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    name: Some(Some("User name".into())),
                    icon: Some(Some("mdi:account".into())),
                    device_class: Some(Some("humidity".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(&entity_id).is_some_and(|state| {
                state.attributes.get("friendly_name") == Some(&json!("User name"))
                    && state.attributes.get("icon") == Some(&json!("mdi:account"))
                    && state.attributes.get("device_class") == Some(&json!("humidity"))
            })
        })
        .await;

        let mut refreshed = info();
        refreshed.original_name = Some("Provider refresh".into());
        refreshed.original_icon = Some("mdi:new".into());
        refreshed.original_device_class = Some("pressure".into());
        refreshed.unit_of_measurement = Some("kPa".into());
        registries
            .update_provider_entity_info(&entity_id, refreshed.clone())
            .unwrap();
        wait_until(|| {
            registries
                .provider_entity(handle.registry_id().unwrap())
                .is_some_and(|entry| entry.original_name.as_deref() == Some("Provider refresh"))
        })
        .await;
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.attributes["friendly_name"], "User name");
        assert_eq!(state.attributes["icon"], "mdi:account");
        assert_eq!(state.attributes["device_class"], "humidity");
        assert_eq!(state.attributes["unit_of_measurement"], "dynamic-unit");

        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    name: Some(None),
                    icon: Some(None),
                    device_class: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(&entity_id).is_some_and(|state| {
                state.attributes.get("friendly_name") == Some(&json!("Dynamic name"))
                    && state.attributes.get("icon") == Some(&json!("mdi:chart"))
                    && state.attributes.get("device_class") == Some(&json!("custom-class"))
            })
        })
        .await;

        let mut without_dynamic_metadata = EntitySnapshot::new("21");
        without_dynamic_metadata
            .attributes
            .insert("source".into(), json!("fixture"));
        handle.publish(without_dynamic_metadata).unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.attributes["friendly_name"], "Provider refresh");
        assert_eq!(state.attributes["icon"], "mdi:new");
        assert_eq!(state.attributes["device_class"], "pressure");
        assert_eq!(state.attributes["unit_of_measurement"], "kPa");
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_receipt_reentry_and_empty_provider_reconcile_clean_owned_state() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let spec = EntitySpec::new("sensor", "generated", "generated", "legacy-key")
            .with_suggested_object_id("generated-reading")
            .with_prior_entity_id("sensor.legacy_reading")
            .with_info(ProviderEntityInfo {
                original_name: Some("Generated".into()),
                ..ProviderEntityInfo::default()
            });
        let handle = lifecycle
            .register_provider_batch("generated", vec![spec.clone()])
            .unwrap()
            .remove(0);
        assert_eq!(
            handle.current_entity_id().as_deref(),
            Some("sensor.legacy_reading")
        );
        handle.publish(EntitySnapshot::new("5")).unwrap();
        handle.release().unwrap();
        assert!(runtime.get_state("sensor.legacy_reading").is_none());

        let reentered = lifecycle
            .register_provider_batch("generated", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(
            reentered.current_entity_id().as_deref(),
            Some("sensor.legacy_reading")
        );
        assert_eq!(
            runtime.get_state("sensor.legacy_reading").unwrap().state,
            "5"
        );

        lifecycle
            .register_provider_batch("generated", Vec::new())
            .unwrap();
        assert!(runtime.get_state("sensor.legacy_reading").is_none());
        assert!(registries.entities().is_empty());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn provider_receipts_are_namespaced_and_cleanup_only_owned_ids() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let first = EntitySpec::new("sensor", "provider_a", "provider_a", "same-key")
            .with_suggested_object_id("same-name");
        let second = EntitySpec::new("sensor", "provider_b", "provider_b", "same-key")
            .with_suggested_object_id("same-name");
        let first_handle = lifecycle
            .register_provider_batch("provider_a", vec![first])
            .unwrap()
            .remove(0);
        let second_handle = lifecycle
            .register_provider_batch("provider_b", vec![second])
            .unwrap()
            .remove(0);
        let first_id = first_handle.current_entity_id().unwrap();
        let second_id = second_handle.current_entity_id().unwrap();
        assert_ne!(first_id, second_id);
        first_handle.publish(EntitySnapshot::new("a")).unwrap();
        second_handle.publish(EntitySnapshot::new("b")).unwrap();

        lifecycle
            .register_provider_batch("provider_a", Vec::new())
            .unwrap();
        assert!(runtime.get_state(&first_id).is_none());
        assert_eq!(runtime.get_state(&second_id).unwrap().state, "b");
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registry_receipts_scope_cleanup_to_config_entry_provider_after_restart() {
        let (runtime, store) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let provider_a = "time_date:entry-a";
        let provider_b = "time_date:entry-b";
        let first = EntitySpec::new("sensor", "time_date", provider_a, "date")
            .with_unique_id("entry-a-date")
            .with_config_entry_id("entry-a")
            .with_suggested_object_id("date");
        let second = EntitySpec::new("sensor", "time_date", provider_b, "date")
            .with_unique_id("entry-b-date")
            .with_config_entry_id("entry-b")
            .with_suggested_object_id("date");
        let first_handle = lifecycle
            .register_provider_batch(provider_a, vec![first])
            .unwrap()
            .remove(0);
        let second_handle = lifecycle
            .register_provider_batch(provider_b, vec![second])
            .unwrap()
            .remove(0);
        let first_id = first_handle.current_entity_id().unwrap();
        let second_id = second_handle.current_entity_id().unwrap();
        first_handle.publish(EntitySnapshot::new("a")).unwrap();
        second_handle.publish(EntitySnapshot::new("b")).unwrap();

        lifecycle.shutdown().await.unwrap();
        drop(first_handle);
        drop(second_handle);
        drop(lifecycle);
        drop(registries);
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        lifecycle
            .register_provider_batch(provider_a, Vec::new())
            .unwrap();

        let first_state = runtime.get_state(&first_id).unwrap();
        assert_eq!(first_state.state, "unavailable");
        assert_eq!(
            first_state.attributes.get("restored"),
            Some(&Value::Bool(true))
        );
        assert_eq!(runtime.get_state(&second_id).unwrap().state, "b");
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registered_identity_and_state_survive_manager_restart() {
        let (runtime, store) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let spec = registered_spec("fixture");
        let handle = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("21")).unwrap();
        let registry_id = handle.registry_id().unwrap().to_owned();
        lifecycle.shutdown().await.unwrap();
        drop(handle);
        drop(lifecycle);
        drop(registries);
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let restored = lifecycle
            .register_provider_batch("fixture", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(
            restored.current_entity_id().as_deref(),
            Some(entity_id.as_str())
        );
        assert_eq!(restored.registry_id(), Some(registry_id.as_str()));
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "21");
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unrecorded_attributes_survive_live_restore_and_are_filtered_from_history() {
        let (runtime, store) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let spec = registered_spec("fixture").with_unrecorded_attributes(["tag_id"]);
        let handle = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        let mut snapshot = EntitySnapshot::new("scanned");
        snapshot
            .attributes
            .insert("tag_id".into(), Value::String("fixture-tag".into()));
        snapshot
            .attributes
            .insert("device_id".into(), Value::String("scanner-1".into()));
        let mut events = runtime.subscribe();

        handle.publish(snapshot).unwrap().unwrap();
        let published = runtime.get_state(&entity_id).unwrap();
        assert_eq!(published.attributes["tag_id"], "fixture-tag");
        assert_eq!(
            events.try_recv().unwrap().data["new_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );
        let first_history = store
            .history(
                &entity_id,
                chrono::DateTime::<chrono::Utc>::MIN_UTC,
                chrono::DateTime::<chrono::Utc>::MAX_UTC,
            )
            .unwrap();
        assert_eq!(first_history.len(), 1);
        assert!(!first_history[0].attributes.contains_key("tag_id"));
        assert_eq!(first_history[0].attributes["device_id"], "scanner-1");

        drop(handle);
        let unavailable = runtime.get_state(&entity_id).unwrap();
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(
            events.try_recv().unwrap().data["old_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );
        assert!(
            !store
                .history(
                    &entity_id,
                    chrono::DateTime::<chrono::Utc>::MIN_UTC,
                    chrono::DateTime::<chrono::Utc>::MAX_UTC,
                )
                .unwrap()[1]
                .attributes
                .contains_key("tag_id")
        );

        let restored = lifecycle
            .register_provider_batch("fixture", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(
            runtime.get_state(&entity_id).unwrap().attributes["tag_id"],
            "fixture-tag"
        );
        assert_eq!(
            events.try_recv().unwrap().data["new_state"]["attributes"]["tag_id"],
            "fixture-tag"
        );
        let history = store
            .history(
                &entity_id,
                chrono::DateTime::<chrono::Utc>::MIN_UTC,
                chrono::DateTime::<chrono::Utc>::MAX_UTC,
            )
            .unwrap();
        assert_eq!(history.len(), 3);
        assert!(
            history
                .iter()
                .all(|state| !state.attributes.contains_key("tag_id"))
        );
        lifecycle.shutdown().await.unwrap();
        drop(restored);
    }

    #[tokio::test]
    async fn dropping_last_registered_lease_restores_state_for_reentry() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let spec = registered_spec("fixture");
        let handle = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("21")).unwrap();

        drop(handle);
        let unavailable = runtime.get_state(&entity_id).unwrap();
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(
            unavailable.attributes.get("restored"),
            Some(&Value::Bool(true))
        );

        let reentered = lifecycle
            .register_provider_batch("fixture", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "21");
        lifecycle.shutdown().await.unwrap();
        drop(reentered);
    }

    #[tokio::test]
    async fn released_placeholder_follows_registry_disable_and_preserves_foreign_state() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let spec = registered_spec("fixture");
        let handle = lifecycle
            .register_batch(vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("21")).unwrap();
        handle.release().unwrap();
        assert_eq!(
            runtime
                .get_state(&entity_id)
                .unwrap()
                .attributes
                .get("restored"),
            Some(&json!(true))
        );

        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("config_entry".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| runtime.get_state(&entity_id).is_none()).await;

        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        let reentered = lifecycle.register_batch(vec![spec]).unwrap().remove(0);
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "21");
        reentered.release().unwrap();
        let foreign = runtime
            .set_state(
                &entity_id,
                "external",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap()
            .state;
        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        lifecycle.inner.reconcile_all(&Context::default()).unwrap();
        assert_eq!(runtime.get_state(&entity_id), Some(foreign.clone()));
        lifecycle.shutdown().await.unwrap();
        assert_eq!(runtime.get_state(&entity_id), Some(foreign));
    }

    #[tokio::test]
    async fn batch_preflight_and_explicit_release_report_lease_errors() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let duplicate_error = lifecycle
            .register_batch(vec![registered_spec("fixture"), registered_spec("fixture")])
            .unwrap_err();
        assert!(
            duplicate_error
                .to_string()
                .contains("duplicate entity identity")
        );
        assert!(registries.entities().is_empty());

        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let clone = handle.clone();
        handle.publish(EntitySnapshot::new("on")).unwrap();
        handle.release().unwrap();
        assert!(clone.current_entity_id().is_none());
        assert!(matches!(clone.status(), EntityStatus::Released { .. }));
        assert!(clone.publish(EntitySnapshot::new("off")).is_err());
        assert_eq!(
            runtime.get_state("sensor.room_temperature").unwrap().state,
            "unavailable"
        );
        assert_eq!(registries.entities().len(), 1);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn default_disabled_registry_entries_preserve_later_user_enablement() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let spec = registered_spec("fixture").with_enabled_by_default(false);
        let handle = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        let id = handle.registry_id().unwrap().to_owned();
        let entry = registries.provider_entity(&id).unwrap();
        assert_eq!(entry.disabled_by.as_deref(), Some("integration"));
        assert!(handle.publish(EntitySnapshot::new("1")).unwrap().is_none());
        assert!(runtime.get_state(&entry.entity_id).is_none());
        registries
            .update_entity(
                &entry.entity_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        assert!(handle.publish(EntitySnapshot::new("2")).unwrap().is_some());
        let refreshed = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        assert_eq!(registries.provider_entity(&id).unwrap().disabled_by, None);
        assert!(
            refreshed
                .publish(EntitySnapshot::new("3"))
                .unwrap()
                .is_some()
        );
        assert_eq!(runtime.get_state(&entry.entity_id).unwrap().state, "3");
        registries.remove_entity(&entry.entity_id).unwrap();
        refreshed.publish(EntitySnapshot::new("ignored")).unwrap();
        let restored = lifecycle
            .register_provider_batch("fixture", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(restored.registry_id(), Some(id.as_str()));
        assert_eq!(registries.provider_entity(&id).unwrap().disabled_by, None);
        assert!(
            restored
                .publish(EntitySnapshot::new("4"))
                .unwrap()
                .is_some()
        );
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn setup_with_disabled_registry_entry_does_not_publish_state() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let entry = registries
            .get_or_create_provider_entities(vec![ProviderEntityRegistration::new(
                EntityRegistration::new("sensor", "fixture", "temperature-1"),
                info(),
            )])
            .unwrap()
            .remove(0);
        registries
            .update_entity(
                &entry.entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        assert!(matches!(handle.status(), EntityStatus::Disabled { .. }));
        assert!(handle.current_entity_id().is_none());
        assert!(handle.publish(EntitySnapshot::new("on")).unwrap().is_none());
        assert!(runtime.get_state(&entry.entity_id).is_none());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn post_commit_restore_failure_keeps_registry_receipt_for_empty_restart_cleanup() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let provider_id = "fixture:entry";
        let spec = registered_spec(provider_id);
        let handle = lifecycle
            .register_provider_batch(provider_id, vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        let registry_id = handle.registry_id().unwrap().to_owned();
        handle.publish(EntitySnapshot::new("17")).unwrap();
        handle.release().unwrap();
        lifecycle.shutdown().await.unwrap();
        drop(handle);
        drop(lifecycle);

        runtime.delete_data(REGISTRY_RECEIPTS_KEY).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let changed_spec = spec.with_info(ProviderEntityInfo {
            original_name: Some("Committed provider name".into()),
            ..info()
        });
        store.fail_next_put_state.store(true, Ordering::SeqCst);
        let error = lifecycle
            .register_provider_batch(provider_id, vec![changed_spec])
            .unwrap_err();
        assert!(error.to_string().contains("injected state write failure"));
        assert_eq!(
            registries
                .provider_entity(&registry_id)
                .unwrap()
                .original_name
                .as_deref(),
            Some("Committed provider name")
        );

        let identity = registry_identity_key_parts("sensor", "fixture", "temperature-1");
        let pending = runtime.get_data(REGISTRY_RECEIPTS_KEY).unwrap().unwrap();
        assert!(
            pending[provider_id]
                .as_array()
                .unwrap()
                .contains(&Value::String(identity))
        );
        runtime
            .set_state(
                &entity_id,
                "foreign replacement",
                Map::new(),
                Context::default(),
                true,
            )
            .unwrap();
        lifecycle.shutdown().await.unwrap();
        drop(lifecycle);

        let restarted = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        restarted
            .register_provider_batch(provider_id, Vec::new())
            .unwrap();
        assert_eq!(
            runtime.get_state(&entity_id).unwrap().state,
            "foreign replacement"
        );
        let remaining = runtime.get_data(REGISTRY_RECEIPTS_KEY).unwrap().unwrap();
        assert!(remaining.get(provider_id).is_none());
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_registry_event_compensation_keeps_provider_receipt() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let provider_id = "fixture:event-entry";
        let spec = registered_spec(provider_id);
        let handle = lifecycle
            .register_provider_batch(provider_id, vec![spec.clone()])
            .unwrap()
            .remove(0);
        let registry_id = handle.registry_id().unwrap().to_owned();
        handle.publish(EntitySnapshot::new("17")).unwrap();
        handle.release().unwrap();
        lifecycle.shutdown().await.unwrap();
        drop(handle);
        drop(lifecycle);

        runtime.delete_data(REGISTRY_RECEIPTS_KEY).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let changed_spec = spec.with_info(ProviderEntityInfo {
            original_name: Some("Event committed name".into()),
            ..info()
        });
        store.fail_next_append_event.store(true, Ordering::SeqCst);
        store.fail_registry_rollback.store(true, Ordering::SeqCst);
        let error = lifecycle
            .register_provider_batch(provider_id, vec![changed_spec])
            .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert_eq!(
            registries
                .provider_entity(&registry_id)
                .unwrap()
                .original_name
                .as_deref(),
            Some("Event committed name")
        );
        let receipt = runtime.get_data(REGISTRY_RECEIPTS_KEY).unwrap().unwrap();
        assert!(receipt.get(provider_id).is_some());
        lifecycle.shutdown().await.unwrap();

        let reconciler = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        reconciler
            .register_provider_batch(provider_id, Vec::new())
            .unwrap();
        let receipt = runtime.get_data(REGISTRY_RECEIPTS_KEY).unwrap().unwrap();
        assert!(receipt.get(provider_id).is_none());
        reconciler.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn state_cleanup_preserves_foreign_replacements() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();

        let registered = lifecycle
            .register_provider_batch("registered", vec![registered_spec("registered")])
            .unwrap()
            .remove(0);
        let registered_id = registered.current_entity_id().unwrap();
        registered
            .publish(EntitySnapshot::new("owned registered"))
            .unwrap();
        runtime
            .set_state(
                &registered_id,
                "foreign registered replacement",
                Map::new(),
                Context::default(),
                true,
            )
            .unwrap();
        registered.release().unwrap();
        assert_eq!(
            runtime.get_state(&registered_id).unwrap().state,
            "foreign registered replacement"
        );

        let generated = EntitySpec::new("sensor", "generated", "generated", "reading")
            .with_suggested_object_id("reading");
        let generated = lifecycle
            .register_provider_batch("generated", vec![generated])
            .unwrap()
            .remove(0);
        let generated_id = generated.current_entity_id().unwrap();
        generated
            .publish(EntitySnapshot::new("owned generated"))
            .unwrap();
        runtime
            .set_state(
                &generated_id,
                "foreign generated replacement",
                Map::new(),
                Context::default(),
                true,
            )
            .unwrap();
        lifecycle
            .register_provider_batch("generated", Vec::new())
            .unwrap();
        assert_eq!(
            runtime.get_state(&generated_id).unwrap().state,
            "foreign generated replacement"
        );
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_state_cleanup_propagates_remove_failure_and_retries() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch(
                "generated",
                vec![
                    EntitySpec::new("sensor", "generated", "generated", "reading")
                        .with_suggested_object_id("reading"),
                ],
            )
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("owned")).unwrap();

        store.fail_next_remove_state.store(true, Ordering::SeqCst);
        let error = lifecycle
            .register_provider_batch("generated", Vec::new())
            .unwrap_err();
        assert!(error.to_string().contains("injected state removal failure"));
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "owned");

        lifecycle
            .register_provider_batch("generated", Vec::new())
            .unwrap();
        assert!(runtime.get_state(&entity_id).is_none());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_provider_replacement_restores_the_previous_lease() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle
            .publish(EntitySnapshot::new("last good state"))
            .unwrap();

        store.fail_next_set_data.store(true, Ordering::SeqCst);
        let error = lifecycle
            .register_provider_batch(
                "fixture",
                vec![
                    EntitySpec::new("sensor", "fixture", "fixture", "generated")
                        .with_suggested_object_id("generated"),
                ],
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected metadata write failure")
        );
        assert!(matches!(handle.status(), EntityStatus::Active { .. }));
        assert_eq!(
            runtime.get_state(&entity_id).unwrap().state,
            "last good state"
        );
        handle
            .publish(EntitySnapshot::new("old lease still works"))
            .unwrap();
        assert_eq!(
            runtime.get_state(&entity_id).unwrap().state,
            "old lease still works"
        );

        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_release_remove_failure_keeps_owned_lease_retryable() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch(
                "generated",
                vec![
                    EntitySpec::new("sensor", "generated", "generated", "reading")
                        .with_suggested_object_id("reading"),
                ],
            )
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("owned")).unwrap();

        store.fail_next_remove_state.store(true, Ordering::SeqCst);
        let error = handle.release().unwrap_err();
        assert!(error.to_string().contains("injected state removal failure"));
        assert!(matches!(handle.status(), EntityStatus::Active { .. }));
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "owned");

        handle.release().unwrap();
        assert!(runtime.get_state(&entity_id).is_none());
        assert!(matches!(handle.status(), EntityStatus::Released { .. }));
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rename_cleanup_preserves_foreign_state_and_rolls_back_on_store_error() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let registry_id = handle.registry_id().unwrap().to_owned();
        let old_id = handle.current_entity_id().unwrap();
        handle
            .publish(EntitySnapshot::new("provider value"))
            .unwrap();
        let mut renamed = registries.provider_entity(&registry_id).unwrap();
        renamed.entity_id = "sensor.renamed_temperature".into();
        {
            let mut records = lock(&lifecycle.inner.records);
            let record = records.get_mut(&handle.lease.receipt_key).unwrap();
            store.fail_next_remove_state.store(true, Ordering::SeqCst);
            let error = lifecycle
                .inner
                .move_state_if_renamed(record, &renamed)
                .unwrap_err();
            assert!(error.to_string().contains("injected state removal failure"));
        }
        assert_eq!(runtime.get_state(&old_id).unwrap().state, "provider value");
        assert!(runtime.get_state("sensor.renamed_temperature").is_none());

        let current = registries.provider_entity(&registry_id).unwrap();
        let mut foreign_rename = current;
        foreign_rename.entity_id = "sensor.foreign_rename".into();
        runtime
            .set_state(
                &foreign_rename.entity_id,
                "foreign target state",
                Map::new(),
                Context::default(),
                true,
            )
            .unwrap();
        {
            let mut records = lock(&lifecycle.inner.records);
            let record = records.get_mut(&handle.lease.receipt_key).unwrap();
            let error = lifecycle
                .inner
                .move_state_if_renamed(record, &foreign_rename)
                .unwrap_err();
            assert!(error.to_string().contains("already has a runtime state"));
        }
        assert_eq!(runtime.get_state(&old_id).unwrap().state, "provider value");
        assert_eq!(
            runtime.get_state(&foreign_rename.entity_id).unwrap().state,
            "foreign target state"
        );
        runtime
            .remove_state(&foreign_rename.entity_id, Context::default())
            .unwrap();
        runtime
            .set_state(
                &old_id,
                "foreign old state",
                Map::new(),
                Context::default(),
                true,
            )
            .unwrap();
        {
            let mut records = lock(&lifecycle.inner.records);
            let record = records.get_mut(&handle.lease.receipt_key).unwrap();
            lifecycle
                .inner
                .move_state_if_renamed(record, &foreign_rename)
                .unwrap();
        }
        assert_eq!(
            runtime.get_state(&old_id).unwrap().state,
            "foreign old state"
        );
        assert_eq!(
            runtime.get_state("sensor.foreign_rename").unwrap().state,
            "provider value"
        );
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pending_drop_release_is_discarded_when_the_identity_reenters() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let spec = registered_spec("fixture");
        let handle = lifecycle
            .register_provider_batch("fixture", vec![spec.clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("before drop")).unwrap();

        store.fail_next_set_data.store(true, Ordering::SeqCst);
        drop(handle);
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "before drop");

        let reentered = lifecycle
            .register_provider_batch("fixture", vec![spec])
            .unwrap()
            .remove(0);
        assert_eq!(
            reentered.current_entity_id().as_deref(),
            Some(entity_id.as_str())
        );
        reentered
            .publish(EntitySnapshot::new("after reentry"))
            .unwrap();
        assert!(lifecycle.inner.retry_pending_releases(true).is_empty());
        lifecycle.shutdown().await.unwrap();
        assert_eq!(
            runtime.get_state(&entity_id).unwrap().state,
            "after reentry"
        );
    }

    #[tokio::test]
    async fn dropped_release_retries_transient_storage_failure() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("before drop")).unwrap();

        store.fail_next_set_data.store(true, Ordering::SeqCst);
        drop(handle);
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "before drop");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if runtime.get_state(&entity_id).is_some_and(|state| {
                    state.state == "unavailable"
                        && state.attributes.get("restored") == Some(&Value::Bool(true))
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pending lease cleanup did not retry");
        assert!(lock(&lifecycle.inner.pending_releases).is_empty());
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn persistent_drop_release_failure_is_reported_at_shutdown() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle.publish(EntitySnapshot::new("before drop")).unwrap();

        store.fail_all_set_data.store(true, Ordering::SeqCst);
        drop(handle);
        let error = lifecycle.shutdown().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected metadata write failure")
        );
        assert_eq!(runtime.get_state(&entity_id).unwrap().state, "before drop");
    }

    #[tokio::test]
    async fn live_attributes_publish_offline_and_follow_registry_lifecycle() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    name: Some(Some("Registry name".into())),
                    icon: Some(Some("mdi:account".into())),
                    device_class: Some(Some("humidity".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();

        let context = Context {
            id: "live-publication-context".into(),
            parent_id: Some("live-publication-parent".into()),
            user_id: Some("live-publication-user".into()),
        };
        let mut snapshot = EntitySnapshot::unavailable();
        snapshot.context = context.clone();
        snapshot
            .attributes
            .insert("ordinary_extra".into(), json!("must be omitted"));
        let publication = EntityPublication::from(snapshot)
            .with_live_attribute("assumed_state", Some(json!(true)))
            .with_live_attribute("supported_features", Some(json!(9)))
            .with_live_attribute("unit_of_measurement", Some(json!("live-unit")))
            .with_live_attribute("friendly_name", Some(json!("Dynamic name")))
            .with_live_attribute("icon", Some(json!("mdi:radio")))
            .with_live_attribute("device_class", Some(json!("custom-class")));
        let change = handle.publish_publication(publication).unwrap().unwrap();

        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "unavailable");
        assert_eq!(state.context, context);
        assert_eq!(change.state.context, state.context);
        assert_eq!(state.attributes.get("assumed_state"), Some(&json!(true)));
        assert_eq!(state.attributes.get("supported_features"), Some(&json!(9)));
        assert_eq!(
            state.attributes.get("unit_of_measurement"),
            Some(&json!("live-unit"))
        );
        assert_eq!(
            state.attributes.get("friendly_name"),
            Some(&json!("Registry name"))
        );
        assert_eq!(state.attributes.get("icon"), Some(&json!("mdi:account")));
        assert_eq!(
            state.attributes.get("device_class"),
            Some(&json!("humidity"))
        );
        assert!(!state.attributes.contains_key("ordinary_extra"));

        let renamed_id = "sensor.live_attributes_renamed";
        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    new_entity_id: Some(renamed_id.into()),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(renamed_id).is_some()
                && runtime.get_state(&entity_id).is_none()
                && handle.current_entity_id().as_deref() == Some(renamed_id)
        })
        .await;
        assert_eq!(
            runtime
                .get_state(renamed_id)
                .unwrap()
                .attributes
                .get("assumed_state"),
            Some(&json!(true))
        );

        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(renamed_id).is_none()
                && matches!(handle.status(), EntityStatus::Disabled { .. })
        })
        .await;
        registries
            .update_entity(
                renamed_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        wait_until(|| {
            runtime.get_state(renamed_id).is_some()
                && matches!(handle.status(), EntityStatus::Active { .. })
        })
        .await;
        let reenabled = runtime.get_state(renamed_id).unwrap();
        assert_eq!(reenabled.state, "unavailable");
        assert_eq!(reenabled.context, context);
        assert_eq!(
            reenabled.attributes.get("assumed_state"),
            Some(&json!(true))
        );
        assert_eq!(
            reenabled.attributes.get("friendly_name"),
            Some(&json!("Registry name"))
        );
        assert!(!reenabled.attributes.contains_key("ordinary_extra"));

        handle.release().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn live_attribute_removal_and_null_survive_sqlite_release_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("entity-lifecycle.sqlite");
        let provider_id = "fixture:live-attributes";
        let info = ProviderEntityInfo {
            supported_features: 37,
            ..info()
        };
        let spec = registered_spec(provider_id).with_info(info);
        let entity_id;
        let context = Context {
            id: "sqlite-live-context".into(),
            parent_id: Some("sqlite-live-parent".into()),
            user_id: None,
        };

        {
            let store = Arc::new(hass_storage::SqliteStore::open(&database).unwrap());
            let runtime = Runtime::new(store).unwrap();
            let registries = Registries::new(runtime.clone()).unwrap();
            let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
            let handle = lifecycle
                .register_provider_batch(provider_id, vec![spec.clone()])
                .unwrap()
                .remove(0);
            entity_id = handle.current_entity_id().unwrap();
            let mut snapshot = EntitySnapshot::new("active");
            snapshot.context = context.clone();
            let publication = EntityPublication::from(snapshot)
                .with_live_attribute("explicit_null", Some(Value::Null))
                .with_live_attribute("unit_of_measurement", None)
                .with_live_attribute("supported_features", None)
                .with_live_attribute("friendly_name", None)
                .with_live_attribute("icon", None)
                .with_live_attribute("device_class", None);
            handle.publish_publication(publication).unwrap().unwrap();
            let active = runtime.get_state(&entity_id).unwrap();
            assert_eq!(active.attributes.get("explicit_null"), Some(&Value::Null));
            for name in [
                "unit_of_measurement",
                "supported_features",
                "friendly_name",
                "icon",
                "device_class",
            ] {
                assert!(
                    !active.attributes.contains_key(name),
                    "{name} should be removed"
                );
            }

            handle.release().unwrap();
            let unavailable = runtime.get_state(&entity_id).unwrap();
            assert_eq!(unavailable.state, "unavailable");
            assert_eq!(unavailable.context, context);
            assert_eq!(
                unavailable.attributes.get("explicit_null"),
                Some(&Value::Null)
            );
            assert_eq!(unavailable.attributes.get("restored"), Some(&json!(true)));
            for name in [
                "unit_of_measurement",
                "supported_features",
                "friendly_name",
                "icon",
                "device_class",
            ] {
                assert!(
                    !unavailable.attributes.contains_key(name),
                    "{name} should stay removed"
                );
            }
            lifecycle.shutdown().await.unwrap();
        }

        {
            let store = Arc::new(hass_storage::SqliteStore::open(&database).unwrap());
            let runtime = Runtime::new(store).unwrap();
            let registries = Registries::new(runtime.clone()).unwrap();
            let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
            let handle = lifecycle
                .register_provider_batch(provider_id, vec![spec])
                .unwrap()
                .remove(0);
            assert_eq!(
                handle.current_entity_id().as_deref(),
                Some(entity_id.as_str())
            );
            let restored = runtime.get_state(&entity_id).unwrap();
            assert_eq!(restored.state, "active");
            assert_eq!(restored.context, context);
            assert_eq!(restored.attributes.get("explicit_null"), Some(&Value::Null));
            for name in [
                "unit_of_measurement",
                "supported_features",
                "friendly_name",
                "icon",
                "device_class",
            ] {
                assert!(
                    !restored.attributes.contains_key(name),
                    "{name} should stay removed after reopen"
                );
            }
            handle.release().unwrap();
            let unavailable = runtime.get_state(&entity_id).unwrap();
            assert_eq!(unavailable.state, "unavailable");
            assert_eq!(
                unavailable.attributes.get("explicit_null"),
                Some(&Value::Null)
            );
            for name in [
                "unit_of_measurement",
                "supported_features",
                "friendly_name",
                "icon",
                "device_class",
            ] {
                assert!(
                    !unavailable.attributes.contains_key(name),
                    "{name} should stay removed after reentry"
                );
            }
            lifecycle.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_live_publication_keeps_the_previous_snapshot_and_attribute_map() {
        let (runtime, store) = fault_runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle
            .publish_publication(
                EntityPublication::from(EntitySnapshot::new("last good"))
                    .with_live_attribute("version", Some(json!(1))),
            )
            .unwrap();
        store.fail_next_put_state.store(true, Ordering::SeqCst);
        let error = handle
            .publish_publication(
                EntityPublication::from(EntitySnapshot::new("failed"))
                    .with_live_attribute("version", Some(json!(2))),
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected state write failure"));
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "last good");
        assert_eq!(state.attributes.get("version"), Some(&json!(1)));

        handle.release().unwrap();
        let unavailable = runtime.get_state(&entity_id).unwrap();
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(unavailable.attributes.get("version"), Some(&json!(1)));
        assert_eq!(unavailable.attributes.get("restored"), Some(&json!(true)));
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_publish_keeps_availability_scoped_attributes_and_empty_live_map() {
        let (runtime, _) = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let handle = lifecycle
            .register_provider_batch("fixture", vec![registered_spec("fixture")])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        handle
            .publish_publication(
                EntityPublication::from(EntitySnapshot::new("online"))
                    .with_live_attribute("live_only", Some(json!("present"))),
            )
            .unwrap();

        let mut unavailable = EntitySnapshot::unavailable();
        unavailable
            .attributes
            .insert("ordinary_extra".into(), json!("omitted"));
        handle.publish(unavailable).unwrap();
        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, "unavailable");
        assert_eq!(
            state.attributes.get("friendly_name"),
            Some(&json!("Temperature"))
        );
        assert_eq!(
            state.attributes.get("icon"),
            Some(&json!("mdi:thermometer"))
        );
        assert_eq!(
            state.attributes.get("device_class"),
            Some(&json!("temperature"))
        );
        assert_eq!(
            state.attributes.get("unit_of_measurement"),
            Some(&json!("°C"))
        );
        assert!(!state.attributes.contains_key("live_only"));
        assert!(!state.attributes.contains_key("ordinary_extra"));

        handle.release().unwrap();
        lifecycle.shutdown().await.unwrap();
    }
}
