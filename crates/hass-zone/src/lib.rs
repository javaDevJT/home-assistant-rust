//! Native Home Assistant zone behavior.
//!
//! This crate provides YAML and storage-backed zone management, entity state
//! publication, occupancy attributes, and WGS-84 location calculations. It
//! does not access device trackers or external location providers directly.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use hass_core::{Context, Error, Event, Result, Runtime, State};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

pub const DEFAULT_ZONE_RADIUS: f64 = 100.0;
/// Private runtime signal carrying persisted zone collection changes.
pub const ZONE_COLLECTION_UPDATED_SIGNAL: &str = "zone_collection_updated";
const STORAGE_KEY: &str = "hass-zone.collection.v1";
const STORAGE_VERSION: u32 = 1;
const HOME_ENTITY_ID: &str = "zone.home";
const HOME_ICON: &str = "mdi:home";

/// One named zone supplied by YAML or created through storage CRUD.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default = "default_radius")]
    pub radius: f64,
    #[serde(default)]
    pub passive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl ZoneConfig {
    /// Validate coordinates and fields using the constraints needed by the
    /// pinned Home Assistant zone schema.
    pub fn validate(&self) -> Result<()> {
        validate_config(self)
    }
}

/// Home location and YAML zones consumed by the native integration.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeZoneConfig {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub zones: Vec<ZoneConfig>,
}

impl HomeZoneConfig {
    /// Validate the implicit home zone and all YAML zones before startup.
    pub fn validate(&self) -> Result<()> {
        validate_location(&self.name, self.latitude, self.longitude)?;
        for zone in &self.zones {
            zone.validate()?;
        }
        Ok(())
    }
}

/// A zone in the editable persisted collection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredZone {
    pub id: String,
    #[serde(flatten)]
    pub config: ZoneConfig,
}

/// Partial update accepted for a stored zone.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Match set returned by zone geometry queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoneMatches {
    /// Smallest active containing zone, or `None` when none matches.
    pub active_zone: Option<String>,
    /// Every containing zone, ordered by radius and then center distance.
    pub zones: Vec<String>,
}

/// Owning lifecycle for a native zone integration instance.
pub struct ZoneIntegration {
    manager: ZoneManager,
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl ZoneIntegration {
    /// Load metadata, publish zone states, and start tracking person and
    /// device-tracker state changes.
    pub async fn start(runtime: Runtime, config: HomeZoneConfig) -> Result<Self> {
        start_integration(runtime, config).await
    }

    /// Return a typed manager handle for CRUD, reload, and location queries.
    pub fn manager(&self) -> ZoneManager {
        self.manager.clone()
    }

    /// Stop state tracking and invalidate outstanding manager callbacks.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shared.active.store(false, Ordering::Release);
        self.shutdown_tx.send_replace(true);
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|error| Error::Storage(format!("zone worker failed: {error}")))??;
        }
        let _quiesce = lock(&self.shared.data);
        Ok(())
    }
}

impl Drop for ZoneIntegration {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
        // Dropping the sender wakes the owned event worker, which observes the
        // closed watch channel and exits without retaining a strong manager.
    }
}

/// Typed manager for persisted zones, YAML reloads, home updates, and geometry.
#[derive(Clone)]
pub struct ZoneManager {
    shared: Weak<Shared>,
}

impl ZoneManager {
    /// Return stored zones in stable ID order.
    pub fn list(&self) -> Result<Vec<StoredZone>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data
            .storage
            .iter()
            .map(|(id, zone)| StoredZone {
                id: id.clone(),
                config: zone.config.clone(),
            })
            .collect())
    }

    /// Return one stored zone by its collection ID.
    pub fn get(&self, id: &str) -> Result<Option<StoredZone>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.storage.get(id).map(|zone| StoredZone {
            id: id.to_owned(),
            config: zone.config.clone(),
        }))
    }

    /// Create a storage-backed editable zone and publish its state.
    pub fn create(&self, config: ZoneConfig) -> Result<StoredZone> {
        let shared = self.live()?;
        config.validate()?;
        create_stored_zone(&shared, config)
    }

    /// Update a storage-backed zone without changing its collection ID.
    pub fn update(&self, id: &str, update: ZoneUpdate) -> Result<StoredZone> {
        let shared = self.live()?;
        update_stored_zone(&shared, id, update)
    }

    /// Remove a stored zone and its current entity state.
    pub fn delete(&self, id: &str) -> Result<()> {
        let shared = self.live()?;
        delete_stored_zone(&shared, id)
    }

    /// Replace YAML-defined zones while preserving storage-backed zones.
    pub fn reload(&self, zones: Vec<ZoneConfig>) -> Result<()> {
        let shared = self.live()?;
        reload_yaml_zones(&shared, zones)
    }

    /// Update the implicit home zone after a core configuration change.
    pub fn update_home(
        &self,
        name: impl Into<String>,
        latitude: f64,
        longitude: f64,
    ) -> Result<()> {
        let shared = self.live()?;
        update_home(&shared, name.into(), latitude, longitude)
    }

    /// Find every zone containing the point and its smallest active zone.
    pub fn zones_containing(
        &self,
        latitude: f64,
        longitude: f64,
        radius: f64,
    ) -> Result<ZoneMatches> {
        let shared = self.live()?;
        zone_matches(&shared.runtime, latitude, longitude, radius)
    }

    /// Return whether the point and radius are inside one available zone.
    pub fn in_zone(
        &self,
        entity_id: &str,
        latitude: f64,
        longitude: f64,
        radius: f64,
    ) -> Result<bool> {
        let shared = self.live()?;
        let Some(zone) = shared.runtime.get_state(entity_id) else {
            return Ok(false);
        };
        Ok(state_contains(&zone, latitude, longitude, radius))
    }

    /// Return every available zone that fully encloses the named zone.
    pub fn enclosing_zones(&self, entity_id: &str) -> Result<Vec<String>> {
        let shared = self.live()?;
        enclosing_zones(&shared.runtime, entity_id)
    }

    /// Take an atomic storage snapshot and subscribe to later collection
    /// changes without retaining the integration's Runtime lease.
    pub fn snapshot_and_subscribe(&self) -> Result<(Vec<StoredZone>, broadcast::Receiver<Event>)> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let receiver = shared.runtime.subscribe_signals();
        let zones = data
            .storage
            .iter()
            .map(|(id, zone)| StoredZone {
                id: id.clone(),
                config: zone.config.clone(),
            })
            .collect();
        Ok((zones, receiver))
    }

    fn live(&self) -> Result<Arc<Shared>> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| Error::InvalidInput("zone integration is shut down".into()))?;
        if shared.active.load(Ordering::Acquire) {
            Ok(shared)
        } else {
            Err(Error::InvalidInput("zone integration is shut down".into()))
        }
    }

    fn ensure_active(shared: &Shared) -> Result<()> {
        if shared.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::InvalidInput("zone integration is shut down".into()))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedZones {
    version: u32,
    #[serde(default)]
    items: Vec<StoredZone>,
    #[serde(default)]
    owned_entity_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct ZoneRuntime {
    entity_id: String,
    config: ZoneConfig,
    editable: bool,
    persons: BTreeSet<String>,
    device_trackers: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct ManagerData {
    home: ZoneRuntime,
    yaml: Vec<ZoneRuntime>,
    storage: BTreeMap<String, ZoneRuntime>,
    owned_entity_ids: BTreeSet<String>,
}

impl ManagerData {
    fn runtime_zones(&self) -> Vec<ZoneRuntime> {
        let mut zones = Vec::with_capacity(1 + self.yaml.len() + self.storage.len());
        zones.push(self.home.clone());
        zones.extend(self.yaml.iter().cloned());
        zones.extend(self.storage.values().cloned());
        zones.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        zones
    }

    fn refresh_owned_entity_ids(&mut self) {
        self.owned_entity_ids = self
            .runtime_zones()
            .into_iter()
            .map(|zone| zone.entity_id)
            .collect();
    }
}

struct Shared {
    runtime: Runtime,
    data: Mutex<ManagerData>,
    active: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn default_radius() -> f64 {
    DEFAULT_ZONE_RADIUS
}

fn validate_config(config: &ZoneConfig) -> Result<()> {
    validate_location(&config.name, config.latitude, config.longitude)?;
    if !config.radius.is_finite() {
        return Err(Error::InvalidInput("zone radius must be finite".into()));
    }
    if let Some(icon) = &config.icon
        && icon.trim().is_empty()
    {
        return Err(Error::InvalidInput("zone icon must not be empty".into()));
    }
    Ok(())
}

fn validate_location(name: &str, latitude: f64, longitude: f64) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::InvalidInput("zone name must not be empty".into()));
    }
    if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
        return Err(Error::InvalidInput(
            "latitude must be between -90 and 90".into(),
        ));
    }
    if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
        return Err(Error::InvalidInput(
            "longitude must be between -180 and 180".into(),
        ));
    }
    Ok(())
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_separator = false;
        } else if !last_separator && !slug.is_empty() {
            slug.push('_');
            last_separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "zone".to_owned()
    } else {
        slug
    }
}

async fn start_integration(runtime: Runtime, config: HomeZoneConfig) -> Result<ZoneIntegration> {
    config.validate()?;
    let (initial_states, event_receiver) = runtime.subscribe_with_states();
    let (persisted, old_metadata) = load_persisted_zones(&runtime)?;
    let mut previous_owned: BTreeSet<String> = persisted.owned_entity_ids.iter().cloned().collect();
    previous_owned.extend(
        persisted
            .items
            .iter()
            .map(|item| format!("zone.{}", item.id)),
    );
    previous_owned.insert(HOME_ENTITY_ID.to_owned());
    let foreign_zone_ids = foreign_zone_entity_ids(&initial_states, &previous_owned);
    let storage = stored_runtimes(persisted.items)?;
    let home = home_runtime(&config);
    let yaml = yaml_runtimes(&config.zones, &storage, &foreign_zone_ids)?;
    ensure_unique_entity_ids(&home, &yaml, &storage)?;
    let mut data = ManagerData {
        home,
        yaml,
        storage,
        owned_entity_ids: BTreeSet::new(),
    };
    recompute_memberships(&mut data, &initial_states);
    data.refresh_owned_entity_ids();
    let stale = previous_owned
        .difference(&data.owned_entity_ids)
        .cloned()
        .collect::<Vec<_>>();
    persist_storage(&runtime, &data)?;
    if let Err(error) = publish_zone_set(&runtime, &stale, &data.runtime_zones()) {
        restore_metadata(&runtime, old_metadata.as_ref());
        return Err(error);
    }

    let shared = Arc::new(Shared {
        runtime,
        data: Mutex::new(data),
        active: AtomicBool::new(true),
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let manager = ZoneManager {
        shared: Arc::downgrade(&shared),
    };
    let worker = tokio::spawn(run_event_worker(
        Arc::downgrade(&shared),
        event_receiver,
        shutdown_rx,
    ));
    Ok(ZoneIntegration {
        manager,
        shared,
        shutdown_tx,
        worker: Some(worker),
    })
}

fn load_persisted_zones(runtime: &Runtime) -> Result<(PersistedZones, Option<Value>)> {
    let old_metadata = runtime.get_data(STORAGE_KEY)?;
    let collection = match old_metadata.clone() {
        Some(value) => serde_json::from_value(value)
            .map_err(|error| Error::Storage(format!("invalid zone metadata: {error}")))?,
        None => PersistedZones {
            version: STORAGE_VERSION,
            items: Vec::new(),
            owned_entity_ids: Vec::new(),
        },
    };
    if collection.version != STORAGE_VERSION {
        return Err(Error::Storage(format!(
            "unsupported zone metadata version {}",
            collection.version
        )));
    }
    let mut seen = BTreeSet::new();
    for item in &collection.items {
        validate_stored_zone(item)?;
        if !seen.insert(item.id.clone()) {
            return Err(Error::Storage(format!(
                "duplicate stored zone ID {:?}",
                item.id
            )));
        }
    }
    let mut owned = BTreeSet::new();
    for entity_id in &collection.owned_entity_ids {
        validate_owned_entity_id(entity_id)?;
        if !owned.insert(entity_id.clone()) {
            return Err(Error::Storage(format!(
                "duplicate owned zone entity ID {entity_id:?}"
            )));
        }
    }
    Ok((collection, old_metadata))
}

fn validate_owned_entity_id(entity_id: &str) -> Result<()> {
    let Some(object_id) = entity_id.strip_prefix("zone.") else {
        return Err(Error::Storage(format!(
            "invalid owned zone entity ID {entity_id:?}"
        )));
    };
    if object_id.is_empty()
        || !object_id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
    {
        return Err(Error::Storage(format!(
            "invalid owned zone entity ID {entity_id:?}"
        )));
    }
    Ok(())
}

fn stored_runtimes(items: Vec<StoredZone>) -> Result<BTreeMap<String, ZoneRuntime>> {
    let mut storage = BTreeMap::new();
    for item in items {
        validate_stored_zone(&item)?;
        let entity_id = format!("zone.{}", item.id);
        storage.insert(
            item.id,
            ZoneRuntime {
                entity_id,
                config: item.config,
                editable: true,
                persons: BTreeSet::new(),
                device_trackers: BTreeSet::new(),
            },
        );
    }
    Ok(storage)
}

fn validate_stored_zone(item: &StoredZone) -> Result<()> {
    if item.id.is_empty()
        || !item.id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
    {
        return Err(Error::InvalidInput(format!(
            "invalid stored zone ID {:?}",
            item.id
        )));
    }
    item.config.validate()
}

fn home_runtime(config: &HomeZoneConfig) -> ZoneRuntime {
    ZoneRuntime {
        entity_id: HOME_ENTITY_ID.to_owned(),
        config: ZoneConfig {
            name: config.name.clone(),
            latitude: config.latitude,
            longitude: config.longitude,
            radius: DEFAULT_ZONE_RADIUS,
            passive: false,
            icon: Some(HOME_ICON.to_owned()),
        },
        // Zone.__init__ leaves the implicit home entity editable=true.
        editable: true,
        persons: BTreeSet::new(),
        device_trackers: BTreeSet::new(),
    }
}

fn yaml_runtimes(
    configs: &[ZoneConfig],
    storage: &BTreeMap<String, ZoneRuntime>,
    foreign_zone_ids: &BTreeSet<String>,
) -> Result<Vec<ZoneRuntime>> {
    let mut occupied = BTreeSet::from(["home".to_owned()]);
    occupied.extend(storage.keys().cloned());
    occupied.extend(
        foreign_zone_ids
            .iter()
            .filter_map(|entity_id| entity_id.strip_prefix("zone.").map(ToOwned::to_owned)),
    );
    let mut zones = Vec::with_capacity(configs.len());
    for config in configs {
        config.validate()?;
        let base = slugify(&config.name);
        let mut slug = base.clone();
        let mut suffix = 2_u32;
        while !occupied.insert(slug.clone()) {
            slug = format!("{base}_{suffix}");
            suffix += 1;
        }
        zones.push(ZoneRuntime {
            entity_id: format!("zone.{slug}"),
            config: config.clone(),
            editable: false,
            persons: BTreeSet::new(),
            device_trackers: BTreeSet::new(),
        });
    }
    Ok(zones)
}

fn foreign_zone_entity_ids(states: &[State], owned: &BTreeSet<String>) -> BTreeSet<String> {
    states
        .iter()
        .filter(|state| state.entity_id.starts_with("zone.") && !owned.contains(&state.entity_id))
        .map(|state| state.entity_id.clone())
        .collect()
}

fn ensure_unique_entity_ids(
    home: &ZoneRuntime,
    yaml: &[ZoneRuntime],
    storage: &BTreeMap<String, ZoneRuntime>,
) -> Result<()> {
    let mut ids = BTreeSet::from([home.entity_id.clone()]);
    for zone in yaml.iter().chain(storage.values()) {
        if !ids.insert(zone.entity_id.clone()) {
            return Err(Error::InvalidInput(format!(
                "duplicate zone entity ID {:?}",
                zone.entity_id
            )));
        }
    }
    Ok(())
}

fn recompute_memberships(data: &mut ManagerData, states: &[State]) {
    let mut zones = data.runtime_zones();
    for zone in &mut zones {
        zone.persons.clear();
        zone.device_trackers.clear();
    }
    for state in states {
        let is_person = state.entity_id.starts_with("person.");
        let is_tracker = state.entity_id.starts_with("device_tracker.");
        if !is_person && !is_tracker {
            continue;
        }
        let Some(in_zones) = state.attributes.get("in_zones").and_then(Value::as_array) else {
            continue;
        };
        for zone in &mut zones {
            if in_zones
                .iter()
                .any(|value| value.as_str() == Some(zone.entity_id.as_str()))
            {
                if is_person {
                    zone.persons.insert(state.entity_id.clone());
                } else {
                    zone.device_trackers.insert(state.entity_id.clone());
                }
            }
        }
    }
    assign_memberships(data, zones);
}

fn assign_memberships(data: &mut ManagerData, zones: Vec<ZoneRuntime>) {
    for zone in zones {
        if zone.entity_id == data.home.entity_id {
            data.home.persons = zone.persons;
            data.home.device_trackers = zone.device_trackers;
        } else if let Some(existing) = data
            .yaml
            .iter_mut()
            .find(|existing| existing.entity_id == zone.entity_id)
        {
            existing.persons = zone.persons;
            existing.device_trackers = zone.device_trackers;
        } else if let Some(existing) = data
            .storage
            .values_mut()
            .find(|existing| existing.entity_id == zone.entity_id)
        {
            existing.persons = zone.persons;
            existing.device_trackers = zone.device_trackers;
        }
    }
}

fn build_state(zone: &ZoneRuntime) -> (String, Map<String, Value>) {
    let mut attributes = Map::new();
    attributes.insert("latitude".to_owned(), json!(zone.config.latitude));
    attributes.insert("longitude".to_owned(), json!(zone.config.longitude));
    attributes.insert("radius".to_owned(), json!(zone.config.radius));
    attributes.insert("passive".to_owned(), json!(zone.config.passive));
    attributes.insert(
        "persons".to_owned(),
        json!(zone.persons.iter().collect::<Vec<_>>()),
    );
    attributes.insert(
        "device_trackers".to_owned(),
        json!(zone.device_trackers.iter().collect::<Vec<_>>()),
    );
    attributes.insert("editable".to_owned(), json!(zone.editable));
    attributes.insert("friendly_name".to_owned(), json!(zone.config.name));
    if let Some(icon) = &zone.config.icon {
        attributes.insert("icon".to_owned(), json!(icon));
    }
    (zone.persons.len().to_string(), attributes)
}

fn publish_zone_set(runtime: &Runtime, remove: &[String], zones: &[ZoneRuntime]) -> Result<()> {
    let mut before: BTreeMap<String, Option<State>> = BTreeMap::new();
    let mut touched = Vec::new();
    for zone in zones {
        before
            .entry(zone.entity_id.clone())
            .or_insert_with(|| runtime.get_state(&zone.entity_id));
        let (state, attributes) = build_state(zone);
        if let Err(error) = runtime.set_state(
            &zone.entity_id,
            &state,
            attributes,
            Context::default(),
            false,
        ) {
            restore_states(runtime, &before, &touched);
            return Err(error);
        }
        touched.push(zone.entity_id.clone());
    }
    for entity_id in remove {
        before
            .entry(entity_id.clone())
            .or_insert_with(|| runtime.get_state(entity_id));
        if let Err(error) = runtime.remove_state(entity_id, Context::default()) {
            restore_states(runtime, &before, &touched);
            return Err(error);
        }
        touched.push(entity_id.clone());
    }
    Ok(())
}

fn restore_states(runtime: &Runtime, before: &BTreeMap<String, Option<State>>, touched: &[String]) {
    for entity_id in touched.iter().rev() {
        match before.get(entity_id).and_then(Option::as_ref) {
            Some(state) => {
                let _ = runtime.set_state(
                    entity_id,
                    &state.state,
                    state.attributes.clone(),
                    state.context.clone(),
                    false,
                );
            }
            None => {
                let _ = runtime.remove_state(entity_id, Context::default());
            }
        }
    }
}

fn persist_storage(runtime: &Runtime, data: &ManagerData) -> Result<()> {
    let collection = PersistedZones {
        version: STORAGE_VERSION,
        items: data
            .storage
            .iter()
            .map(|(id, zone)| StoredZone {
                id: id.clone(),
                config: zone.config.clone(),
            })
            .collect(),
        owned_entity_ids: data.owned_entity_ids.iter().cloned().collect(),
    };
    let value = serde_json::to_value(collection)
        .map_err(|error| Error::Storage(format!("serialize zone metadata: {error}")))?;
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

fn refresh_memberships(shared: &Shared) -> Result<()> {
    let states = shared.runtime.states();
    let mut data = lock(&shared.data);
    let old_zones = data.runtime_zones();
    let mut next = data.clone();
    recompute_memberships(&mut next, &states);
    let new_zones = next.runtime_zones();
    let changed = changed_occupancy(&old_zones, &new_zones);
    publish_zone_set(&shared.runtime, &[], &changed)?;
    *data = next;
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
                    return Ok(());
                }
            }
            event = events.recv() => {
                let Some(shared) = shared.upgrade() else {
                    return Ok(());
                };
                match event {
                    Ok(event) => process_state_event(&shared, &event)?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        while !matches!(
                            events.try_recv(),
                            Err(broadcast::error::TryRecvError::Empty)
                                | Err(broadcast::error::TryRecvError::Closed)
                        ) {}
                        refresh_memberships(&shared)?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

fn process_state_event(shared: &Shared, event: &Event) -> Result<()> {
    if event.event_type != "state_changed" {
        return Ok(());
    }
    let Some(entity_id) = event.data.get("entity_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let is_person = entity_id.starts_with("person.");
    let is_tracker = entity_id.starts_with("device_tracker.");
    if !is_person && !is_tracker {
        return Ok(());
    }
    let new_attributes = event
        .data
        .get("new_state")
        .filter(|state| !state.is_null())
        .and_then(|state| state.get("attributes"));
    let in_zones = new_attributes
        .and_then(|attributes| attributes.get("in_zones"))
        .and_then(Value::as_array);

    let mut data = lock(&shared.data);
    let old_zones = data.runtime_zones();
    let mut new_zones = old_zones.clone();
    for zone in &mut new_zones {
        let contains = in_zones.is_some_and(|ids| {
            ids.iter()
                .any(|value| value.as_str() == Some(zone.entity_id.as_str()))
        });
        let members = if is_person {
            &mut zone.persons
        } else {
            &mut zone.device_trackers
        };
        if contains {
            members.insert(entity_id.to_owned());
        } else {
            members.remove(entity_id);
        }
    }
    let changed = changed_occupancy(&old_zones, &new_zones);
    if changed.is_empty() {
        return Ok(());
    }
    let mut next = data.clone();
    assign_memberships(&mut next, new_zones.clone());
    publish_zone_set(&shared.runtime, &[], &changed)?;
    *data = next;
    Ok(())
}

fn changed_occupancy(old: &[ZoneRuntime], new: &[ZoneRuntime]) -> Vec<ZoneRuntime> {
    new.iter()
        .filter(|zone| {
            old.iter()
                .find(|previous| previous.entity_id == zone.entity_id)
                .is_some_and(|previous| {
                    previous.persons != zone.persons
                        || previous.device_trackers != zone.device_trackers
                })
        })
        .cloned()
        .collect()
}

fn create_stored_zone(shared: &Shared, config: ZoneConfig) -> Result<StoredZone> {
    let mut data = lock(&shared.data);
    ZoneManager::ensure_active(shared)?;
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    let mut next = data.clone();
    let id = unique_storage_id(&config.name, &next, &shared.runtime);
    let entity_id = format!("zone.{id}");
    let new_zone = ZoneRuntime {
        entity_id: entity_id.clone(),
        config: config.clone(),
        editable: true,
        persons: BTreeSet::new(),
        device_trackers: BTreeSet::new(),
    };
    if next
        .runtime_zones()
        .iter()
        .any(|existing| existing.entity_id == entity_id)
    {
        return Err(Error::InvalidInput(format!(
            "zone entity ID {entity_id:?} already exists"
        )));
    }
    next.storage.insert(id.clone(), new_zone);
    recompute_memberships(&mut next, &shared.runtime.states());
    next.refresh_owned_entity_ids();
    let zone = next
        .storage
        .get(&id)
        .expect("the just-created zone is present")
        .clone();
    persist_storage(&shared.runtime, &next)?;
    if let Err(error) = publish_zone_set(&shared.runtime, &[], std::slice::from_ref(&zone)) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        return Err(error);
    }
    if let Err(error) = emit_collection_change(
        shared,
        "added",
        &id,
        Some(&StoredZone {
            id: id.clone(),
            config: config.clone(),
        }),
    ) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        let _ = shared.runtime.remove_state(&entity_id, Context::default());
        return Err(error);
    }
    *data = next;
    Ok(StoredZone { id, config })
}

fn update_stored_zone(shared: &Shared, id: &str, update: ZoneUpdate) -> Result<StoredZone> {
    let mut data = lock(&shared.data);
    ZoneManager::ensure_active(shared)?;
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    let mut next = data.clone();
    let updated_zone = {
        let zone = next
            .storage
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("zone {id}")))?;
        apply_zone_update(&mut zone.config, update)?;
        zone.clone()
    };
    next.refresh_owned_entity_ids();
    let config = updated_zone.config.clone();
    let old_state = shared.runtime.get_state(&updated_zone.entity_id);
    persist_storage(&shared.runtime, &next)?;
    if let Err(error) = publish_zone_set(&shared.runtime, &[], std::slice::from_ref(&updated_zone))
    {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        return Err(error);
    }
    let item = StoredZone {
        id: id.to_owned(),
        config: config.clone(),
    };
    if let Err(error) = emit_collection_change(shared, "updated", id, Some(&item)) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        if let Some(state) = old_state {
            let _ = shared.runtime.set_state(
                &state.entity_id,
                &state.state,
                state.attributes,
                state.context,
                false,
            );
        }
        return Err(error);
    }
    *data = next;
    Ok(item)
}

fn apply_zone_update(config: &mut ZoneConfig, update: ZoneUpdate) -> Result<()> {
    if let Some(name) = update.name {
        config.name = name;
    }
    if let Some(latitude) = update.latitude {
        config.latitude = latitude;
    }
    if let Some(longitude) = update.longitude {
        config.longitude = longitude;
    }
    if let Some(radius) = update.radius {
        config.radius = radius;
    }
    if let Some(passive) = update.passive {
        config.passive = passive;
    }
    if let Some(icon) = update.icon {
        config.icon = Some(icon);
    }
    config.validate()
}

fn delete_stored_zone(shared: &Shared, id: &str) -> Result<()> {
    let mut data = lock(&shared.data);
    ZoneManager::ensure_active(shared)?;
    let mut next = data.clone();
    let removed = next
        .storage
        .remove(id)
        .ok_or_else(|| Error::NotFound(format!("zone {id}")))?;
    let item = StoredZone {
        id: id.to_owned(),
        config: removed.config.clone(),
    };
    let entity_id = removed.entity_id.clone();
    next.refresh_owned_entity_ids();
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    let old_state = shared.runtime.get_state(&entity_id);
    persist_storage(&shared.runtime, &next)?;
    if let Err(error) = publish_zone_set(&shared.runtime, std::slice::from_ref(&entity_id), &[]) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        return Err(error);
    }
    if let Err(error) = emit_collection_change(shared, "removed", id, Some(&item)) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        if let Some(state) = old_state {
            let _ = shared.runtime.set_state(
                &state.entity_id,
                &state.state,
                state.attributes,
                state.context,
                false,
            );
        }
        return Err(error);
    }
    *data = next;
    Ok(())
}

fn reload_yaml_zones(shared: &Shared, configs: Vec<ZoneConfig>) -> Result<()> {
    for config in &configs {
        config.validate()?;
    }
    let mut data = lock(&shared.data);
    ZoneManager::ensure_active(shared)?;
    let old_metadata = shared.runtime.get_data(STORAGE_KEY)?;
    let mut next = data.clone();
    let current_states = shared.runtime.states();
    let foreign = foreign_zone_entity_ids(&current_states, &data.owned_entity_ids);
    next.yaml = yaml_runtimes(&configs, &next.storage, &foreign)?;
    ensure_unique_entity_ids(&next.home, &next.yaml, &next.storage)?;
    recompute_memberships(&mut next, &shared.runtime.states());
    next.refresh_owned_entity_ids();
    let removed = data
        .owned_entity_ids
        .difference(&next.owned_entity_ids)
        .cloned()
        .collect::<Vec<_>>();
    persist_storage(&shared.runtime, &next)?;
    if let Err(error) = publish_zone_set(&shared.runtime, &removed, &next.runtime_zones()) {
        restore_metadata(&shared.runtime, old_metadata.as_ref());
        return Err(error);
    }
    *data = next;
    Ok(())
}

fn update_home(shared: &Shared, name: String, latitude: f64, longitude: f64) -> Result<()> {
    validate_location(&name, latitude, longitude)?;
    let mut data = lock(&shared.data);
    ZoneManager::ensure_active(shared)?;
    let mut next = data.clone();
    next.home.config.name = name;
    next.home.config.latitude = latitude;
    next.home.config.longitude = longitude;
    publish_zone_set(&shared.runtime, &[], &[next.home.clone()])?;
    data.home = next.home;
    Ok(())
}

fn unique_storage_id(name: &str, data: &ManagerData, runtime: &Runtime) -> String {
    let base = slugify(name);
    let mut id = base.clone();
    let mut suffix = 2_u32;
    loop {
        let entity_id = format!("zone.{id}");
        let used = id == "home"
            || data.storage.contains_key(&id)
            || data.yaml.iter().any(|zone| zone.entity_id == entity_id)
            || data
                .storage
                .values()
                .any(|zone| zone.entity_id == entity_id)
            || (runtime.get_state(&entity_id).is_some()
                && !data.owned_entity_ids.contains(&entity_id));
        if !used {
            return id;
        }
        id = format!("{base}_{suffix}");
        suffix += 1;
    }
}

fn emit_collection_change(
    shared: &Shared,
    change_type: &str,
    id: &str,
    item: Option<&StoredZone>,
) -> Result<()> {
    let item = item
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| Error::Storage(format!("serialize zone collection item: {error}")))?;
    let change = json!({
        "change_type": change_type,
        "zone_id": id,
        "item": item,
    });
    let data = Map::from_iter([("changes".to_owned(), Value::Array(vec![change]))]);
    shared
        .runtime
        .dispatch_signal(ZONE_COLLECTION_UPDATED_SIGNAL, data, Context::default())?;
    Ok(())
}

fn zone_matches(
    runtime: &Runtime,
    latitude: f64,
    longitude: f64,
    radius: f64,
) -> Result<ZoneMatches> {
    if !radius.is_finite() {
        return Ok(ZoneMatches::default());
    }
    let mut matches = Vec::new();
    let mut active: Option<(State, f64, f64)> = None;
    for zone in runtime.states() {
        if !zone.entity_id.starts_with("zone.") || zone.state == "unavailable" {
            continue;
        }
        let Some(zone_latitude) = number_attribute(&zone, "latitude") else {
            continue;
        };
        let Some(zone_longitude) = number_attribute(&zone, "longitude") else {
            continue;
        };
        let Some(zone_radius) = number_attribute(&zone, "radius") else {
            continue;
        };
        let Some(zone_distance) =
            distance_meters(latitude, longitude, zone_latitude, zone_longitude)
        else {
            continue;
        };
        if zone_distance - zone_radius >= radius {
            continue;
        }
        matches.push((zone.entity_id.clone(), zone_radius, zone_distance));
        if zone
            .attributes
            .get("passive")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let replace = active
            .as_ref()
            .is_none_or(|(_, best_distance, best_radius)| {
                zone_radius < *best_radius
                    || (zone_radius == *best_radius && zone_distance < *best_distance)
            });
        if replace {
            active = Some((zone, zone_distance, zone_radius));
        }
    }
    matches.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.2.total_cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(ZoneMatches {
        active_zone: active.map(|(zone, _, _)| zone.entity_id),
        zones: matches
            .into_iter()
            .map(|(entity_id, _, _)| entity_id)
            .collect(),
    })
}

fn enclosing_zones(runtime: &Runtime, entity_id: &str) -> Result<Vec<String>> {
    let Some(input) = runtime.get_state(entity_id) else {
        return Ok(Vec::new());
    };
    if input.state == "unavailable" {
        return Ok(Vec::new());
    }
    let Some(input_latitude) = number_attribute(&input, "latitude") else {
        return Ok(Vec::new());
    };
    let Some(input_longitude) = number_attribute(&input, "longitude") else {
        return Ok(Vec::new());
    };
    let Some(input_radius) = number_attribute(&input, "radius") else {
        return Ok(Vec::new());
    };
    let mut matches = Vec::new();
    for zone in runtime.states() {
        if zone.entity_id == entity_id
            || !zone.entity_id.starts_with("zone.")
            || zone.state == "unavailable"
        {
            continue;
        }
        let Some(latitude) = number_attribute(&zone, "latitude") else {
            continue;
        };
        let Some(longitude) = number_attribute(&zone, "longitude") else {
            continue;
        };
        let Some(radius) = number_attribute(&zone, "radius") else {
            continue;
        };
        let Some(distance) = distance_meters(input_latitude, input_longitude, latitude, longitude)
        else {
            continue;
        };
        if distance + input_radius <= radius {
            matches.push((zone.entity_id, radius, distance));
        }
    }
    matches.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.2.total_cmp(&right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(matches
        .into_iter()
        .map(|(entity_id, _, _)| entity_id)
        .collect())
}

fn state_contains(zone: &State, latitude: f64, longitude: f64, radius: f64) -> bool {
    if zone.state == "unavailable" || !radius.is_finite() {
        return false;
    }
    let Some(zone_latitude) = number_attribute(zone, "latitude") else {
        return false;
    };
    let Some(zone_longitude) = number_attribute(zone, "longitude") else {
        return false;
    };
    let Some(zone_radius) = number_attribute(zone, "radius") else {
        return false;
    };
    distance_meters(latitude, longitude, zone_latitude, zone_longitude)
        .is_some_and(|distance| distance - radius < zone_radius)
}

fn number_attribute(state: &State, name: &str) -> Option<f64> {
    state
        .attributes
        .get(name)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> Option<f64> {
    if !lat1.is_finite()
        || !lon1.is_finite()
        || !lat2.is_finite()
        || !lon2.is_finite()
        || !(-90.0..=90.0).contains(&lat1)
        || !(-90.0..=90.0).contains(&lat2)
        || !(-180.0..=180.0).contains(&lon1)
        || !(-180.0..=180.0).contains(&lon2)
    {
        return None;
    }
    if lat1 == lat2 && lon1 == lon2 {
        return Some(0.0);
    }

    const AXIS_A: f64 = 6_378_137.0;
    const AXIS_B: f64 = 6_356_752.314_245;
    const FLATTENING: f64 = 1.0 / 298.257_223_563;
    const MAX_ITERATIONS: usize = 200;
    const CONVERGENCE_THRESHOLD: f64 = 1e-12;

    let reduced_latitude_1 = ((1.0 - FLATTENING) * lat1.to_radians().tan()).atan();
    let reduced_latitude_2 = ((1.0 - FLATTENING) * lat2.to_radians().tan()).atan();
    let longitude_difference = (lon2 - lon1).to_radians();
    let mut lambda = longitude_difference;
    let sin_u1 = reduced_latitude_1.sin();
    let cos_u1 = reduced_latitude_1.cos();
    let sin_u2 = reduced_latitude_2.sin();
    let cos_u2 = reduced_latitude_2.cos();
    let mut inverse = None;

    for _ in 0..MAX_ITERATIONS {
        let sin_lambda = lambda.sin();
        let cos_lambda = lambda.cos();
        let sin_sigma = ((cos_u2 * sin_lambda).powi(2)
            + (cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda).powi(2))
        .sqrt();
        if sin_sigma == 0.0 {
            return Some(0.0);
        }
        let cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
        let sigma = sin_sigma.atan2(cos_sigma);
        let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
        let cos_sq_alpha = 1.0 - sin_alpha.powi(2);
        let cos2_sigma_m = if cos_sq_alpha == 0.0 {
            0.0
        } else {
            cos_sigma - 2.0 * sin_u1 * sin_u2 / cos_sq_alpha
        };
        let coefficient =
            FLATTENING / 16.0 * cos_sq_alpha * (4.0 + FLATTENING * (4.0 - 3.0 * cos_sq_alpha));
        let previous_lambda = lambda;
        lambda = longitude_difference
            + (1.0 - coefficient)
                * FLATTENING
                * sin_alpha
                * (sigma
                    + coefficient
                        * sin_sigma
                        * (cos2_sigma_m
                            + coefficient * cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))));
        if (lambda - previous_lambda).abs() < CONVERGENCE_THRESHOLD {
            inverse = Some((sin_sigma, cos_sigma, sigma, cos_sq_alpha, cos2_sigma_m));
            break;
        }
    }
    let (sin_sigma, cos_sigma, sigma, cos_sq_alpha, cos2_sigma_m) = inverse?;

    let u_squared = cos_sq_alpha * (AXIS_A.powi(2) - AXIS_B.powi(2)) / AXIS_B.powi(2);
    let coefficient_a = 1.0
        + u_squared / 16_384.0
            * (4096.0 + u_squared * (-768.0 + u_squared * (320.0 - 175.0 * u_squared)));
    let coefficient_b =
        u_squared / 1024.0 * (256.0 + u_squared * (-128.0 + u_squared * (74.0 - 47.0 * u_squared)));
    let delta_sigma = coefficient_b
        * sin_sigma
        * (cos2_sigma_m
            + coefficient_b / 4.0
                * (cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))
                    - coefficient_b / 6.0
                        * cos2_sigma_m
                        * (-3.0 + 4.0 * sin_sigma.powi(2))
                        * (-3.0 + 4.0 * cos2_sigma_m.powi(2))));
    let distance_km = AXIS_B * coefficient_a * (sigma - delta_sigma) / 1000.0;
    Some((distance_km * 1_000_000.0).round_ties_even() / 1_000.0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hass_core::{Context, MemoryStore};
    use serde_json::{Map, json};

    use super::{
        DEFAULT_ZONE_RADIUS, HomeZoneConfig, ZONE_COLLECTION_UPDATED_SIGNAL, ZoneConfig,
        ZoneIntegration, ZoneUpdate, distance_meters,
    };

    fn runtime() -> hass_core::Runtime {
        hass_core::Runtime::new(Arc::new(MemoryStore::default())).expect("runtime")
    }

    fn home_config(zones: Vec<ZoneConfig>) -> HomeZoneConfig {
        HomeZoneConfig {
            name: "Home".to_owned(),
            latitude: 0.0,
            longitude: 0.0,
            zones,
        }
    }

    fn zone(name: &str, latitude: f64, longitude: f64, radius: f64) -> ZoneConfig {
        ZoneConfig {
            name: name.to_owned(),
            latitude,
            longitude,
            radius,
            passive: false,
            icon: None,
        }
    }

    fn attributes(value: serde_json::Value) -> Map<String, serde_json::Value> {
        value.as_object().expect("object attributes").clone()
    }

    fn set_membership(runtime: &hass_core::Runtime, entity_id: &str, zones: &[&str]) {
        runtime
            .set_state(
                entity_id,
                "home",
                attributes(json!({"in_zones": zones})),
                Context::default(),
                false,
            )
            .expect("set tracked state");
    }

    async fn wait_for_state(
        runtime: &hass_core::Runtime,
        entity_id: &str,
        predicate: impl Fn(&hass_core::State) -> bool,
    ) -> hass_core::State {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(state) = runtime.get_state(entity_id)
                    && predicate(&state)
                {
                    return state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("state change should arrive")
    }

    #[tokio::test]
    async fn startup_publishes_home_and_yaml_zone_attributes() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 1.0, 2.0, 250.0)]),
        )
        .await
        .expect("start");

        let home = runtime.get_state("zone.home").expect("home zone");
        assert_eq!(home.state, "0");
        assert_eq!(home.attributes["radius"], DEFAULT_ZONE_RADIUS);
        assert_eq!(home.attributes["passive"], false);
        assert_eq!(home.attributes["editable"], true);
        assert_eq!(home.attributes["icon"], "mdi:home");
        assert_eq!(home.attributes["friendly_name"], "Home");

        let work = runtime.get_state("zone.work").expect("yaml zone");
        assert_eq!(work.attributes["latitude"], 1.0);
        assert_eq!(work.attributes["longitude"], 2.0);
        assert_eq!(work.attributes["radius"], 250.0);
        assert_eq!(work.attributes["editable"], false);
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn invalid_home_config_fails_before_publishing_any_state() {
        let runtime = runtime();
        let config = HomeZoneConfig {
            name: "Home".to_owned(),
            latitude: 91.0,
            longitude: 0.0,
            zones: Vec::new(),
        };
        assert!(
            ZoneIntegration::start(runtime.clone(), config)
                .await
                .is_err()
        );
        assert!(runtime.get_state("zone.home").is_none());
    }

    #[tokio::test]
    async fn yaml_zone_validation_rejects_non_finite_radius_before_writes() {
        let runtime = runtime();
        let mut invalid = zone("Bad", 1.0, 1.0, 100.0);
        invalid.radius = f64::NAN;
        assert!(
            ZoneIntegration::start(runtime.clone(), home_config(vec![invalid]))
                .await
                .is_err()
        );
        assert!(runtime.get_state("zone.home").is_none());
    }

    #[tokio::test]
    async fn serde_zone_config_applies_upstream_radius_and_passive_defaults() {
        let config: ZoneConfig =
            serde_json::from_value(json!({"name":"Office","latitude":1,"longitude":2}))
                .expect("zone config defaults");
        assert_eq!(config.radius, DEFAULT_ZONE_RADIUS);
        assert!(!config.passive);
    }

    #[tokio::test]
    async fn startup_rejects_unknown_storage_version_without_state_mutation() {
        let runtime = runtime();
        runtime
            .set_data(
                "hass-zone.collection.v1",
                &json!({"version": 8, "items": []}),
            )
            .expect("seed metadata");
        assert!(
            ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
                .await
                .is_err()
        );
        assert!(runtime.get_state("zone.home").is_none());
    }

    #[tokio::test]
    async fn invalid_owned_entity_receipt_fails_before_runtime_state_mutation() {
        let runtime = runtime();
        let manual_state = json!({
            "latitude": 10.0,
            "longitude": 20.0,
            "radius": 100.0,
            "friendly_name": "Manual zone",
        });
        runtime
            .set_state(
                "zone.manual",
                "manual",
                attributes(manual_state.clone()),
                Context::default(),
                false,
            )
            .expect("seed foreign state");
        let malformed_metadata = json!({
            "version": 1,
            "items": [],
            "owned_entity_ids": ["sensor.not_a_zone"],
        });
        runtime
            .set_data("hass-zone.collection.v1", &malformed_metadata)
            .expect("seed malformed receipt");

        assert!(
            ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
                .await
                .is_err()
        );
        assert!(runtime.get_state("zone.home").is_none());
        assert_eq!(
            runtime.get_state("zone.manual").unwrap().attributes,
            attributes(manual_state)
        );
        assert_eq!(
            runtime.get_data("hass-zone.collection.v1").unwrap(),
            Some(malformed_metadata)
        );
    }

    #[tokio::test]
    async fn storage_crud_updates_and_removes_entity_state() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
            .await
            .expect("start");
        let manager = integration.manager();

        let created = manager
            .create(zone("Work", 1.0, 2.0, 300.0))
            .expect("create");
        assert_eq!(created.id, "work");
        assert_eq!(
            runtime.get_state("zone.work").unwrap().attributes["editable"],
            true
        );
        assert_eq!(manager.list().unwrap(), vec![created.clone()]);
        assert_eq!(manager.get("work").unwrap(), Some(created));
        assert!(
            runtime
                .get_data("hass-zone.collection.v1")
                .unwrap()
                .unwrap()["owned_entity_ids"]
                .as_array()
                .unwrap()
                .contains(&json!("zone.work"))
        );

        let updated = manager
            .update(
                "work",
                ZoneUpdate {
                    name: Some("Job".to_owned()),
                    latitude: Some(3.0),
                    radius: Some(400.0),
                    passive: Some(true),
                    ..ZoneUpdate::default()
                },
            )
            .expect("update");
        assert_eq!(updated.id, "work");
        assert_eq!(updated.config.name, "Job");
        let state = runtime.get_state("zone.work").expect("updated state");
        assert_eq!(state.attributes["friendly_name"], "Job");
        assert_eq!(state.attributes["latitude"], 3.0);
        assert_eq!(state.attributes["radius"], 400.0);
        assert_eq!(state.attributes["passive"], true);

        manager.delete("work").expect("delete");
        assert!(manager.list().unwrap().is_empty());
        assert!(runtime.get_state("zone.work").is_none());
        assert!(
            !runtime
                .get_data("hass-zone.collection.v1")
                .unwrap()
                .unwrap()["owned_entity_ids"]
                .as_array()
                .unwrap()
                .contains(&json!("zone.work"))
        );
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn storage_zone_metadata_survives_integration_restart() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
            .await
            .expect("start");
        integration
            .manager()
            .create(zone("Garage", 4.0, 5.0, 100.0))
            .expect("create");
        integration.shutdown().await.expect("shutdown");

        let restarted = ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
            .await
            .expect("restart");
        assert_eq!(restarted.manager().list().unwrap()[0].id, "garage");
        assert_eq!(
            runtime.get_state("zone.garage").unwrap().attributes["latitude"],
            4.0
        );
        restarted.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn yaml_reload_replaces_only_yaml_zones() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Office", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        let manager = integration.manager();
        manager
            .create(zone("Storage", 3.0, 4.0, 100.0))
            .expect("create storage");

        manager
            .reload(vec![zone("Cabin", 5.0, 6.0, 200.0)])
            .expect("reload YAML");
        assert!(runtime.get_state("zone.office").is_none());
        assert!(runtime.get_state("zone.cabin").is_some());
        assert!(runtime.get_state("zone.storage").is_some());
        assert_eq!(manager.list().unwrap().len(), 1);
        let metadata = runtime
            .get_data("hass-zone.collection.v1")
            .unwrap()
            .unwrap();
        let owned = metadata["owned_entity_ids"].as_array().unwrap();
        assert!(owned.contains(&json!("zone.home")));
        assert!(owned.contains(&json!("zone.cabin")));
        assert!(owned.contains(&json!("zone.storage")));
        assert!(!owned.contains(&json!("zone.office")));
        assert_eq!(metadata["items"].as_array().unwrap().len(), 1);
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn new_runtime_removes_stale_owned_yaml_and_preserves_foreign_and_stored_zones() {
        let store = Arc::new(MemoryStore::default());
        let manual_attributes = attributes(json!({
            "latitude": 20.0,
            "longitude": 30.0,
            "radius": 150.0,
            "friendly_name": "Manual zone",
        }));
        {
            let runtime = hass_core::Runtime::new(store.clone()).expect("first runtime");
            runtime
                .set_state(
                    "zone.manual",
                    "manual",
                    manual_attributes.clone(),
                    Context::default(),
                    false,
                )
                .expect("seed foreign zone");
            let integration = ZoneIntegration::start(
                runtime.clone(),
                home_config(vec![
                    zone("Obsolete", 1.0, 2.0, 100.0),
                    zone("Keep", 3.0, 4.0, 100.0),
                ]),
            )
            .await
            .expect("first start");
            integration
                .manager()
                .create(zone("Persistent", 5.0, 6.0, 200.0))
                .expect("create stored zone");
            integration.shutdown().await.expect("first shutdown");
        }

        let runtime = hass_core::Runtime::new(store).expect("second runtime");
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![
                zone("Keep", 30.0, 40.0, 300.0),
                zone("New", 50.0, 60.0, 400.0),
            ]),
        )
        .await
        .expect("second start");

        assert!(runtime.get_state("zone.obsolete").is_none());
        assert_eq!(
            runtime.get_state("zone.keep").unwrap().attributes["latitude"],
            30.0
        );
        assert_eq!(
            runtime.get_state("zone.keep").unwrap().attributes["longitude"],
            40.0
        );
        assert!(runtime.get_state("zone.new").is_some());
        assert!(runtime.get_state("zone.persistent").is_some());
        assert_eq!(
            runtime.get_state("zone.manual").unwrap().attributes,
            manual_attributes
        );
        assert_eq!(integration.manager().list().unwrap()[0].id, "persistent");
        integration.shutdown().await.expect("second shutdown");
    }

    #[tokio::test]
    async fn config_and_storage_zone_ids_do_not_overwrite_foreign_manual_zone_states() {
        let runtime = runtime();
        let manual_attributes = attributes(json!({
            "latitude": 20.0,
            "longitude": 30.0,
            "radius": 150.0,
            "friendly_name": "Manual zone",
        }));
        runtime
            .set_state(
                "zone.manual",
                "manual",
                manual_attributes.clone(),
                Context::default(),
                false,
            )
            .expect("seed foreign zone");
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Manual", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        assert_eq!(
            runtime.get_state("zone.manual").unwrap().attributes,
            manual_attributes
        );
        assert!(runtime.get_state("zone.manual_2").is_some());
        let stored = integration
            .manager()
            .create(zone("Manual", 3.0, 4.0, 100.0))
            .expect("create without replacing manual state");
        assert_eq!(stored.id, "manual_3");
        assert!(runtime.get_state("zone.manual_3").is_some());
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn update_home_changes_only_implicit_home_entity() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(runtime.clone(), home_config(Vec::new()))
            .await
            .expect("start");
        integration
            .manager()
            .update_home("New Home", 10.0, 20.0)
            .expect("update home");
        let home = runtime.get_state("zone.home").expect("home");
        assert_eq!(home.attributes["friendly_name"], "New Home");
        assert_eq!(home.attributes["latitude"], 10.0);
        assert_eq!(home.attributes["longitude"], 20.0);
        assert_eq!(home.attributes["radius"], DEFAULT_ZONE_RADIUS);
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn startup_tracks_preexisting_person_and_tracker_membership_separately() {
        let runtime = runtime();
        set_membership(&runtime, "person.alice", &["zone.home", "zone.work"]);
        set_membership(
            &runtime,
            "device_tracker.phone",
            &["zone.home", "zone.work"],
        );
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 1.0, 2.0, 500.0)]),
        )
        .await
        .expect("start");

        let work = runtime.get_state("zone.work").unwrap();
        assert_eq!(work.state, "1");
        assert_eq!(work.attributes["persons"], json!(["person.alice"]));
        assert_eq!(
            work.attributes["device_trackers"],
            json!(["device_tracker.phone"])
        );
        let home = runtime.get_state("zone.home").unwrap();
        assert_eq!(home.state, "1");
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn person_state_changes_update_person_count_from_in_zones() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        set_membership(&runtime, "person.alice", &["zone.work"]);
        let state = wait_for_state(&runtime, "zone.work", |state| state.state == "1").await;
        assert_eq!(state.attributes["persons"], json!(["person.alice"]));

        set_membership(&runtime, "person.alice", &[]);
        let state = wait_for_state(&runtime, "zone.work", |state| state.state == "0").await;
        assert_eq!(state.attributes["persons"], json!([]));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn tracker_changes_do_not_contribute_to_zone_state_count() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        set_membership(&runtime, "device_tracker.phone", &["zone.work"]);
        let state = wait_for_state(&runtime, "zone.work", |state| {
            state.attributes["device_trackers"] == json!(["device_tracker.phone"])
        })
        .await;
        assert_eq!(state.state, "0");
        assert_eq!(state.attributes["persons"], json!([]));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn removing_person_state_clears_zone_membership() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        set_membership(&runtime, "person.alice", &["zone.work"]);
        wait_for_state(&runtime, "zone.work", |state| state.state == "1").await;
        runtime
            .remove_state("person.alice", Context::default())
            .expect("remove person");
        let state = wait_for_state(&runtime, "zone.work", |state| state.state == "0").await;
        assert_eq!(state.attributes["persons"], json!([]));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn passive_zones_are_in_matches_but_not_active() {
        let runtime = runtime();
        let mut passive = zone("Passive", 0.0, 0.0, 200.0);
        passive.passive = true;
        let integration = ZoneIntegration::start(runtime.clone(), home_config(vec![passive]))
            .await
            .expect("start");
        let matches = integration
            .manager()
            .zones_containing(0.0, 0.0, 0.0)
            .expect("matches");
        assert_eq!(matches.active_zone.as_deref(), Some("zone.home"));
        assert!(matches.zones.contains(&"zone.passive".to_owned()));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn active_zone_prefers_smallest_zone_before_center_distance() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            HomeZoneConfig {
                name: "Home".to_owned(),
                latitude: 1.0,
                longitude: 1.0,
                zones: vec![zone("Small", 0.0, 0.0, 50.0), zone("Near", 0.0, 0.0, 200.0)],
            },
        )
        .await
        .expect("start");
        let matches = integration
            .manager()
            .zones_containing(0.0, 0.0, 500.0)
            .expect("matches");
        assert_eq!(matches.active_zone.as_deref(), Some("zone.small"));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn active_zone_uses_center_distance_for_equal_radii() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            HomeZoneConfig {
                name: "Home".to_owned(),
                latitude: 1.0,
                longitude: 1.0,
                zones: vec![
                    zone("Far", 0.0, 0.0005, 100.0),
                    zone("Near", 0.0, 0.0001, 100.0),
                ],
            },
        )
        .await
        .expect("start");
        let matches = integration
            .manager()
            .zones_containing(0.0, 0.0, 100.0)
            .expect("matches");
        assert_eq!(matches.active_zone.as_deref(), Some("zone.near"));
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn strict_containment_boundary_excludes_exact_edge() {
        let runtime = runtime();
        let edge_distance = distance_meters(0.0, 0.0, 0.0, 0.001).expect("distance");
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Edge", 0.0, 0.0, edge_distance)]),
        )
        .await
        .expect("start");
        let manager = integration.manager();
        assert!(!manager.in_zone("zone.edge", 0.0, 0.001, 0.0).unwrap());
        assert!(
            !manager
                .zones_containing(0.0, 0.001, 0.0)
                .unwrap()
                .zones
                .contains(&"zone.edge".to_owned())
        );
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn unavailable_zones_are_skipped_from_membership_queries() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 0.0, 0.0, 250.0)]),
        )
        .await
        .expect("start");
        runtime
            .set_state(
                "zone.work",
                "unavailable",
                attributes(json!({"latitude": 0.0, "longitude": 0.0, "radius": 250.0})),
                Context::default(),
                false,
            )
            .expect("mark unavailable");
        let matches = integration
            .manager()
            .zones_containing(0.0, 0.0, 0.0)
            .expect("matches");
        assert!(!matches.zones.contains(&"zone.work".to_owned()));
        assert!(
            !integration
                .manager()
                .in_zone("zone.work", 0.0, 0.0, 0.0)
                .unwrap()
        );
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn enclosing_zones_include_passive_and_touching_edges() {
        let runtime = runtime();
        let mut small = zone("Small", 0.0, 0.0, 10.0);
        small.passive = true;
        let mut outer = zone("Outer", 0.0, 0.0, 10.0);
        outer.passive = true;
        let integration = ZoneIntegration::start(runtime.clone(), home_config(vec![small, outer]))
            .await
            .expect("start");
        assert_eq!(
            integration.manager().enclosing_zones("zone.small").unwrap(),
            vec!["zone.outer", "zone.home"]
        );
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn enclosing_query_returns_empty_for_missing_or_unavailable_input() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Work", 0.0, 0.0, 250.0)]),
        )
        .await
        .expect("start");
        let manager = integration.manager();
        assert!(manager.enclosing_zones("zone.missing").unwrap().is_empty());
        runtime
            .set_state(
                "zone.work",
                "unavailable",
                attributes(json!({"latitude": 0.0, "longitude": 0.0, "radius": 250.0})),
                Context::default(),
                false,
            )
            .expect("mark unavailable");
        assert!(manager.enclosing_zones("zone.work").unwrap().is_empty());
        integration.shutdown().await.expect("shutdown");
    }

    #[test]
    fn vincenty_distance_matches_wgs84_equatorial_degree() {
        assert_eq!(distance_meters(0.0, 0.0, 0.0, 1.0), Some(111_319.491));
    }

    #[test]
    fn vincenty_nonconvergence_returns_none_for_antipodal_points() {
        assert_eq!(distance_meters(0.0, 0.0, 0.0, 180.0), None);
    }

    #[tokio::test]
    async fn storage_change_signal_matches_collection_websocket_contract() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(
            runtime.clone(),
            home_config(vec![zone("Yaml", 1.0, 2.0, 100.0)]),
        )
        .await
        .expect("start");
        let manager = integration.manager();
        let (snapshot, mut changes) = manager.snapshot_and_subscribe().unwrap();
        assert!(snapshot.is_empty());
        let created = manager.create(zone("Stored", 3.0, 4.0, 100.0)).unwrap();
        let event = changes.recv().await.expect("collection change");
        assert_eq!(event.event_type, ZONE_COLLECTION_UPDATED_SIGNAL);
        let change = &event.data["changes"][0];
        assert_eq!(change["change_type"], "added");
        assert_eq!(change["zone_id"], created.id);
        assert_eq!(change["item"]["name"], "Stored");

        manager
            .update(
                &created.id,
                ZoneUpdate {
                    passive: Some(true),
                    ..ZoneUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(
            changes.recv().await.unwrap().data["changes"][0]["change_type"],
            "updated"
        );
        manager.delete(&created.id).unwrap();
        assert_eq!(
            changes.recv().await.unwrap().data["changes"][0]["change_type"],
            "removed"
        );
        integration.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn shutdown_invalidates_retained_manager_handles() {
        let runtime = runtime();
        let integration = ZoneIntegration::start(runtime, home_config(Vec::new()))
            .await
            .expect("start");
        let retained = integration.manager();
        integration.shutdown().await.expect("shutdown");
        assert!(retained.list().is_err());
    }
}
