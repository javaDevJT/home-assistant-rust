//! Persistent, native Home Assistant registries.
//!
//! Registry snapshots are stored through [`hass_core::Runtime`] metadata. The
//! registry never opens its own database and only publishes registry events
//! after the new snapshot has been persisted.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use hass_core::{Context, Error, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod provider;
pub mod targets;
mod websocket;
pub use provider::{ProviderEntityInfo, ProviderEntityRegistration};

/// Result type shared with the core runtime.
pub type Result<T> = hass_core::Result<T>;

const STORAGE_KEY: &str = "core.registries";
const LEGACY_REGISTRY_VERSION: u8 = 1;
const REGISTRY_VERSION: u8 = 2;

/// Resolves a config-entry ID to its integration domain for composite parent
/// alias matching.
pub type ConfigEntryDomainResolver = dyn Fn(&str) -> Option<String> + Send + Sync + 'static;

/// A persistent entity registry entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EntityEntry {
    pub id: String,
    pub entity_id: String,
    pub domain: String,
    pub platform: String,
    pub unique_id: String,
    #[serde(default)]
    pub previous_unique_id: Option<String>,
    #[serde(default)]
    pub aliases: Vec<Value>,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub categories: BTreeMap<String, String>,
    #[serde(default)]
    pub capabilities: Option<Value>,
    #[serde(default)]
    pub config_entry_id: Option<String>,
    #[serde(default)]
    pub config_subentry_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub entity_category: Option<String>,
    #[serde(default)]
    pub hidden_by: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    pub modified_at: DateTime<Utc>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
    #[serde(default)]
    pub original_device_class: Option<String>,
    #[serde(default)]
    pub original_icon: Option<String>,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub suggested_object_id: Option<String>,
    #[serde(default)]
    pub supported_features: u64,
    #[serde(default)]
    pub translation_key: Option<String>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    #[serde(default)]
    pub has_entity_name: bool,
    /// Time this deleted entity became orphaned from a config entry/subentry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_timestamp: Option<DateTime<Utc>>,
}

impl EntityEntry {
    fn websocket_value(&self) -> Value {
        let original_name = self.original_name.clone();
        let next_name_part = if self.area_id.is_some() {
            Some("area")
        } else if self.device_id.is_some() {
            Some("device")
        } else {
            None
        };
        json!({
            "area_id": self.area_id,
            "categories": self.categories,
            "config_entry_id": self.config_entry_id,
            "config_subentry_id": self.config_subentry_id,
            "created_at": timestamp(self.created_at),
            "device_id": self.device_id,
            "disabled_by": self.disabled_by,
            "entity_category": self.entity_category,
            "entity_id": self.entity_id,
            "hidden_by": self.hidden_by,
            "icon": self.icon,
            "id": self.id,
            "labels": self.labels,
            "modified_at": timestamp(self.modified_at),
            "name": self.name,
            "next_name_part": next_name_part,
            "options": self.options,
            "original_name": original_name,
            "platform": self.platform,
            "translation_key": self.translation_key,
            "unique_id": self.unique_id,
            "aliases": self.aliases,
            "capabilities": self.capabilities,
            "device_class": self.device_class,
            "original_device_class": self.original_device_class,
            "original_icon": self.original_icon,
        })
    }

    fn display_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("ei".into(), Value::String(self.entity_id.clone()));
        value.insert("pl".into(), Value::String(self.platform.clone()));
        if let Some(area_id) = &self.area_id {
            value.insert("ai".into(), Value::String(area_id.clone()));
        }
        if !self.labels.is_empty() {
            value.insert("lb".into(), json!(self.labels));
        }
        if let Some(device_id) = &self.device_id {
            value.insert("di".into(), Value::String(device_id.clone()));
        }
        if let Some(next_name_part) =
            next_name_part(self.area_id.as_deref(), self.device_id.as_deref())
        {
            value.insert("np".into(), Value::String(next_name_part.into()));
        }
        if let Some(icon) = &self.icon {
            value.insert("ic".into(), Value::String(icon.clone()));
        }
        if let Some(translation_key) = &self.translation_key {
            value.insert("tk".into(), Value::String(translation_key.clone()));
        }
        if let Some(category) = &self.entity_category {
            let index = match category.as_str() {
                "config" => Some(0),
                "diagnostic" => Some(1),
                _ => None,
            };
            if let Some(index) = index {
                value.insert("ec".into(), json!(index));
            }
        }
        if self.hidden_by.is_some() {
            value.insert("hb".into(), Value::Bool(true));
        }
        if let Some(name) = self.name.as_ref().or(self.original_name.as_ref())
            && !name.is_empty()
        {
            value.insert("en".into(), Value::String(name.clone()));
        }
        if self.domain == "sensor"
            && let Some(sensor_options) = self.options.get("sensor").and_then(Value::as_object)
            && let Some(precision) = sensor_options
                .get("display_precision")
                .or_else(|| sensor_options.get("suggested_display_precision"))
        {
            value.insert("dp".into(), precision.clone());
        }
        Value::Object(value)
    }
}

/// Input used by integrations to register an entity by its upstream identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntityRegistration {
    pub domain: String,
    pub platform: String,
    pub unique_id: String,
    pub suggested_object_id: Option<String>,
    pub device_id: Option<String>,
    pub area_id: Option<String>,
    pub config_entry_id: Option<String>,
    pub config_subentry_id: Option<String>,
    pub name: Option<String>,
    pub original_name: Option<String>,
    pub device_class: Option<String>,
    pub original_device_class: Option<String>,
    pub icon: Option<String>,
    pub original_icon: Option<String>,
    pub entity_category: Option<String>,
    pub disabled_by: Option<String>,
    pub translation_key: Option<String>,
    pub unit_of_measurement: Option<String>,
    pub supported_features: u64,
    pub has_entity_name: bool,
    pub labels: BTreeSet<String>,
}

impl EntityRegistration {
    pub fn new(
        domain: impl Into<String>,
        platform: impl Into<String>,
        unique_id: impl Into<String>,
    ) -> Self {
        Self {
            domain: domain.into(),
            platform: platform.into(),
            unique_id: unique_id.into(),
            ..Self::default()
        }
    }
}

/// Typed entity update. `Some(None)` clears nullable fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntityUpdate {
    pub aliases: Option<Vec<Value>>,
    pub area_id: Option<Option<String>>,
    pub categories: Option<BTreeMap<String, Option<String>>>,
    pub device_class: Option<Option<String>>,
    pub device_id: Option<Option<String>>,
    pub disabled_by: Option<Option<String>>,
    pub hidden_by: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub labels: Option<BTreeSet<String>>,
    pub name: Option<Option<String>>,
    pub new_entity_id: Option<String>,
    pub options: Option<(String, Option<Value>)>,
}

/// A persistent device registry entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    pub id: String,
    #[serde(default)]
    pub config_entry_id: Option<String>,
    #[serde(default)]
    pub config_entries: Vec<String>,
    #[serde(default)]
    pub config_entries_subentries: BTreeMap<String, Vec<Option<String>>>,
    #[serde(default)]
    pub config_subentry_id: Option<String>,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub configuration_url: Option<String>,
    /// Former id of a multi-entry device which this split entry replaced.
    #[serde(default)]
    pub composite_device_id: Option<String>,
    /// Former primary config entry of the composite device.
    #[serde(default)]
    pub composite_primary_config_entry: Option<String>,
    #[serde(default)]
    pub connections: BTreeSet<(String, String)>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub hw_version: Option<String>,
    #[serde(default)]
    pub identifiers: BTreeSet<(String, String)>,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    pub modified_at: DateTime<Utc>,
    #[serde(default)]
    pub name_by_user: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
    /// Time the legacy composite device was split into single-entry devices.
    #[serde(default)]
    pub split_at: Option<DateTime<Utc>>,
    /// Set on split rows until their owning integration first re-registers IDs.
    #[serde(default)]
    pub has_composite_identifiers: bool,
    #[serde(default)]
    pub sw_version: Option<String>,
    #[serde(default)]
    pub via_device_id: Option<String>,
    /// Domain remembered for an orphaned deleted device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Time this deleted device became orphaned from a config entry/subentry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_timestamp: Option<DateTime<Utc>>,
}

impl DeviceEntry {
    fn websocket_value(&self) -> Value {
        let mut config_entries = self.config_entries.clone();
        if config_entries.is_empty()
            && let Some(config_entry_id) = &self.config_entry_id
        {
            config_entries.push(config_entry_id.clone());
        }
        let mut subentries = self.config_entries_subentries.clone();
        if subentries.is_empty()
            && let Some(config_entry_id) = &self.config_entry_id
        {
            subentries.insert(
                config_entry_id.clone(),
                vec![self.config_subentry_id.clone()],
            );
        }
        json!({
            "area_id": self.area_id,
            "configuration_url": self.configuration_url,
            "composite_device_id": self.composite_device_id,
            "composite_primary_config_entry": self.composite_primary_config_entry,
            "config_entries": config_entries,
            "config_entries_subentries": subentries,
            "config_entry_id": self.config_entry_id,
            "config_subentry_id": self.config_subentry_id,
            "connections": self.connections,
            "created_at": timestamp(self.created_at),
            "disabled_by": self.disabled_by,
            "entry_type": self.entry_type,
            "hw_version": self.hw_version,
            "has_composite_identifiers": self.has_composite_identifiers,
            "id": self.id,
            "identifiers": self.identifiers,
            "labels": self.labels,
            "manufacturer": self.manufacturer,
            "model": self.model,
            "model_id": self.model_id,
            "modified_at": timestamp(self.modified_at),
            "name_by_user": self.name_by_user,
            "name": self.name,
            "next_name_part": "device",
            "parent_device_id": Value::Null,
            "primary_config_entry": self.config_entry_id,
            "serial_number": self.serial_number,
            "split_at": self.split_at.as_ref().map(DateTime::to_rfc3339),
            "sw_version": self.sw_version,
            "via_device_id": self.via_device_id,
        })
    }
}

/// Counts produced while clearing registry records for one config subentry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigSubentryCleanup {
    pub entities_removed: usize,
    pub devices_removed: usize,
    pub entity_tombstones_orphaned: usize,
    pub device_tombstones_orphaned: usize,
}

/// Counts produced while clearing registry records for one config entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigEntryCleanup {
    pub entities_removed: usize,
    pub devices_removed: usize,
    pub entity_tombstones_orphaned: usize,
    pub device_tombstones_orphaned: usize,
    pub devices_updated: usize,
}

/// Split members of a removed pre-migration composite device.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositeSplit {
    /// Live device ids which replaced the composite device, in registry order.
    pub split_ids: Vec<String>,
    /// Split id owned by the former primary config entry, if it still exists.
    pub primary_id: Option<String>,
}

/// Input used by integrations to identify or create a device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceRegistration {
    /// Provider device type; Home Assistant currently defines only `service`.
    pub entry_type: Option<String>,
    pub config_entry_id: Option<String>,
    pub config_subentry_id: Option<String>,
    pub identifiers: BTreeSet<(String, String)>,
    pub connections: BTreeSet<(String, String)>,
    pub name: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub model_id: Option<String>,
    pub serial_number: Option<String>,
    pub sw_version: Option<String>,
    pub hw_version: Option<String>,
    pub configuration_url: Option<String>,
    pub via_device_id: Option<String>,
    pub area_id: Option<String>,
    pub labels: BTreeSet<String>,
}

/// Typed device update. `Some(None)` clears nullable fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceUpdate {
    pub area_id: Option<Option<String>>,
    pub disabled_by: Option<Option<String>>,
    pub labels: Option<BTreeSet<String>>,
    pub name_by_user: Option<Option<String>>,
}

/// Persistent area registry entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AreaEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    #[serde(default)]
    pub floor_id: Option<String>,
    #[serde(default)]
    pub humidity_entity_id: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub temperature_entity_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl AreaEntry {
    fn websocket_value(&self) -> Value {
        json!({
            "aliases": self.aliases,
            "area_id": self.id,
            "floor_id": self.floor_id,
            "humidity_entity_id": self.humidity_entity_id,
            "icon": self.icon,
            "labels": self.labels,
            "name": self.name,
            "picture": self.picture,
            "temperature_entity_id": self.temperature_entity_id,
            "created_at": timestamp(self.created_at),
            "modified_at": timestamp(self.modified_at),
        })
    }
}

/// Input used to create an area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AreaInput {
    pub name: String,
    pub aliases: BTreeSet<String>,
    pub floor_id: Option<String>,
    pub humidity_entity_id: Option<String>,
    pub icon: Option<String>,
    pub labels: BTreeSet<String>,
    pub picture: Option<String>,
    pub temperature_entity_id: Option<String>,
}

impl AreaInput {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Typed area update. `Some(None)` clears nullable fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AreaUpdate {
    pub aliases: Option<BTreeSet<String>>,
    pub floor_id: Option<Option<String>>,
    pub humidity_entity_id: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub labels: Option<BTreeSet<String>>,
    pub name: Option<String>,
    pub picture: Option<Option<String>>,
    pub temperature_entity_id: Option<Option<String>>,
}

/// Persistent floor registry entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FloorEntry {
    pub floor_id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub level: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl FloorEntry {
    fn websocket_value(&self) -> Value {
        json!({
            "aliases": self.aliases,
            "created_at": timestamp(self.created_at),
            "floor_id": self.floor_id,
            "icon": self.icon,
            "level": self.level,
            "name": self.name,
            "modified_at": timestamp(self.modified_at),
        })
    }
}

/// Input used to create a floor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FloorInput {
    pub name: String,
    pub aliases: BTreeSet<String>,
    pub icon: Option<String>,
    pub level: Option<i64>,
}

impl FloorInput {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Typed floor update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FloorUpdate {
    pub aliases: Option<BTreeSet<String>>,
    pub icon: Option<Option<String>>,
    pub level: Option<Option<i64>>,
    pub name: Option<String>,
}

/// Persistent label registry entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LabelEntry {
    pub label_id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl LabelEntry {
    fn websocket_value(&self) -> Value {
        json!({
            "color": self.color,
            "created_at": timestamp(self.created_at),
            "description": self.description,
            "icon": self.icon,
            "label_id": self.label_id,
            "name": self.name,
            "modified_at": timestamp(self.modified_at),
        })
    }
}

/// Input used to create a label.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LabelInput {
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
}

impl LabelInput {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Typed label update. `Some(None)` clears nullable fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LabelUpdate {
    pub color: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RegistryData {
    #[serde(default = "default_version")]
    version: u8,
    #[serde(default)]
    entities: Vec<EntityEntry>,
    #[serde(default)]
    deleted_entities: Vec<EntityEntry>,
    #[serde(default)]
    devices: Vec<DeviceEntry>,
    #[serde(default)]
    deleted_devices: Vec<DeviceEntry>,
    #[serde(default)]
    areas: Vec<AreaEntry>,
    #[serde(default)]
    floors: Vec<FloorEntry>,
    #[serde(default)]
    labels: Vec<LabelEntry>,
    #[serde(default)]
    entity_id_parts: Option<Vec<String>>,
}

fn default_version() -> u8 {
    LEGACY_REGISTRY_VERSION
}

impl Default for RegistryData {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            entities: Vec::new(),
            deleted_entities: Vec::new(),
            devices: Vec::new(),
            deleted_devices: Vec::new(),
            areas: Vec::new(),
            floors: Vec::new(),
            labels: Vec::new(),
            entity_id_parts: None,
        }
    }
}

#[derive(Clone)]
struct RegistryEvent {
    event_type: &'static str,
    action: &'static str,
    id_key: Option<&'static str>,
    id: Option<String>,
    changes: Option<Value>,
    extra: Option<(&'static str, Value)>,
}

impl RegistryEvent {
    fn item(
        event_type: &'static str,
        action: &'static str,
        id_key: &'static str,
        id: &str,
    ) -> Self {
        Self {
            event_type,
            action,
            id_key: Some(id_key),
            id: Some(id.to_owned()),
            changes: None,
            extra: None,
        }
    }

    fn changed(event_type: &'static str, id_key: &'static str, id: &str, changes: Value) -> Self {
        Self {
            event_type,
            action: "update",
            id_key: Some(id_key),
            id: Some(id.to_owned()),
            changes: Some(changes),
            extra: None,
        }
    }

    fn reorder(event_type: &'static str, id_key: &'static str) -> Self {
        let id_key = if event_type == "floor_registry_updated" {
            None
        } else {
            Some(id_key)
        };
        Self {
            event_type,
            action: "reorder",
            id_key,
            id: None,
            changes: None,
            extra: None,
        }
    }

    fn websocket_data(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("action".into(), Value::String(self.action.into()));
        if let Some(id_key) = self.id_key {
            data.insert(
                id_key.into(),
                self.id.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
        if let Some(changes) = &self.changes {
            data.insert("changes".into(), changes.clone());
        }
        if let Some((key, value)) = &self.extra {
            data.insert((*key).into(), value.clone());
        }
        data
    }
}

struct RegistryInner {
    runtime: Runtime,
    data: RwLock<RegistryData>,
    write_order: Mutex<()>,
    config_entry_domain_resolver: Option<Arc<ConfigEntryDomainResolver>>,
}

/// Cloneable access to entity, device, area, floor, and label registries.
#[derive(Clone)]
pub struct Registries {
    inner: Arc<RegistryInner>,
}

impl Registries {
    /// Load registry state from the runtime's durable metadata store.
    pub fn new(runtime: Runtime) -> Result<Self> {
        Self::new_with_optional_config_entry_domain_resolver(runtime, None)
    }
    /// Load registry state and resolve config-entry integration domains when
    /// mapping parent links through a legacy composite device ID.
    ///
    /// Parent links prefer a split in the same config entry, then one owned by
    /// the same integration domain, then any split for that composite ID.
    pub fn new_with_config_entry_domain_resolver(
        runtime: Runtime,
        resolver: Arc<ConfigEntryDomainResolver>,
    ) -> Result<Self> {
        Self::new_with_optional_config_entry_domain_resolver(runtime, Some(resolver))
    }
    fn new_with_optional_config_entry_domain_resolver(
        runtime: Runtime,
        resolver: Option<Arc<ConfigEntryDomainResolver>>,
    ) -> Result<Self> {
        let data = match runtime.get_data(STORAGE_KEY)? {
            Some(value) => {
                let mut loaded: RegistryData = serde_json::from_value(value).map_err(|error| {
                    Error::Storage(format!("invalid registry snapshot: {error}"))
                })?;
                if loaded.version == LEGACY_REGISTRY_VERSION {
                    migrate_legacy_devices(&mut loaded, resolver.as_deref()).map_err(|error| {
                        Error::Storage(format!("cannot migrate registry snapshot: {error}"))
                    })?;
                    loaded.version = REGISTRY_VERSION;
                    validate_data(&loaded).map_err(|error| {
                        Error::Storage(format!("invalid migrated registry snapshot: {error}"))
                    })?;
                    let serialized = serde_json::to_value(&loaded).map_err(|error| {
                        Error::Storage(format!("serialize migrated registry snapshot: {error}"))
                    })?;
                    runtime.set_data(STORAGE_KEY, &serialized)?;
                } else if loaded.version != REGISTRY_VERSION {
                    return Err(Error::Storage(format!(
                        "unsupported registry snapshot version {}",
                        loaded.version
                    )));
                } else {
                    validate_data(&loaded).map_err(|error| {
                        Error::Storage(format!("invalid registry snapshot: {error}"))
                    })?;
                }
                loaded
            }
            None => RegistryData::default(),
        };
        Ok(Self {
            inner: Arc::new(RegistryInner {
                runtime,
                data: RwLock::new(data),
                write_order: Mutex::new(()),
                config_entry_domain_resolver: resolver,
            }),
        })
    }

    /// Return entries in registry insertion order.
    pub fn entities(&self) -> Vec<EntityEntry> {
        self.snapshot().entities
    }
    /// Return one active entity entry.
    pub fn entity(&self, entity_id: &str) -> Option<EntityEntry> {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entities
            .iter()
            .find(|entry| entry.entity_id == entity_id)
            .cloned()
    }
    /// Return device entries in registry insertion order.
    pub fn devices(&self) -> Vec<DeviceEntry> {
        self.snapshot().devices
    }
    /// Return the live devices which replaced a pre-migration composite id.
    ///
    /// An id with no recorded split is not a composite device and returns an
    /// empty vector. The registry's persisted order is retained for callers
    /// that need the primary member or a stable fallback.
    pub fn devices_for_composite_device_id(&self, composite_device_id: &str) -> Vec<DeviceEntry> {
        self.snapshot()
            .devices
            .into_iter()
            .filter(|entry| entry.composite_device_id.as_deref() == Some(composite_device_id))
            .collect()
    }
    /// Return persisted composite ids and their current live split members.
    pub fn composite_splits(&self) -> BTreeMap<String, CompositeSplit> {
        let devices = self.snapshot().devices;
        let mut grouped: BTreeMap<String, Vec<&DeviceEntry>> = BTreeMap::new();
        for entry in &devices {
            if let Some(composite_id) = &entry.composite_device_id {
                grouped.entry(composite_id.clone()).or_default().push(entry);
            }
        }
        grouped
            .into_iter()
            .map(|(composite_id, members)| {
                let primary_config_entry = members
                    .first()
                    .and_then(|entry| entry.composite_primary_config_entry.as_deref());
                let primary_id = primary_config_entry.and_then(|primary_config_entry| {
                    members
                        .iter()
                        .find(|entry| {
                            entry.config_entry_id.as_deref() == Some(primary_config_entry)
                        })
                        .map(|entry| entry.id.clone())
                });
                let split_ids = members.iter().map(|entry| entry.id.clone()).collect();
                (
                    composite_id,
                    CompositeSplit {
                        split_ids,
                        primary_id,
                    },
                )
            })
            .collect()
    }
    /// Return one device entry by stable device ID.
    pub fn device(&self, device_id: &str) -> Option<DeviceEntry> {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .devices
            .iter()
            .find(|entry| entry.id == device_id)
            .cloned()
    }
    /// Return area entries in registry order.
    pub fn areas(&self) -> Vec<AreaEntry> {
        self.snapshot().areas
    }
    /// Return one area entry by stable area ID.
    pub fn area(&self, area_id: &str) -> Option<AreaEntry> {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .areas
            .iter()
            .find(|entry| entry.id == area_id)
            .cloned()
    }
    /// Return floor entries in registry order.
    pub fn floors(&self) -> Vec<FloorEntry> {
        self.snapshot().floors
    }
    /// Return one floor entry by stable floor ID.
    pub fn floor(&self, floor_id: &str) -> Option<FloorEntry> {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .floors
            .iter()
            .find(|entry| entry.floor_id == floor_id)
            .cloned()
    }
    /// Return label entries in registry order.
    pub fn labels(&self) -> Vec<LabelEntry> {
        self.snapshot().labels
    }
    /// Return one label entry by stable label ID.
    pub fn label(&self, label_id: &str) -> Option<LabelEntry> {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .labels
            .iter()
            .find(|entry| entry.label_id == label_id)
            .cloned()
    }

    /// Get or create an entity by `(domain, platform, unique_id)` identity.
    pub fn get_or_create_entity(&self, registration: EntityRegistration) -> Result<EntityEntry> {
        validate_registration(&registration)?;
        self.transact(|data| {
            if let Some(index) = data
                .entities
                .iter()
                .position(|entry| entity_identity_matches(entry, &registration))
            {
                let old = data.entities[index].clone();
                let mut new = old.clone();
                new.device_id = registration.device_id.clone().or(new.device_id);
                new.area_id = registration.area_id.clone().or(new.area_id);
                new.config_entry_id = registration.config_entry_id.clone().or(new.config_entry_id);
                new.config_subentry_id = registration
                    .config_subentry_id
                    .clone()
                    .or(new.config_subentry_id);
                if new != old {
                    new.modified_at = Utc::now();
                    let changes = entity_change_values(&old, &new);
                    data.entities[index] = new.clone();
                    return Ok((
                        new.clone(),
                        vec![RegistryEvent::changed(
                            "entity_registry_updated",
                            "entity_id",
                            &new.entity_id,
                            changes,
                        )],
                    ));
                }
                return Ok((old, Vec::new()));
            }
            if let Some(index) = data
                .deleted_entities
                .iter()
                .position(|entry| entity_identity_matches(entry, &registration))
            {
                let mut restored = data.deleted_entities.remove(index);
                if !entity_id_available(data, &restored.entity_id, &self.inner.runtime) {
                    restored.entity_id =
                        generate_entity_id(data, &registration, &self.inner.runtime);
                    restored.id = new_hex_id();
                }
                restored.device_id = registration.device_id.clone().or(restored.device_id);
                restored.area_id = registration.area_id.clone().or(restored.area_id);
                restored.config_entry_id = registration
                    .config_entry_id
                    .clone()
                    .or(restored.config_entry_id);
                restored.config_subentry_id = registration
                    .config_subentry_id
                    .clone()
                    .or(restored.config_subentry_id);
                restored.modified_at = Utc::now();
                data.entities.push(restored.clone());
                return Ok((
                    restored.clone(),
                    vec![RegistryEvent::item(
                        "entity_registry_updated",
                        "create",
                        "entity_id",
                        &restored.entity_id,
                    )],
                ));
            }
            let now = Utc::now();
            let entry = EntityEntry {
                id: new_hex_id(),
                entity_id: generate_entity_id(data, &registration, &self.inner.runtime),
                domain: registration.domain.clone(),
                platform: registration.platform.clone(),
                unique_id: registration.unique_id.clone(),
                previous_unique_id: None,
                aliases: Vec::new(),
                area_id: registration.area_id.clone(),
                categories: BTreeMap::new(),
                capabilities: None,
                config_entry_id: registration.config_entry_id.clone(),
                config_subentry_id: registration.config_subentry_id.clone(),
                created_at: now,
                device_class: registration.device_class.clone(),
                device_id: registration.device_id.clone(),
                disabled_by: registration.disabled_by.clone(),
                entity_category: registration.entity_category.clone(),
                hidden_by: None,
                icon: registration.icon.clone(),
                labels: registration.labels.clone(),
                modified_at: now,
                name: registration.name.clone(),
                options: BTreeMap::new(),
                original_device_class: registration.original_device_class.clone(),
                original_icon: registration.original_icon.clone(),
                original_name: registration.original_name.clone(),
                suggested_object_id: registration.suggested_object_id.clone(),
                supported_features: registration.supported_features,
                translation_key: registration.translation_key.clone(),
                unit_of_measurement: registration.unit_of_measurement.clone(),
                has_entity_name: registration.has_entity_name,
                orphaned_timestamp: None,
            };
            data.entities.push(entry.clone());
            Ok((
                entry.clone(),
                vec![RegistryEvent::item(
                    "entity_registry_updated",
                    "create",
                    "entity_id",
                    &entry.entity_id,
                )],
            ))
        })
    }

    /// Update a registered entity entry.
    pub fn update_entity(&self, entity_id: &str, update: EntityUpdate) -> Result<EntityEntry> {
        self.transact(|data| {
            let index = data
                .entities
                .iter()
                .position(|entry| entry.entity_id == entity_id)
                .ok_or_else(|| Error::NotFound(format!("entity {entity_id}")))?;
            let old = data.entities[index].clone();
            let mut new = old.clone();
            if let Some(value) = update.aliases {
                new.aliases = clean_entity_aliases(value)?;
            }
            if let Some(value) = update.area_id {
                new.area_id = value;
            }
            if let Some(categories) = update.categories {
                for (scope, category) in categories {
                    validate_nonempty(&scope, "category scope")?;
                    if !valid_slug(&scope) {
                        return Err(invalid("category scope must be a lowercase slug"));
                    }
                    match category {
                        Some(value) => {
                            validate_nonempty(&value, "category ID")?;
                            if !valid_slug(&value) {
                                return Err(invalid("category ID must be a lowercase slug"));
                            }
                            new.categories.insert(scope, value);
                        }
                        None => {
                            new.categories.remove(&scope);
                        }
                    }
                }
            }
            if let Some(value) = update.device_class {
                new.device_class = value;
            }
            if let Some(value) = update.device_id {
                new.device_id = value;
            }
            if let Some(value) = update.disabled_by {
                validate_disabler(value.as_deref())?;
                new.disabled_by = value;
            }
            if let Some(value) = update.hidden_by {
                validate_user_flag(value.as_deref(), "hidden_by")?;
                new.hidden_by = value;
            }
            if let Some(value) = update.icon {
                new.icon = value;
            }
            if let Some(value) = update.labels {
                new.labels = value;
            }
            if let Some(value) = update.name {
                new.name = value;
            }
            if let Some(new_entity_id) = update.new_entity_id {
                validate_entity_id(&new_entity_id)?;
                if entity_domain(&new_entity_id) != new.domain {
                    return Err(invalid(
                        "new_entity_id must keep the existing entity domain",
                    ));
                }
                if data
                    .entities
                    .iter()
                    .any(|entry| entry.entity_id == new_entity_id && entry.entity_id != entity_id)
                {
                    return Err(invalid(format!(
                        "entity ID {new_entity_id} is already registered"
                    )));
                }
                if new_entity_id != entity_id
                    && self.inner.runtime.get_state(&new_entity_id).is_some()
                {
                    return Err(invalid(format!(
                        "entity ID {new_entity_id} already has a runtime state"
                    )));
                }
                new.entity_id = new_entity_id;
            }
            if let Some((domain, options)) = update.options {
                validate_nonempty(&domain, "options_domain")?;
                match options {
                    Some(value) => {
                        if !value.is_object() {
                            return Err(invalid("options must be an object or null"));
                        }
                        new.options.insert(domain, value);
                    }
                    None => {
                        new.options.remove(&domain);
                    }
                }
            }
            if new == old {
                return Ok((old, Vec::new()));
            }
            new.modified_at = Utc::now();
            let changes = entity_change_values(&old, &new);
            let event_id = new.entity_id.clone();
            let mut event =
                RegistryEvent::changed("entity_registry_updated", "entity_id", &event_id, changes);
            if old.entity_id != new.entity_id {
                event.extra = Some(("old_entity_id", Value::String(old.entity_id.clone())));
            }
            data.entities[index] = new.clone();
            Ok((new, vec![event]))
        })
    }

    /// Remove an entity entry. Its unique-ID identity is retained for restoration.
    pub fn remove_entity(&self, entity_id: &str) -> Result<()> {
        self.transact(|data| Ok(((), remove_entity_from_data(data, entity_id)?)))
    }

    /// Get or create a device by an identifier or connection pair.
    pub fn get_or_create_device(&self, registration: DeviceRegistration) -> Result<DeviceEntry> {
        validate_device_registration(&registration)?;
        let resolver = self.inner.config_entry_domain_resolver.clone();
        let registration_domain =
            registration
                .config_entry_id
                .as_deref()
                .and_then(|config_entry_id| {
                    resolver
                        .as_deref()
                        .and_then(|resolver| resolver(config_entry_id))
                });
        self.transact(move |data| {
            let mut registration = registration;
            if let Some(via_device_id) = registration.via_device_id.clone() {
                registration.via_device_id = Some(resolve_via_device_id(
                    data,
                    &via_device_id,
                    registration.config_entry_id.as_deref(),
                    resolver.as_deref(),
                ));
            }
            let matching: Vec<usize> = data
                .devices
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    if !device_in_config_entry(entry, registration.config_entry_id.as_deref()) {
                        return None;
                    }
                    let identifier_match = entry
                        .identifiers
                        .iter()
                        .any(|pair| registration.identifiers.contains(pair));
                    let connection_match = entry
                        .connections
                        .iter()
                        .any(|pair| registration.connections.contains(pair));
                    (identifier_match || connection_match).then_some(index)
                })
                .collect();
            if matching.len() > 1 {
                return Err(invalid(
                    "device identity matches more than one registered device",
                ));
            }
            if let Some(index) = matching.first().copied() {
                let old = data.devices[index].clone();
                let mut new = old.clone();
                if old.has_composite_identifiers {
                    new.identifiers = registration.identifiers.clone();
                    new.connections = registration.connections.clone();
                    new.has_composite_identifiers = false;
                } else {
                    new.identifiers.extend(registration.identifiers.clone());
                    new.connections.extend(registration.connections.clone());
                }
                if new.config_entry_id.is_none() {
                    new.config_entry_id = registration.config_entry_id.clone();
                }
                if new.config_subentry_id.is_none() {
                    new.config_subentry_id = registration.config_subentry_id.clone();
                }
                if let Some(config_entry_id) = &registration.config_entry_id {
                    if !new.config_entries.contains(config_entry_id) {
                        new.config_entries.push(config_entry_id.clone());
                    }
                    let subentries = new
                        .config_entries_subentries
                        .entry(config_entry_id.clone())
                        .or_default();
                    if !subentries.contains(&registration.config_subentry_id) {
                        subentries.push(registration.config_subentry_id.clone());
                    }
                }
                if registration.name.is_some() {
                    new.name = registration.name.clone();
                }
                if registration.entry_type.is_some() {
                    new.entry_type = registration.entry_type.clone();
                }
                if registration.manufacturer.is_some() {
                    new.manufacturer = registration.manufacturer.clone();
                }
                if registration.model.is_some() {
                    new.model = registration.model.clone();
                }
                if registration.model_id.is_some() {
                    new.model_id = registration.model_id.clone();
                }
                if registration.serial_number.is_some() {
                    new.serial_number = registration.serial_number.clone();
                }
                if registration.sw_version.is_some() {
                    new.sw_version = registration.sw_version.clone();
                }
                if registration.hw_version.is_some() {
                    new.hw_version = registration.hw_version.clone();
                }
                if registration.configuration_url.is_some() {
                    new.configuration_url = registration.configuration_url.clone();
                }
                if registration.area_id.is_some() {
                    new.area_id = registration.area_id.clone();
                }
                if registration.via_device_id.is_some() {
                    new.via_device_id = registration.via_device_id.clone();
                }
                new.labels.extend(registration.labels.clone());
                if new != old {
                    new.modified_at = Utc::now();
                    let changes = device_change_values(&old, &new);
                    let id = new.id.clone();
                    data.devices[index] = new.clone();
                    return Ok((
                        new,
                        vec![RegistryEvent::changed(
                            "device_registry_updated",
                            "device_id",
                            &id,
                            changes,
                        )],
                    ));
                }
                return Ok((old, Vec::new()));
            }
            let matching_deleted: Vec<usize> = data
                .deleted_devices
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let owned_by_entry =
                        device_in_config_entry(entry, registration.config_entry_id.as_deref());
                    let orphaned_from_same_domain =
                        registration_domain.as_deref().is_some_and(|domain| {
                            entry.orphaned_timestamp.is_some()
                                && entry.domain.as_deref() == Some(domain)
                        });
                    if !owned_by_entry && !orphaned_from_same_domain {
                        return None;
                    }
                    let identifier_match = entry
                        .identifiers
                        .iter()
                        .any(|pair| registration.identifiers.contains(pair));
                    let connection_match = entry
                        .connections
                        .iter()
                        .any(|pair| registration.connections.contains(pair));
                    (identifier_match || connection_match).then_some(index)
                })
                .collect();
            if matching_deleted.len() > 1 {
                return Err(invalid(
                    "device identity matches more than one removed device",
                ));
            }
            if let Some(index) = matching_deleted.first().copied() {
                let mut entry = data.deleted_devices.remove(index);
                entry.identifiers.extend(registration.identifiers.clone());
                entry.connections.extend(registration.connections.clone());
                entry.domain = None;
                entry.orphaned_timestamp = None;
                if entry.config_entry_id.is_none() {
                    entry.config_entry_id = registration.config_entry_id.clone();
                }
                if entry.config_subentry_id.is_none() {
                    entry.config_subentry_id = registration.config_subentry_id.clone();
                }
                if let Some(config_entry_id) = &registration.config_entry_id {
                    if !entry.config_entries.contains(config_entry_id) {
                        entry.config_entries.push(config_entry_id.clone());
                    }
                    let subentries = entry
                        .config_entries_subentries
                        .entry(config_entry_id.clone())
                        .or_default();
                    if !subentries.contains(&registration.config_subentry_id) {
                        subentries.push(registration.config_subentry_id.clone());
                    }
                }
                if registration.name.is_some() {
                    entry.name = registration.name.clone();
                }
                if registration.entry_type.is_some() {
                    entry.entry_type = registration.entry_type.clone();
                }
                if registration.manufacturer.is_some() {
                    entry.manufacturer = registration.manufacturer.clone();
                }
                if registration.model.is_some() {
                    entry.model = registration.model.clone();
                }
                if registration.model_id.is_some() {
                    entry.model_id = registration.model_id.clone();
                }
                if registration.serial_number.is_some() {
                    entry.serial_number = registration.serial_number.clone();
                }
                if registration.sw_version.is_some() {
                    entry.sw_version = registration.sw_version.clone();
                }
                if registration.hw_version.is_some() {
                    entry.hw_version = registration.hw_version.clone();
                }
                if registration.configuration_url.is_some() {
                    entry.configuration_url = registration.configuration_url.clone();
                }
                if registration.area_id.is_some() {
                    entry.area_id = registration.area_id.clone();
                }
                if registration.via_device_id.is_some() {
                    entry.via_device_id = registration.via_device_id.clone();
                }
                entry.labels.extend(registration.labels.clone());
                entry.modified_at = Utc::now();
                let id = entry.id.clone();
                data.devices.push(entry.clone());
                return Ok((
                    entry,
                    vec![RegistryEvent::item(
                        "device_registry_updated",
                        "create",
                        "device_id",
                        &id,
                    )],
                ));
            }
            let now = Utc::now();
            let config_entries = registration
                .config_entry_id
                .clone()
                .into_iter()
                .collect::<Vec<_>>();
            let config_entries_subentries = registration
                .config_entry_id
                .clone()
                .map(|id| (id, vec![registration.config_subentry_id.clone()]))
                .into_iter()
                .collect();
            let entry = DeviceEntry {
                id: new_hex_id(),
                config_entry_id: registration.config_entry_id.clone(),
                config_entries,
                config_entries_subentries,
                config_subentry_id: registration.config_subentry_id.clone(),
                area_id: registration.area_id.clone(),
                configuration_url: registration.configuration_url.clone(),
                composite_device_id: None,
                composite_primary_config_entry: None,
                connections: registration.connections.clone(),
                created_at: now,
                disabled_by: None,
                entry_type: registration.entry_type.clone(),
                hw_version: registration.hw_version.clone(),
                identifiers: registration.identifiers.clone(),
                labels: registration.labels.clone(),
                manufacturer: registration.manufacturer.clone(),
                model: registration.model.clone(),
                model_id: registration.model_id.clone(),
                modified_at: now,
                name_by_user: None,
                name: registration.name.clone(),
                serial_number: registration.serial_number.clone(),
                split_at: None,
                has_composite_identifiers: false,
                sw_version: registration.sw_version.clone(),
                via_device_id: registration.via_device_id.clone(),
                domain: None,
                orphaned_timestamp: None,
            };
            let id = entry.id.clone();
            data.devices.push(entry.clone());
            Ok((
                entry,
                vec![RegistryEvent::item(
                    "device_registry_updated",
                    "create",
                    "device_id",
                    &id,
                )],
            ))
        })
    }

    /// Split a legacy multi-entry device into one stable device per config entry.
    ///
    /// The old id is retained as `composite_device_id` on each replacement so
    /// callers which still hold it can resolve all split members. Repeating the
    /// operation for an already split id returns its current members.
    pub fn split_legacy_device(&self, composite_device_id: &str) -> Result<Vec<DeviceEntry>> {
        let resolver = self.inner.config_entry_domain_resolver.clone();
        self.transact(move |data| {
            if data
                .devices
                .iter()
                .any(|entry| entry.composite_device_id.as_deref() == Some(composite_device_id))
            {
                return Ok((
                    data.devices
                        .iter()
                        .filter(|entry| {
                            entry.composite_device_id.as_deref() == Some(composite_device_id)
                        })
                        .cloned()
                        .collect(),
                    Vec::new(),
                ));
            }
            let (members, mut events) =
                split_legacy_device_in_data(data, composite_device_id, resolver.as_deref())?;
            events.extend(reconcile_via_device_references(data, resolver.as_deref()));
            Ok((members, events))
        })
    }

    /// Update an existing device.
    pub fn update_device(&self, device_id: &str, update: DeviceUpdate) -> Result<DeviceEntry> {
        self.transact(|data| {
            let index = data
                .devices
                .iter()
                .position(|entry| entry.id == device_id)
                .ok_or_else(|| Error::NotFound(format!("device {device_id}")))?;
            let old = data.devices[index].clone();
            let mut new = old.clone();
            if let Some(value) = update.area_id {
                new.area_id = value;
            }
            if let Some(value) = update.disabled_by {
                validate_disabler(value.as_deref())?;
                new.disabled_by = value;
            }
            if let Some(value) = update.labels {
                new.labels = value;
            }
            if let Some(value) = update.name_by_user {
                new.name_by_user = value;
            }
            if new == old {
                return Ok((old, Vec::new()));
            }
            new.modified_at = Utc::now();
            let changes = device_change_values(&old, &new);
            let id = new.id.clone();
            data.devices[index] = new.clone();
            Ok((
                new,
                vec![RegistryEvent::changed(
                    "device_registry_updated",
                    "device_id",
                    &id,
                    changes,
                )],
            ))
        })
    }

    /// Remove a device and clear entity and parent-device references to it.
    pub fn remove_device(&self, device_id: &str) -> Result<()> {
        self.transact(|data| Ok(((), remove_device_from_data(data, device_id)?)))
    }

    /// Clear device and entity registry ownership for one config subentry.
    ///
    /// Device and entity changes are committed as one registry snapshot. Device
    /// removal notifications precede entity notifications, matching the core
    /// config-entry cleanup sequence. Deleted entity tombstones intentionally
    /// match by subentry ID alone, as in the pinned entity registry helper.
    pub fn clear_config_subentry(
        &self,
        config_entry_id: &str,
        config_subentry_id: &str,
    ) -> Result<ConfigSubentryCleanup> {
        let domain = self
            .inner
            .config_entry_domain_resolver
            .as_deref()
            .and_then(|resolver| resolver(config_entry_id));
        let now = Utc::now();
        self.transact(|data| {
            let mut cleanup = ConfigSubentryCleanup::default();
            let mut events = Vec::new();

            // The pinned device registry selects active devices by their owning
            // config entry and primary config_subentry_id. Keep other entries and
            // additional legacy subentry-map memberships untouched.
            let device_ids = data
                .devices
                .iter()
                .filter(|device| {
                    device_in_config_entry(device, Some(config_entry_id))
                        && device.config_subentry_id.as_deref() == Some(config_subentry_id)
                })
                .map(|device| device.id.clone())
                .collect::<Vec<_>>();
            for device_id in device_ids {
                events.extend(remove_device_from_data(data, &device_id)?);
                cleanup.devices_removed += 1;
            }

            // A removed device can have produced a tombstone above, so perform
            // orphaning after the active-device removal pass.
            let deleted_device_ids = data
                .deleted_devices
                .iter()
                .filter(|device| {
                    device.config_entry_id.as_deref() == Some(config_entry_id)
                        && device.config_subentry_id.as_deref() == Some(config_subentry_id)
                })
                .map(|device| device.id.clone())
                .collect::<Vec<_>>();
            for device_id in deleted_device_ids {
                if orphan_deleted_device(data, &device_id, domain.as_deref(), now) {
                    cleanup.device_tombstones_orphaned += 1;
                }
            }

            let entity_ids = data
                .entities
                .iter()
                .filter(|entity| {
                    entity.config_entry_id.as_deref() == Some(config_entry_id)
                        && entity.config_subentry_id.as_deref() == Some(config_subentry_id)
                })
                .map(|entity| entity.entity_id.clone())
                .collect::<Vec<_>>();
            for entity_id in entity_ids {
                events.extend(remove_entity_from_data(data, &entity_id)?);
                cleanup.entities_removed += 1;
            }

            // The source helper deliberately matches deleted entities globally
            // by subentry ID, even when a tombstone still names another parent.
            for entity in &mut data.deleted_entities {
                if entity.config_subentry_id.as_deref() != Some(config_subentry_id) {
                    continue;
                }
                entity.orphaned_timestamp = Some(now);
                entity.config_entry_id = None;
                entity.config_subentry_id = None;
                cleanup.entity_tombstones_orphaned += 1;
            }

            Ok((cleanup, events))
        })
    }

    /// Clear device and entity registry ownership for a removed config entry.
    ///
    /// The domain is supplied by the removal observer because the config entry
    /// has already been committed out of the config-entry registry by the time
    /// this method runs. No Runtime states are removed by registry cleanup.
    pub fn clear_config_entry(
        &self,
        config_entry_id: &str,
        domain: &str,
    ) -> Result<ConfigEntryCleanup> {
        validate_nonempty(domain, "config entry domain")?;
        let domain = domain.to_owned();
        let now = Utc::now();
        self.transact(|data| {
            let mut cleanup = ConfigEntryCleanup::default();
            let mut events = Vec::new();

            let device_ids = data
                .devices
                .iter()
                .filter(|device| device_in_config_entry(device, Some(config_entry_id)))
                .map(|device| device.id.clone())
                .collect::<Vec<_>>();
            for device_id in device_ids {
                events.extend(remove_device_from_data(data, &device_id)?);
                cleanup.devices_removed += 1;
            }

            // Split devices retain this former-primary owner for old composite
            // aliases. Once the owner is gone, clear only that dangling pointer.
            for device in &mut data.devices {
                if device.composite_primary_config_entry.as_deref() == Some(config_entry_id) {
                    device.composite_primary_config_entry = None;
                    cleanup.devices_updated += 1;
                }
            }

            let deleted_device_ids = data
                .deleted_devices
                .iter()
                .filter(|device| device.config_entry_id.as_deref() == Some(config_entry_id))
                .map(|device| device.id.clone())
                .collect::<Vec<_>>();
            for device_id in deleted_device_ids {
                if orphan_deleted_device(data, &device_id, Some(&domain), now) {
                    cleanup.device_tombstones_orphaned += 1;
                }
            }

            let entity_ids = data
                .entities
                .iter()
                .filter(|entity| entity.config_entry_id.as_deref() == Some(config_entry_id))
                .map(|entity| entity.entity_id.clone())
                .collect::<Vec<_>>();
            for entity_id in entity_ids {
                events.extend(remove_entity_from_data(data, &entity_id)?);
                cleanup.entities_removed += 1;
            }

            // Config-entry cleanup preserves the entity tombstone's historical
            // subentry ID, matching async_clear_config_entry in core.
            for entity in &mut data.deleted_entities {
                if entity.config_entry_id.as_deref() != Some(config_entry_id) {
                    continue;
                }
                entity.orphaned_timestamp = Some(now);
                entity.config_entry_id = None;
                cleanup.entity_tombstones_orphaned += 1;
            }

            Ok((cleanup, events))
        })
    }

    /// Remove the device named by the deprecated `remove_config_entry` command.
    pub fn remove_config_entry_from_device(
        &self,
        device_id: &str,
        config_entry_id: &str,
    ) -> Result<()> {
        let device = self
            .device(device_id)
            .ok_or_else(|| Error::NotFound(format!("device {device_id}")))?;
        if device.config_entry_id.as_deref() != Some(config_entry_id)
            && !device.config_entries.iter().any(|id| id == config_entry_id)
        {
            return Err(invalid("config_entry_id does not match the device"));
        }
        self.remove_device(device_id)
    }

    /// Create an area with a stable slug-based ID.
    pub fn create_area(&self, input: AreaInput) -> Result<AreaEntry> {
        self.transact(|data| {
            validate_name(&input.name, "area name")?;
            ensure_unique_name(
                data.areas.iter().map(|entry| &entry.name),
                &input.name,
                "area",
            )?;
            let now = Utc::now();
            let id = generated_name_id(
                &input.name,
                data.areas.iter().map(|entry| entry.id.as_str()),
            );
            let entry = AreaEntry {
                id: id.clone(),
                name: input.name.clone(),
                aliases: clean_aliases(&input.aliases),
                floor_id: input.floor_id.clone(),
                humidity_entity_id: input.humidity_entity_id.clone(),
                icon: input.icon.clone(),
                labels: input.labels.clone(),
                picture: input.picture.clone(),
                temperature_entity_id: input.temperature_entity_id.clone(),
                created_at: now,
                modified_at: now,
            };
            data.areas.push(entry.clone());
            Ok((
                entry,
                vec![RegistryEvent::item(
                    "area_registry_updated",
                    "create",
                    "area_id",
                    &id,
                )],
            ))
        })
    }

    /// Update an area, preserving its stable ID.
    pub fn update_area(&self, area_id: &str, update: AreaUpdate) -> Result<AreaEntry> {
        self.transact(|data| {
            let index = data
                .areas
                .iter()
                .position(|entry| entry.id == area_id)
                .ok_or_else(|| Error::NotFound(format!("area {area_id}")))?;
            let old = data.areas[index].clone();
            let mut new = old.clone();
            if let Some(value) = update.aliases {
                new.aliases = clean_aliases(&value);
            }
            if let Some(value) = update.floor_id {
                new.floor_id = value;
            }
            if let Some(value) = update.humidity_entity_id {
                new.humidity_entity_id = value;
            }
            if let Some(value) = update.icon {
                new.icon = value;
            }
            if let Some(value) = update.labels {
                new.labels = value;
            }
            if let Some(value) = update.name {
                new.name = value;
            }
            if let Some(value) = update.picture {
                new.picture = value;
            }
            if let Some(value) = update.temperature_entity_id {
                new.temperature_entity_id = value;
            }
            if new.name != old.name {
                validate_name(&new.name, "area name")?;
                if normalized_name(&new.name) != normalized_name(&old.name) {
                    ensure_unique_name(
                        data.areas.iter().map(|entry| &entry.name),
                        &new.name,
                        "area",
                    )?;
                }
            }
            if new == old {
                return Ok((old, Vec::new()));
            }
            new.modified_at = Utc::now();
            data.areas[index] = new.clone();
            Ok((
                new.clone(),
                vec![RegistryEvent::item(
                    "area_registry_updated",
                    "update",
                    "area_id",
                    area_id,
                )],
            ))
        })
    }

    /// Delete an area and clear all entity/device area references.
    pub fn delete_area(&self, area_id: &str) -> Result<()> {
        self.transact(|data| {
            let index = data
                .areas
                .iter()
                .position(|entry| entry.id == area_id)
                .ok_or_else(|| Error::NotFound(format!("area {area_id}")))?;
            data.areas.remove(index);
            let mut events = Vec::new();
            for entity in &mut data.entities {
                if entity.area_id.as_deref() == Some(area_id) {
                    entity.area_id = None;
                    entity.modified_at = Utc::now();
                    events.push(RegistryEvent::changed(
                        "entity_registry_updated",
                        "entity_id",
                        &entity.entity_id,
                        json!({"area_id": area_id}),
                    ));
                }
            }
            for entity in &mut data.deleted_entities {
                if entity.area_id.as_deref() == Some(area_id) {
                    entity.area_id = None;
                    entity.modified_at = Utc::now();
                }
            }
            for device in &mut data.devices {
                if device.area_id.as_deref() == Some(area_id) {
                    device.area_id = None;
                    device.modified_at = Utc::now();
                    events.push(RegistryEvent::changed(
                        "device_registry_updated",
                        "device_id",
                        &device.id,
                        json!({"area_id": area_id}),
                    ));
                }
            }
            for device in &mut data.deleted_devices {
                if device.area_id.as_deref() == Some(area_id) {
                    device.area_id = None;
                    device.modified_at = Utc::now();
                }
            }
            events.push(RegistryEvent::item(
                "area_registry_updated",
                "remove",
                "area_id",
                area_id,
            ));
            Ok(((), events))
        })
    }

    /// Reorder all areas. IDs must contain each existing area exactly once.
    pub fn reorder_areas(&self, area_ids: &[String]) -> Result<()> {
        self.transact(|data| {
            validate_reorder(
                area_ids,
                data.areas.iter().map(|entry| entry.id.as_str()),
                "area_ids",
            )?;
            let mut by_id: HashMap<String, AreaEntry> = data
                .areas
                .drain(..)
                .map(|entry| (entry.id.clone(), entry))
                .collect();
            data.areas = area_ids
                .iter()
                .map(|id| by_id.remove(id).expect("validated area ID"))
                .collect();
            Ok((
                (),
                vec![RegistryEvent::reorder("area_registry_updated", "area_id")],
            ))
        })
    }

    /// Create a floor with a stable slug-based ID.
    pub fn create_floor(&self, input: FloorInput) -> Result<FloorEntry> {
        self.transact(|data| {
            validate_name(&input.name, "floor name")?;
            ensure_unique_name(
                data.floors.iter().map(|entry| &entry.name),
                &input.name,
                "floor",
            )?;
            let now = Utc::now();
            let id = generated_name_id(
                &input.name,
                data.floors.iter().map(|entry| entry.floor_id.as_str()),
            );
            let entry = FloorEntry {
                floor_id: id.clone(),
                name: input.name.clone(),
                aliases: clean_aliases(&input.aliases),
                icon: input.icon.clone(),
                level: input.level,
                created_at: now,
                modified_at: now,
            };
            data.floors.push(entry.clone());
            Ok((
                entry,
                vec![RegistryEvent::item(
                    "floor_registry_updated",
                    "create",
                    "floor_id",
                    &id,
                )],
            ))
        })
    }

    /// Update a floor, preserving its stable ID.
    pub fn update_floor(&self, floor_id: &str, update: FloorUpdate) -> Result<FloorEntry> {
        self.transact(|data| {
            let index = data
                .floors
                .iter()
                .position(|entry| entry.floor_id == floor_id)
                .ok_or_else(|| Error::NotFound(format!("floor {floor_id}")))?;
            let old = data.floors[index].clone();
            let mut new = old.clone();
            if let Some(value) = update.aliases {
                new.aliases = clean_aliases(&value);
            }
            if let Some(value) = update.icon {
                new.icon = value;
            }
            if let Some(value) = update.level {
                new.level = value;
            }
            if let Some(value) = update.name {
                new.name = value;
            }
            if new.name != old.name {
                validate_name(&new.name, "floor name")?;
                if normalized_name(&new.name) != normalized_name(&old.name) {
                    ensure_unique_name(
                        data.floors.iter().map(|entry| &entry.name),
                        &new.name,
                        "floor",
                    )?;
                }
            }
            if new == old {
                return Ok((old, Vec::new()));
            }
            new.modified_at = Utc::now();
            data.floors[index] = new.clone();
            Ok((
                new,
                vec![RegistryEvent::item(
                    "floor_registry_updated",
                    "update",
                    "floor_id",
                    floor_id,
                )],
            ))
        })
    }

    /// Delete a floor and clear the floor reference from its areas.
    pub fn delete_floor(&self, floor_id: &str) -> Result<()> {
        self.transact(|data| {
            let index = data
                .floors
                .iter()
                .position(|entry| entry.floor_id == floor_id)
                .ok_or_else(|| Error::NotFound(format!("floor {floor_id}")))?;
            data.floors.remove(index);
            let mut events = Vec::new();
            for area in &mut data.areas {
                if area.floor_id.as_deref() == Some(floor_id) {
                    area.floor_id = None;
                    area.modified_at = Utc::now();
                    events.push(RegistryEvent::item(
                        "area_registry_updated",
                        "update",
                        "area_id",
                        &area.id,
                    ));
                }
            }
            events.push(RegistryEvent::item(
                "floor_registry_updated",
                "remove",
                "floor_id",
                floor_id,
            ));
            Ok(((), events))
        })
    }

    /// Reorder all floors. IDs must contain each existing floor exactly once.
    pub fn reorder_floors(&self, floor_ids: &[String]) -> Result<()> {
        self.transact(|data| {
            validate_reorder(
                floor_ids,
                data.floors.iter().map(|entry| entry.floor_id.as_str()),
                "floor_ids",
            )?;
            let mut by_id: HashMap<String, FloorEntry> = data
                .floors
                .drain(..)
                .map(|entry| (entry.floor_id.clone(), entry))
                .collect();
            data.floors = floor_ids
                .iter()
                .map(|id| by_id.remove(id).expect("validated floor ID"))
                .collect();
            Ok((
                (),
                vec![RegistryEvent::reorder("floor_registry_updated", "floor_id")],
            ))
        })
    }

    /// Create a label with a stable slug-based ID.
    pub fn create_label(&self, input: LabelInput) -> Result<LabelEntry> {
        self.transact(|data| {
            validate_name(&input.name, "label name")?;
            validate_label_color(input.color.as_deref())?;
            validate_icon(input.icon.as_deref())?;
            ensure_unique_name(
                data.labels.iter().map(|entry| &entry.name),
                &input.name,
                "label",
            )?;
            let now = Utc::now();
            let id = generated_name_id(
                &input.name,
                data.labels.iter().map(|entry| entry.label_id.as_str()),
            );
            let entry = LabelEntry {
                label_id: id.clone(),
                name: input.name.clone(),
                color: input.color.clone(),
                description: input.description.clone(),
                icon: input.icon.as_deref().map(str::trim).map(str::to_owned),
                created_at: now,
                modified_at: now,
            };
            data.labels.push(entry.clone());
            Ok((
                entry,
                vec![RegistryEvent::item(
                    "label_registry_updated",
                    "create",
                    "label_id",
                    &id,
                )],
            ))
        })
    }

    /// Update a label, preserving its stable ID.
    pub fn update_label(&self, label_id: &str, update: LabelUpdate) -> Result<LabelEntry> {
        self.transact(|data| {
            let index = data
                .labels
                .iter()
                .position(|entry| entry.label_id == label_id)
                .ok_or_else(|| Error::NotFound(format!("label {label_id}")))?;
            let old = data.labels[index].clone();
            let mut new = old.clone();
            if let Some(value) = update.color {
                validate_label_color(value.as_deref())?;
                new.color = value;
            }
            if let Some(value) = update.description {
                new.description = value;
            }
            if let Some(value) = update.icon {
                validate_icon(value.as_deref())?;
                new.icon = value.as_deref().map(str::trim).map(str::to_owned);
            }
            if let Some(value) = update.name {
                new.name = value;
            }
            if new.name != old.name {
                validate_name(&new.name, "label name")?;
                if normalized_name(&new.name) != normalized_name(&old.name) {
                    ensure_unique_name(
                        data.labels.iter().map(|entry| &entry.name),
                        &new.name,
                        "label",
                    )?;
                }
            }
            if new == old {
                return Ok((old, Vec::new()));
            }
            new.modified_at = Utc::now();
            data.labels[index] = new.clone();
            Ok((
                new,
                vec![RegistryEvent::item(
                    "label_registry_updated",
                    "update",
                    "label_id",
                    label_id,
                )],
            ))
        })
    }

    /// Delete a label and remove it from all associated entries.
    pub fn delete_label(&self, label_id: &str) -> Result<()> {
        self.transact(|data| {
            let index = data
                .labels
                .iter()
                .position(|entry| entry.label_id == label_id)
                .ok_or_else(|| Error::NotFound(format!("label {label_id}")))?;
            data.labels.remove(index);
            let mut events = Vec::new();
            for entity in &mut data.entities {
                if entity.labels.contains(label_id) {
                    let old_labels = entity.labels.clone();
                    entity.labels.remove(label_id);
                    entity.modified_at = Utc::now();
                    events.push(RegistryEvent::changed(
                        "entity_registry_updated",
                        "entity_id",
                        &entity.entity_id,
                        json!({"labels": old_labels}),
                    ));
                }
            }
            for entity in &mut data.deleted_entities {
                if entity.labels.remove(label_id) {
                    entity.modified_at = Utc::now();
                }
            }
            for device in &mut data.devices {
                if device.labels.contains(label_id) {
                    let old_labels = device.labels.clone();
                    device.labels.remove(label_id);
                    device.modified_at = Utc::now();
                    events.push(RegistryEvent::changed(
                        "device_registry_updated",
                        "device_id",
                        &device.id,
                        json!({"labels": old_labels}),
                    ));
                }
            }
            for device in &mut data.deleted_devices {
                if device.labels.remove(label_id) {
                    device.modified_at = Utc::now();
                }
            }
            for area in &mut data.areas {
                if area.labels.remove(label_id) {
                    area.modified_at = Utc::now();
                    events.push(RegistryEvent::item(
                        "area_registry_updated",
                        "update",
                        "area_id",
                        &area.id,
                    ));
                }
            }
            events.push(RegistryEvent::item(
                "label_registry_updated",
                "remove",
                "label_id",
                label_id,
            ));
            Ok(((), events))
        })
    }

    /// Handle a Home Assistant registry websocket command and return its result body.
    /// The websocket adapter adds the protocol envelope and command ID.
    pub fn handle_ws(&self, command: &Value) -> Result<Value> {
        websocket::handle(self, command)
    }

    fn snapshot(&self) -> RegistryData {
        self.inner
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn transact<T>(
        &self,
        change: impl FnOnce(&mut RegistryData) -> Result<(T, Vec<RegistryEvent>)>,
    ) -> Result<T> {
        let _order = self
            .inner
            .write_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.snapshot();
        let mut next = previous.clone();
        let (result, events) = change(&mut next)?;
        if next == previous {
            return Ok(result);
        }
        validate_data(&next)?;
        let serialized = serde_json::to_value(&next)
            .map_err(|error| Error::Storage(format!("serialize registry snapshot: {error}")))?;
        self.inner.runtime.set_data(STORAGE_KEY, &serialized)?;
        *self
            .inner
            .data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        let mut emitted = false;
        for event in events {
            if let Err(error) = self.inner.runtime.fire_event(
                event.event_type,
                event.websocket_data(),
                Context::default(),
            ) {
                // Roll back if no notification was published. Once an earlier
                // event is committed, keep the snapshot aligned with it.
                if !emitted
                    && let Ok(previous_value) = serde_json::to_value(&previous)
                    && self
                        .inner
                        .runtime
                        .set_data(STORAGE_KEY, &previous_value)
                        .is_ok()
                {
                    *self
                        .inner
                        .data
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = previous;
                }
                return Err(error);
            }
            emitted = true;
        }
        Ok(result)
    }
}

fn remove_entity_from_data(data: &mut RegistryData, entity_id: &str) -> Result<Vec<RegistryEvent>> {
    let index = data
        .entities
        .iter()
        .position(|entry| entry.entity_id == entity_id)
        .ok_or_else(|| Error::NotFound(format!("entity {entity_id}")))?;
    let removed = data.entities.remove(index);
    data.deleted_entities
        .retain(|entry| !entity_identity_matches_entry(entry, &removed));
    data.deleted_entities.push(removed.clone());
    let mut events = vec![RegistryEvent::item(
        "entity_registry_updated",
        "remove",
        "entity_id",
        &removed.entity_id,
    )];
    for area in &mut data.areas {
        let mut changed = false;
        if area.humidity_entity_id.as_deref() == Some(entity_id) {
            area.humidity_entity_id = None;
            changed = true;
        }
        if area.temperature_entity_id.as_deref() == Some(entity_id) {
            area.temperature_entity_id = None;
            changed = true;
        }
        if changed {
            area.modified_at = Utc::now();
            events.push(RegistryEvent::item(
                "area_registry_updated",
                "update",
                "area_id",
                &area.id,
            ));
        }
    }
    Ok(events)
}

fn remove_device_from_data(data: &mut RegistryData, device_id: &str) -> Result<Vec<RegistryEvent>> {
    if data
        .devices
        .iter()
        .any(|entry| entry.composite_device_id.as_deref() == Some(device_id))
    {
        return Err(Error::NotAllowed("Cannot remove a composite device".into()));
    }
    let index = data
        .devices
        .iter()
        .position(|entry| entry.id == device_id)
        .ok_or_else(|| Error::NotFound(format!("device {device_id}")))?;
    let removed = data.devices.remove(index);
    data.deleted_devices.retain(|entry| entry.id != device_id);
    data.deleted_devices.push(removed.clone());
    let mut events = vec![RegistryEvent {
        event_type: "device_registry_updated",
        action: "remove",
        id_key: Some("device_id"),
        id: Some(device_id.to_owned()),
        changes: None,
        extra: Some(("device", removed.websocket_value())),
    }];
    for entity in &mut data.entities {
        if entity.device_id.as_deref() == Some(device_id) {
            entity.device_id = None;
            entity.modified_at = Utc::now();
            events.push(RegistryEvent::changed(
                "entity_registry_updated",
                "entity_id",
                &entity.entity_id,
                json!({"device_id": device_id}),
            ));
        }
    }
    for entity in &mut data.deleted_entities {
        if entity.device_id.as_deref() == Some(device_id) {
            entity.device_id = None;
            entity.modified_at = Utc::now();
        }
    }
    for device in &mut data.devices {
        if device.via_device_id.as_deref() == Some(device_id) {
            device.via_device_id = None;
            device.modified_at = Utc::now();
            events.push(RegistryEvent::changed(
                "device_registry_updated",
                "device_id",
                &device.id,
                json!({"via_device_id": device_id}),
            ));
        }
    }
    for device in &mut data.deleted_devices {
        if device.via_device_id.as_deref() == Some(device_id) {
            device.via_device_id = None;
            device.modified_at = Utc::now();
        }
    }
    Ok(events)
}

fn orphan_deleted_device(
    data: &mut RegistryData,
    device_id: &str,
    domain: Option<&str>,
    now: DateTime<Utc>,
) -> bool {
    if let Some(domain) = domain
        && let Some(removed) = data
            .deleted_devices
            .iter()
            .find(|entry| entry.id == device_id)
    {
        let identifiers = removed.identifiers.clone();
        let connections = removed.connections.clone();
        data.deleted_devices.retain(|existing| {
            existing.id == device_id
                || existing.orphaned_timestamp.is_none()
                || existing.domain.as_deref() != Some(domain)
                || (existing.identifiers.is_disjoint(&identifiers)
                    && existing.connections.is_disjoint(&connections))
        });
    }

    let Some(entry) = data
        .deleted_devices
        .iter_mut()
        .find(|entry| entry.id == device_id)
    else {
        return false;
    };
    entry.config_entry_id = None;
    entry.config_entries.clear();
    entry.config_entries_subentries.clear();
    entry.config_subentry_id = None;
    entry.domain = domain.map(str::to_owned);
    entry.orphaned_timestamp = Some(now);
    true
}

// Helpers used by websocket adapters and registry mutations.
fn timestamp(value: DateTime<Utc>) -> f64 {
    value.timestamp_millis() as f64 / 1000.0
}
fn next_name_part(area_id: Option<&str>, device_id: Option<&str>) -> Option<&'static str> {
    if area_id.is_some() {
        Some("area")
    } else if device_id.is_some() {
        Some("device")
    } else {
        None
    }
}
fn new_hex_id() -> String {
    Uuid::new_v4().simple().to_string()
}
fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}
fn validate_nonempty(value: &str, what: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(invalid(format!("{what} cannot be empty")))
    } else {
        Ok(())
    }
}
fn validate_name(value: &str, what: &str) -> Result<()> {
    validate_nonempty(value, what)
}
fn validate_user_flag(value: Option<&str>, field: &str) -> Result<()> {
    if value.is_some_and(|value| value != "user") {
        return Err(invalid(format!("{field} can only be set to user or null")));
    }
    Ok(())
}
fn validate_disabler(value: Option<&str>) -> Result<()> {
    if value
        .is_some_and(|value| !["user", "integration", "config_entry", "device"].contains(&value))
    {
        return Err(invalid("disabled_by is not a recognized registry disabler"));
    }
    Ok(())
}
fn validate_entity_id(value: &str) -> Result<()> {
    let Some((domain, object)) = value.split_once('.') else {
        return Err(invalid("entity_id must contain one domain separator"));
    };
    if object.contains('.') || !valid_slug(domain) || !valid_slug(object) {
        return Err(invalid(format!("invalid entity ID {value}")));
    }
    Ok(())
}
fn entity_domain(entity_id: &str) -> &str {
    entity_id.split_once('.').map_or("", |(domain, _)| domain)
}
fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for c in value.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if separator && !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c);
            separator = false;
        } else {
            separator = true;
        }
    }
    let out = out.trim_matches('_').to_owned();
    if out.is_empty() {
        "unnamed".to_owned()
    } else {
        out
    }
}
fn normalized_name(value: &str) -> String {
    value.to_lowercase().replace(' ', "")
}
fn generated_name_id<'a>(name: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let existing: HashSet<&str> = existing.collect();
    let base = slugify(name);
    if !existing.contains(base.as_str()) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}_{suffix}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
    }
}
fn clean_aliases(values: &BTreeSet<String>) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}
fn validate_aliases(values: &[Value]) -> Result<()> {
    if values
        .iter()
        .any(|value| !value.is_null() && !value.is_string())
    {
        return Err(invalid("aliases must contain only strings or null"));
    }
    Ok(())
}
fn clean_entity_aliases(values: Vec<Value>) -> Result<Vec<Value>> {
    validate_aliases(&values)?;
    Ok(values
        .into_iter()
        .filter_map(|value| {
            if value.is_null() {
                Some(Value::Null)
            } else {
                let alias = value.as_str().unwrap_or_default().trim();
                (!alias.is_empty()).then(|| Value::String(alias.to_owned()))
            }
        })
        .collect())
}
fn validate_label_color(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        let color = value.strip_prefix('#');
        let valid_hex = color
            .is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let named = [
            "primary",
            "accent",
            "disabled",
            "red",
            "pink",
            "purple",
            "deep-purple",
            "indigo",
            "blue",
            "light-blue",
            "cyan",
            "teal",
            "green",
            "light-green",
            "lime",
            "yellow",
            "amber",
            "orange",
            "deep-orange",
            "brown",
            "light-grey",
            "grey",
            "dark-grey",
            "blue-grey",
            "black",
            "white",
        ];
        if !valid_hex && !named.contains(&value) {
            return Err(invalid(
                "color must be a six digit hex color or supported theme color",
            ));
        }
    }
    Ok(())
}
fn validate_icon(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|icon| {
        let icon = icon.trim();
        !icon.contains(':') || icon.chars().any(char::is_whitespace)
    }) {
        return Err(invalid(
            "icon must use the prefix:name format without whitespace",
        ));
    }
    Ok(())
}
fn validate_registration(input: &EntityRegistration) -> Result<()> {
    validate_nonempty(&input.domain, "entity domain")?;
    validate_nonempty(&input.platform, "entity platform")?;
    validate_nonempty(&input.unique_id, "entity unique_id")?;
    if !valid_slug(&input.domain) {
        return Err(invalid(
            "entity domain must be lowercase alphanumeric or underscore",
        ));
    }
    validate_disabler(input.disabled_by.as_deref())?;
    Ok(())
}
fn validate_device_registration(input: &DeviceRegistration) -> Result<()> {
    if input
        .entry_type
        .as_deref()
        .is_some_and(|kind| kind != "service")
    {
        return Err(invalid("device entry_type must be service or absent"));
    }
    if input.identifiers.is_empty() && input.connections.is_empty() {
        return Err(invalid(
            "a device requires at least one identifier or connection",
        ));
    }
    for (kind, value) in input.identifiers.iter().chain(input.connections.iter()) {
        validate_nonempty(kind, "device identity type")?;
        validate_nonempty(value, "device identity value")?;
    }
    Ok(())
}
fn entity_identity_matches(entry: &EntityEntry, registration: &EntityRegistration) -> bool {
    entry.domain == registration.domain
        && entry.platform == registration.platform
        && entry.unique_id == registration.unique_id
}
fn device_in_config_entry(entry: &DeviceEntry, config_entry_id: Option<&str>) -> bool {
    match config_entry_id {
        Some(config_entry_id) => {
            entry.config_entry_id.as_deref() == Some(config_entry_id)
                || entry
                    .config_entries
                    .iter()
                    .any(|entry_id| entry_id == config_entry_id)
        }
        None => entry.config_entry_id.is_none() && entry.config_entries.is_empty(),
    }
}

fn composite_split_for_config_entry<'a>(
    data: &'a RegistryData,
    composite_device_id: &str,
    config_entry_id: Option<&str>,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> Option<&'a DeviceEntry> {
    composite_split_from_entries(
        &data.devices,
        composite_device_id,
        config_entry_id,
        resolver,
    )
}

fn composite_split_from_entries<'a>(
    devices: &'a [DeviceEntry],
    composite_device_id: &str,
    config_entry_id: Option<&str>,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> Option<&'a DeviceEntry> {
    config_entry_id
        .and_then(|config_entry_id| {
            devices.iter().find(|entry| {
                entry.composite_device_id.as_deref() == Some(composite_device_id)
                    && entry.config_entry_id.as_deref() == Some(config_entry_id)
            })
        })
        .or_else(|| {
            let resolver = resolver?;
            let config_entry_id = config_entry_id?;
            let domain = resolver(config_entry_id)?;
            devices.iter().find(|entry| {
                entry.composite_device_id.as_deref() == Some(composite_device_id)
                    && entry
                        .config_entry_id
                        .as_deref()
                        .and_then(resolver)
                        .is_some_and(|entry_domain| entry_domain == domain)
            })
        })
        .or_else(|| {
            devices
                .iter()
                .find(|entry| entry.composite_device_id.as_deref() == Some(composite_device_id))
        })
}

fn resolve_via_device_id(
    data: &RegistryData,
    via_device_id: &str,
    config_entry_id: Option<&str>,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> String {
    if data.devices.iter().any(|entry| entry.id == via_device_id) {
        return via_device_id.to_owned();
    }
    composite_split_for_config_entry(data, via_device_id, config_entry_id, resolver)
        .map(|entry| entry.id.clone())
        .unwrap_or_else(|| via_device_id.to_owned())
}

fn reconcile_via_device_references(
    data: &mut RegistryData,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> Vec<RegistryEvent> {
    let split_members = data.devices.clone();
    let live_ids = data
        .devices
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let all_ids = data
        .devices
        .iter()
        .chain(data.deleted_devices.iter())
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let mut events = Vec::new();
    for device in &mut data.devices {
        let Some(old_via_device_id) = device.via_device_id.clone() else {
            continue;
        };
        let new_via_device_id = if old_via_device_id == device.id {
            None
        } else if live_ids.contains(&old_via_device_id) {
            Some(old_via_device_id)
        } else {
            composite_split_from_entries(
                &split_members,
                &old_via_device_id,
                device.config_entry_id.as_deref(),
                resolver,
            )
            .map(|parent| parent.id.clone())
            .filter(|parent_id| parent_id != &device.id)
        };
        if device.via_device_id != new_via_device_id {
            let previous = device.clone();
            device.via_device_id = new_via_device_id;
            events.push(RegistryEvent::changed(
                "device_registry_updated",
                "device_id",
                &device.id,
                device_change_values(&previous, device),
            ));
        }
    }
    for device in &mut data.deleted_devices {
        let Some(old_via_device_id) = device.via_device_id.clone() else {
            continue;
        };
        if old_via_device_id == device.id {
            device.via_device_id = None;
        } else if !all_ids.contains(&old_via_device_id) {
            device.via_device_id = composite_split_from_entries(
                &split_members,
                &old_via_device_id,
                device.config_entry_id.as_deref(),
                resolver,
            )
            .map(|parent| parent.id.clone())
            .filter(|parent_id| parent_id != &device.id);
        }
    }
    events
}

fn legacy_device_config_pairs(entry: &DeviceEntry) -> Result<Vec<(String, Option<String>)>> {
    let mut pairs = Vec::new();
    if !entry.config_entries_subentries.is_empty() {
        for (config_entry_id, subentries) in &entry.config_entries_subentries {
            validate_nonempty(config_entry_id, "device config entry ID")?;
            if subentries.iter().flatten().any(String::is_empty) {
                return Err(invalid("device config subentry IDs cannot be empty"));
            }
            let subentry_id = subentries.iter().find_map(Option::as_ref).cloned();
            if subentry_id.as_deref().is_some_and(str::is_empty) {
                return Err(invalid("device config subentry IDs cannot be empty"));
            }
            pairs.push((config_entry_id.clone(), subentry_id));
        }
    } else if !entry.config_entries.is_empty() {
        for config_entry_id in &entry.config_entries {
            validate_nonempty(config_entry_id, "device config entry ID")?;
            let subentry_id = (entry.config_entry_id.as_deref() == Some(config_entry_id))
                .then(|| entry.config_subentry_id.clone())
                .flatten();
            pairs.push((config_entry_id.clone(), subentry_id));
        }
    } else if let Some(config_entry_id) = &entry.config_entry_id {
        validate_nonempty(config_entry_id, "device config entry ID")?;
        if entry
            .config_subentry_id
            .as_deref()
            .is_some_and(str::is_empty)
        {
            return Err(invalid("device config subentry IDs cannot be empty"));
        }
        pairs.push((config_entry_id.clone(), entry.config_subentry_id.clone()));
    }
    let unique_entries: HashSet<&str> = pairs
        .iter()
        .map(|(config_entry_id, _)| config_entry_id.as_str())
        .collect();
    if unique_entries.len() != pairs.len() {
        return Err(invalid(
            "legacy device can have only one split per config entry",
        ));
    }
    Ok(pairs)
}

fn migrate_legacy_devices(
    data: &mut RegistryData,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> Result<()> {
    let legacy_ids = data
        .devices
        .iter()
        .filter(|entry| entry.composite_device_id.is_none())
        .map(|entry| Ok((entry.id.clone(), legacy_device_config_pairs(entry)?)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(device_id, pairs)| (!pairs.is_empty()).then_some(device_id))
        .collect::<Vec<_>>();
    for device_id in legacy_ids {
        split_legacy_device_in_data(data, &device_id, resolver)?;
    }
    migrate_legacy_deleted_devices(data)?;
    reconcile_via_device_references(data, resolver);
    Ok(())
}

fn retain_legacy_owner_subentries(target: &mut DeviceEntry, original: &DeviceEntry) {
    // The scalar field represents one association. Earlier native snapshots
    // also permit multiple subentries for an owner; retain that owner's full
    // vector rather than discarding valid associations during migration.
    target.config_entries.clear();
    target.config_entries_subentries = target
        .config_entry_id
        .as_ref()
        .and_then(|owner| {
            original
                .config_entries_subentries
                .get(owner)
                .filter(|subentries| subentries.len() > 1)
                .map(|subentries| (owner.clone(), subentries.clone()))
        })
        .into_iter()
        .collect();
}

fn migrate_legacy_deleted_devices(data: &mut RegistryData) -> Result<()> {
    let mut unavailable_ids = data
        .devices
        .iter()
        .chain(data.deleted_devices.iter())
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let mut migrated = Vec::new();
    for old in std::mem::take(&mut data.deleted_devices) {
        let pairs = legacy_device_config_pairs(&old)?;
        if pairs.len() <= 1 {
            let mut normalized = old.clone();
            if let Some((config_entry_id, config_subentry_id)) = pairs.into_iter().next() {
                normalized.config_entry_id = Some(config_entry_id);
                normalized.config_subentry_id = config_subentry_id;
            }
            retain_legacy_owner_subentries(&mut normalized, &old);
            normalized.composite_device_id = None;
            normalized.composite_primary_config_entry = None;
            normalized.split_at = None;
            normalized.has_composite_identifiers = false;
            migrated.push(normalized);
            continue;
        }

        for (index, (config_entry_id, config_subentry_id)) in pairs.into_iter().enumerate() {
            let mut split = old.clone();
            if index > 0 {
                split.id = loop {
                    let candidate = new_hex_id();
                    if unavailable_ids.insert(candidate.clone()) {
                        break candidate;
                    }
                };
            }
            split.config_entry_id = Some(config_entry_id);
            split.config_subentry_id = config_subentry_id;
            retain_legacy_owner_subentries(&mut split, &old);
            split.composite_device_id = None;
            split.composite_primary_config_entry = None;
            split.split_at = None;
            split.has_composite_identifiers = false;
            migrated.push(split);
        }
    }
    data.deleted_devices = migrated;
    Ok(())
}

fn split_legacy_device_in_data(
    data: &mut RegistryData,
    composite_device_id: &str,
    resolver: Option<&ConfigEntryDomainResolver>,
) -> Result<(Vec<DeviceEntry>, Vec<RegistryEvent>)> {
    let index = data
        .devices
        .iter()
        .position(|entry| entry.id == composite_device_id)
        .ok_or_else(|| Error::NotFound(format!("device {composite_device_id}")))?;
    let old = data.devices[index].clone();
    if old.composite_device_id.is_some() {
        return Err(invalid("a split device cannot be split again"));
    }
    let pairs = legacy_device_config_pairs(&old)?;
    if pairs.is_empty() {
        return Err(invalid("legacy device has no config entry to split"));
    }
    if pairs.len() == 1 {
        let (config_entry_id, config_subentry_id) = pairs[0].clone();
        let mut normalized = old.clone();
        normalized.config_entry_id = Some(config_entry_id);
        normalized.config_subentry_id = config_subentry_id;
        retain_legacy_owner_subentries(&mut normalized, &old);
        if normalized == old {
            return Ok((vec![old], Vec::new()));
        }
        let changes = device_change_values(&old, &normalized);
        data.devices[index] = normalized.clone();
        return Ok((
            vec![normalized.clone()],
            vec![RegistryEvent::changed(
                "device_registry_updated",
                "device_id",
                &normalized.id,
                changes,
            )],
        ));
    }

    let mut unavailable_ids: HashSet<String> = data
        .devices
        .iter()
        .chain(data.deleted_devices.iter())
        .map(|entry| entry.id.clone())
        .collect();
    let split_at = Utc::now();
    let primary_config_entry = old.config_entry_id.clone();
    let mut split_devices = Vec::with_capacity(pairs.len());
    for (config_entry_id, config_subentry_id) in pairs {
        let mut split = old.clone();
        let id = loop {
            let candidate = new_hex_id();
            if unavailable_ids.insert(candidate.clone()) {
                break candidate;
            }
        };
        split.id = id;
        split.config_entry_id = Some(config_entry_id);
        split.config_subentry_id = config_subentry_id;
        retain_legacy_owner_subentries(&mut split, &old);
        split.composite_device_id = Some(composite_device_id.to_owned());
        split.composite_primary_config_entry = primary_config_entry.clone();
        split.split_at = Some(split_at);
        split.has_composite_identifiers = true;
        split_devices.push(split);
    }

    let split_for_entity = |entity: &EntityEntry| {
        entity
            .config_entry_id
            .as_deref()
            .and_then(|config_entry_id| {
                split_devices
                    .iter()
                    .find(|split| {
                        split.config_entry_id.as_deref() == Some(config_entry_id)
                            && split.config_subentry_id == entity.config_subentry_id
                    })
                    .or_else(|| {
                        split_devices
                            .iter()
                            .find(|split| split.config_entry_id.as_deref() == Some(config_entry_id))
                    })
            })
            .map(|split| split.id.clone())
    };
    let split_for_config_entry = |config_entry_id: Option<&str>| {
        composite_split_from_entries(
            &split_devices,
            composite_device_id,
            config_entry_id,
            resolver,
        )
        .map(|split| split.id.clone())
    };

    let mut events = vec![RegistryEvent {
        event_type: "device_registry_updated",
        action: "remove",
        id_key: Some("device_id"),
        id: Some(composite_device_id.to_owned()),
        changes: None,
        extra: Some(("device", old.websocket_value())),
    }];
    let mut entity_events = Vec::new();
    let mut parent_events = Vec::new();
    for entity in &mut data.entities {
        if entity.device_id.as_deref() != Some(composite_device_id) {
            continue;
        }
        let previous = entity.clone();
        entity.device_id = split_for_entity(entity);
        if entity.device_id != previous.device_id {
            entity_events.push(RegistryEvent::changed(
                "entity_registry_updated",
                "entity_id",
                &entity.entity_id,
                entity_change_values(&previous, entity),
            ));
        }
    }
    for entity in &mut data.deleted_entities {
        if entity.device_id.as_deref() == Some(composite_device_id) {
            entity.device_id = split_for_entity(entity);
        }
    }

    for device in &mut data.devices {
        if device.id == composite_device_id
            || device.via_device_id.as_deref() != Some(composite_device_id)
        {
            continue;
        }
        let previous = device.clone();
        device.via_device_id = split_for_config_entry(device.config_entry_id.as_deref());
        if device.via_device_id != previous.via_device_id {
            parent_events.push(RegistryEvent::changed(
                "device_registry_updated",
                "device_id",
                &device.id,
                device_change_values(&previous, device),
            ));
        }
    }
    for device in &mut data.deleted_devices {
        if device.via_device_id.as_deref() == Some(composite_device_id) {
            device.via_device_id = split_for_config_entry(device.config_entry_id.as_deref());
        }
    }

    data.devices.splice(index..=index, split_devices.clone());
    for split in &split_devices {
        events.push(RegistryEvent::item(
            "device_registry_updated",
            "create",
            "device_id",
            &split.id,
        ));
    }
    events.extend(entity_events);
    events.extend(parent_events);
    let members = data
        .devices
        .iter()
        .filter(|entry| entry.composite_device_id.as_deref() == Some(composite_device_id))
        .cloned()
        .collect();
    Ok((members, events))
}

fn entity_identity_matches_entry(left: &EntityEntry, right: &EntityEntry) -> bool {
    left.domain == right.domain
        && left.platform == right.platform
        && left.unique_id == right.unique_id
}
fn entity_id_available(data: &RegistryData, entity_id: &str, runtime: &Runtime) -> bool {
    !data
        .entities
        .iter()
        .any(|entry| entry.entity_id == entity_id)
        && !data
            .deleted_entities
            .iter()
            .any(|entry| entry.entity_id == entity_id)
        && runtime.get_state(entity_id).is_none()
}
fn generate_entity_id(
    data: &RegistryData,
    registration: &EntityRegistration,
    runtime: &Runtime,
) -> String {
    let fallback = format!("{}_{}", registration.platform, registration.unique_id);
    let base = registration
        .name
        .as_deref()
        .or(registration.suggested_object_id.as_deref())
        .or(registration.original_name.as_deref())
        .unwrap_or(&fallback);
    let object = slugify(base);
    let base_id = format!("{}.{}", registration.domain, object);
    if entity_id_available(data, &base_id, runtime) {
        return base_id;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{}.{}_{}", registration.domain, object, suffix);
        if entity_id_available(data, &candidate, runtime) {
            return candidate;
        }
        suffix += 1;
    }
}
fn ensure_unique_name<'a>(
    names: impl Iterator<Item = &'a String>,
    name: &str,
    kind: &str,
) -> Result<()> {
    let normalized = normalized_name(name);
    if names
        .into_iter()
        .any(|candidate| normalized_name(candidate) == normalized)
    {
        return Err(invalid(format!("the {kind} name {name} is already in use")));
    }
    Ok(())
}
fn validate_reorder<'a>(
    ids: &[String],
    current: impl Iterator<Item = &'a str>,
    name: &str,
) -> Result<()> {
    let current: HashSet<&str> = current.collect();
    let requested: HashSet<&str> = ids.iter().map(String::as_str).collect();
    if ids.len() != current.len() || requested.len() != ids.len() || requested != current {
        return Err(invalid(format!(
            "{name} must contain all existing IDs exactly once"
        )));
    }
    Ok(())
}
fn entity_change_values(old: &EntityEntry, new: &EntityEntry) -> Value {
    let mut changes = Map::new();
    macro_rules! changed {
        ($field:ident) => {
            if old.$field != new.$field {
                changes.insert(stringify!($field).into(), json!(old.$field));
            }
        };
    }
    changed!(area_id);
    changed!(categories);
    changed!(config_entry_id);
    changed!(config_subentry_id);
    changed!(device_class);
    changed!(device_id);
    changed!(disabled_by);
    changed!(hidden_by);
    changed!(icon);
    changed!(labels);
    changed!(name);
    changed!(options);
    changed!(entity_id);
    changed!(aliases);
    changed!(original_device_class);
    changed!(original_icon);
    changed!(original_name);
    changed!(supported_features);
    changed!(translation_key);
    changed!(unit_of_measurement);
    Value::Object(changes)
}
fn device_change_values(old: &DeviceEntry, new: &DeviceEntry) -> Value {
    let mut changes = Map::new();
    macro_rules! changed {
        ($field:ident) => {
            if old.$field != new.$field {
                changes.insert(stringify!($field).into(), json!(old.$field));
            }
        };
    }
    changed!(area_id);
    changed!(config_entry_id);
    changed!(config_subentry_id);
    changed!(config_entries);
    changed!(config_entries_subentries);
    changed!(composite_device_id);
    changed!(composite_primary_config_entry);
    changed!(disabled_by);
    changed!(labels);
    changed!(name_by_user);
    changed!(name);
    changed!(identifiers);
    changed!(connections);
    changed!(via_device_id);
    changed!(configuration_url);
    changed!(entry_type);
    changed!(hw_version);
    changed!(manufacturer);
    changed!(model);
    changed!(model_id);
    changed!(serial_number);
    changed!(split_at);
    changed!(has_composite_identifiers);
    changed!(sw_version);
    Value::Object(changes)
}
fn validate_data(data: &RegistryData) -> Result<()> {
    if data.version != REGISTRY_VERSION {
        return Err(invalid("unsupported registry snapshot version"));
    }
    let mut entity_ids = HashSet::new();
    let mut entity_entry_ids = HashSet::new();
    let mut identities = HashSet::new();
    let device_ids: HashSet<&str> = data.devices.iter().map(|entry| entry.id.as_str()).collect();
    if device_ids.len() != data.devices.len() {
        return Err(invalid("duplicate device ID"));
    }
    let all_device_ids: HashSet<&str> = data
        .devices
        .iter()
        .chain(data.deleted_devices.iter())
        .map(|entry| entry.id.as_str())
        .collect();
    if all_device_ids.len() != data.devices.len() + data.deleted_devices.len() {
        return Err(invalid("duplicate active or removed device ID"));
    }
    let mut composite_metadata: HashMap<&str, (Option<&str>, &DateTime<Utc>)> = HashMap::new();
    for device in data.devices.iter().chain(data.deleted_devices.iter()) {
        let Some(composite_device_id) = device.composite_device_id.as_deref() else {
            if device.composite_primary_config_entry.is_some() || device.split_at.is_some() {
                return Err(invalid(
                    "composite split metadata requires a composite device ID",
                ));
            }
            continue;
        };
        validate_nonempty(composite_device_id, "composite device ID")?;
        if all_device_ids.contains(composite_device_id) {
            return Err(invalid(
                "composite device ID must not identify a registered device",
            ));
        }
        if device
            .composite_primary_config_entry
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(invalid("composite primary config entry cannot be empty"));
        }
        let split_at = device
            .split_at
            .as_ref()
            .ok_or_else(|| invalid("composite split metadata requires split_at"))?;
        let metadata = (device.composite_primary_config_entry.as_deref(), split_at);
        if composite_metadata
            .get(composite_device_id)
            .is_some_and(|existing| *existing != metadata)
        {
            return Err(invalid(
                "all members of a composite split must share primary entry and split time",
            ));
        }
        composite_metadata.insert(composite_device_id, metadata);
    }
    let area_ids: HashSet<&str> = data.areas.iter().map(|entry| entry.id.as_str()).collect();
    if area_ids.len() != data.areas.len() {
        return Err(invalid("duplicate area ID"));
    }
    let floor_ids: HashSet<&str> = data
        .floors
        .iter()
        .map(|entry| entry.floor_id.as_str())
        .collect();
    if floor_ids.len() != data.floors.len() {
        return Err(invalid("duplicate floor ID"));
    }
    let label_ids: HashSet<&str> = data
        .labels
        .iter()
        .map(|entry| entry.label_id.as_str())
        .collect();
    if label_ids.len() != data.labels.len() {
        return Err(invalid("duplicate label ID"));
    }
    for entry in data.entities.iter().chain(data.deleted_entities.iter()) {
        validate_entity_id(&entry.entity_id)?;
        if entity_domain(&entry.entity_id) != entry.domain {
            return Err(invalid("entity ID domain does not match its domain field"));
        }
        if entry.id.is_empty() || !entity_entry_ids.insert(entry.id.as_str()) {
            return Err(invalid("duplicate or empty entity entry ID"));
        }
        if !entity_ids.insert(entry.entity_id.as_str()) {
            return Err(invalid("duplicate entity ID"));
        }
        if !identities.insert((
            entry.domain.as_str(),
            entry.platform.as_str(),
            entry.unique_id.as_str(),
        )) {
            return Err(invalid("duplicate entity unique-ID identity"));
        }
        if entry
            .device_id
            .as_deref()
            .is_some_and(|id| !device_ids.contains(id))
        {
            return Err(invalid(format!(
                "entity {} references a missing device",
                entry.entity_id
            )));
        }
        if entry
            .area_id
            .as_deref()
            .is_some_and(|id| !area_ids.contains(id))
        {
            return Err(invalid(format!(
                "entity {} references a missing area",
                entry.entity_id
            )));
        }
        if entry
            .labels
            .iter()
            .any(|id| !label_ids.contains(id.as_str()))
        {
            return Err(invalid(format!(
                "entity {} references a missing label",
                entry.entity_id
            )));
        }
        validate_aliases(&entry.aliases)?;
    }
    let mut identifier_owners: HashMap<(Option<&str>, &(String, String)), &str> = HashMap::new();
    let mut connection_owners: HashMap<(Option<&str>, &(String, String)), &str> = HashMap::new();
    for device in &data.devices {
        if device.id.is_empty() {
            return Err(invalid("device ID cannot be empty"));
        }
        if device
            .area_id
            .as_deref()
            .is_some_and(|id| !area_ids.contains(id))
        {
            return Err(invalid(format!(
                "device {} references a missing area",
                device.id
            )));
        }
        if device
            .via_device_id
            .as_deref()
            .is_some_and(|id| !device_ids.contains(id) || id == device.id)
        {
            return Err(invalid(format!(
                "device {} references a missing or self parent device",
                device.id
            )));
        }
        if device
            .labels
            .iter()
            .any(|id| !label_ids.contains(id.as_str()))
        {
            return Err(invalid(format!(
                "device {} references a missing label",
                device.id
            )));
        }
        if device.config_entry_id.as_deref().is_some_and(str::is_empty)
            || device
                .config_subentry_id
                .as_deref()
                .is_some_and(str::is_empty)
        {
            return Err(invalid("device config entry IDs cannot be empty"));
        }
        let unique_config_entries: HashSet<&str> =
            device.config_entries.iter().map(String::as_str).collect();
        if unique_config_entries.len() != device.config_entries.len() {
            return Err(invalid("device config entries must be unique"));
        }
        let namespaces: Vec<Option<&str>> = if device.config_entries.is_empty() {
            vec![device.config_entry_id.as_deref()]
        } else {
            device
                .config_entries
                .iter()
                .map(String::as_str)
                .map(Some)
                .collect()
        };
        for namespace in namespaces {
            for identity in &device.identifiers {
                if identifier_owners
                    .insert((namespace, identity), device.id.as_str())
                    .is_some_and(|old| old != device.id)
                {
                    return Err(invalid(
                        "device identifier is already registered in this config entry",
                    ));
                }
            }
            for identity in &device.connections {
                if connection_owners
                    .insert((namespace, identity), device.id.as_str())
                    .is_some_and(|old| old != device.id)
                {
                    return Err(invalid(
                        "device connection is already registered in this config entry",
                    ));
                }
            }
        }
    }
    for device in &data.deleted_devices {
        if device.id.is_empty() {
            return Err(invalid("removed device ID cannot be empty"));
        }
        if device
            .area_id
            .as_deref()
            .is_some_and(|id| !area_ids.contains(id))
        {
            return Err(invalid("removed device references a missing area"));
        }
        if device
            .via_device_id
            .as_deref()
            .is_some_and(|id| !all_device_ids.contains(id) || id == device.id)
        {
            return Err(invalid(
                "removed device references a missing or self parent device",
            ));
        }
        if device
            .labels
            .iter()
            .any(|id| !label_ids.contains(id.as_str()))
        {
            return Err(invalid("removed device references a missing label"));
        }
    }
    for device in &data.devices {
        let mut path = HashSet::new();
        let mut current = Some(device.id.as_str());
        while let Some(device_id) = current {
            if !path.insert(device_id) {
                return Err(invalid("device parent references cannot contain a cycle"));
            }
            current = data
                .devices
                .iter()
                .find(|candidate| candidate.id == device_id)
                .and_then(|candidate| candidate.via_device_id.as_deref());
        }
    }
    let mut area_names = HashSet::new();
    for area in &data.areas {
        if area.id.is_empty() {
            return Err(invalid("area ID cannot be empty"));
        }
        if !area_names.insert(normalized_name(&area.name)) {
            return Err(invalid("duplicate normalized area name"));
        }
        if area
            .floor_id
            .as_deref()
            .is_some_and(|id| !floor_ids.contains(id))
        {
            return Err(invalid(format!(
                "area {} references a missing floor",
                area.id
            )));
        }
        if area
            .labels
            .iter()
            .any(|id| !label_ids.contains(id.as_str()))
        {
            return Err(invalid(format!(
                "area {} references a missing label",
                area.id
            )));
        }
        for (entity_id, expected_class) in [
            (area.humidity_entity_id.as_deref(), "humidity"),
            (area.temperature_entity_id.as_deref(), "temperature"),
        ] {
            let Some(entity_id) = entity_id else { continue };
            let entry = data
                .entities
                .iter()
                .find(|entry| entry.entity_id == entity_id)
                .ok_or_else(|| invalid(format!("area {} references a missing entity", area.id)))?;
            let actual_class = entry
                .device_class
                .as_deref()
                .or(entry.original_device_class.as_deref());
            if entry.domain != "sensor" || actual_class != Some(expected_class) {
                return Err(invalid(format!(
                    "area {} {} entity must be a {expected_class} sensor",
                    area.id, expected_class
                )));
            }
        }
    }
    let mut floor_names = HashSet::new();
    for floor in &data.floors {
        if floor.floor_id.is_empty() {
            return Err(invalid("floor ID cannot be empty"));
        }
        if !floor_names.insert(normalized_name(&floor.name)) {
            return Err(invalid("duplicate normalized floor name"));
        }
    }
    let mut label_names = HashSet::new();
    for label in &data.labels {
        if label.label_id.is_empty() {
            return Err(invalid("label ID cannot be empty"));
        }
        if !label_names.insert(normalized_name(&label.name)) {
            return Err(invalid("duplicate normalized label name"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
