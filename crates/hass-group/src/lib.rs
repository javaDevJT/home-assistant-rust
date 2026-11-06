//! Native implementation of Home Assistant's legacy `group` integration.
//!
//! This crate handles only `group:` YAML entities and the legacy `group.set`
//! and `group.remove` services. Platform groups such as `light: - platform:
//! group` are separate integrations and are intentionally rejected by the
//! strict configuration type until their providers are ported.

use hass_core::{Context, Error, Event, Runtime, ServiceCall, ServiceResult, State};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{broadcast, oneshot},
    task::JoinHandle,
};

const DOMAIN: &str = "group";
const SERVICE_SET: &str = "set";
const SERVICE_REMOVE: &str = "remove";
const OWNED_IDS_DATA_KEY: &str = "hass_group.owned_ids.v1";
const MAX_GROUPS: usize = 4_096;
const MAX_MEMBERS_PER_GROUP: usize = 4_096;
const MAX_TOTAL_MEMBERS: usize = 65_536;
const MAX_EXPANDED_ENTITIES: usize = 65_536;
const MAX_GROUP_DEPTH: usize = 64;
const MAX_CONFIG_STRING_CHARS: usize = 255;

/// A legacy group definition.
///
/// A definition accepts the strict object form with `name`, `entities`,
/// `icon`, and `all` fields. For compatibility with the pinned YAML
/// integration it also accepts a member string or list as shorthand for
/// `entities`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GroupConfig {
    /// Display name. When omitted, the YAML object ID is used.
    pub name: Option<String>,
    /// Ordered member entity IDs.
    pub entities: Vec<String>,
    /// Optional Material Design Icons name.
    pub icon: Option<String>,
    /// Require every currently available member to be on.
    pub all: bool,
}

impl GroupConfig {
    /// Create a default `any` group from ordered member IDs.
    pub fn new<I, S>(entities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            name: None,
            entities: entities.into_iter().map(Into::into).collect(),
            icon: None,
            all: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupConfigObject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_entities")]
    entities: Vec<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EntitiesInput {
    One(String),
    Many(Vec<String>),
    Null(()),
}

fn deserialize_entities<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match EntitiesInput::deserialize(deserializer)? {
        EntitiesInput::One(entity_id) => vec![entity_id],
        EntitiesInput::Many(entity_ids) => entity_ids,
        EntitiesInput::Null(()) => Vec::new(),
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupConfigInput {
    Null(()),
    One(String),
    Many(Vec<String>),
    Object(GroupConfigObject),
}

impl<'de> Deserialize<'de> for GroupConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match GroupConfigInput::deserialize(deserializer)? {
            GroupConfigInput::Null(()) => Self::default(),
            GroupConfigInput::One(entity_id) => Self::new([entity_id]),
            GroupConfigInput::Many(entity_ids) => Self::new(entity_ids),
            GroupConfigInput::Object(object) => Self {
                name: object.name,
                entities: object.entities,
                icon: object.icon,
                all: object.all,
            },
        })
    }
}

/// Validate group definitions without reading or mutating runtime state.
///
/// Entity IDs are case-folded during validation just as Home Assistant does
/// when it begins tracking members. Static references to configured groups
/// are checked as a directed graph, so cycles fail before any state is
/// written.
pub fn validate(config: &BTreeMap<String, GroupConfig>) -> hass_core::Result<()> {
    let normalized = normalize_config_map(config)?;
    let _ = topological_order(&normalized)?;
    Ok(())
}

fn normalize_object_id(value: &str) -> hass_core::Result<String> {
    let object_id = value.to_ascii_lowercase();
    if object_id.is_empty()
        || object_id.chars().count() > MAX_CONFIG_STRING_CHARS
        || !object_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid group object ID {value:?}: expected 1 to {MAX_CONFIG_STRING_CHARS} lowercase ASCII letters, digits, or underscores"
        )));
    }
    Ok(object_id)
}

fn normalize_entity_id(value: &str) -> hass_core::Result<String> {
    let entity_id = value.to_ascii_lowercase();
    let Some((domain, object_id)) = entity_id.split_once('.') else {
        return Err(Error::InvalidInput(format!(
            "invalid group member entity ID {value:?}: expected domain.object_id"
        )));
    };
    if object_id.contains('.') {
        return Err(Error::InvalidInput(format!(
            "invalid group member entity ID {value:?}: expected exactly one dot"
        )));
    }
    for (component, label) in [(domain, "domain"), (object_id, "object ID")] {
        if component.is_empty()
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(Error::InvalidInput(format!(
                "invalid group member entity ID {value:?}: {label} must contain lowercase ASCII letters, digits, or underscores"
            )));
        }
    }
    if entity_id.chars().count() > 511 {
        return Err(Error::InvalidInput(format!(
            "group member entity ID {value:?} is too long"
        )));
    }
    Ok(entity_id)
}

fn validate_label(value: &str, label: &str, allow_empty: bool) -> hass_core::Result<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.chars().count() > MAX_CONFIG_STRING_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(format!(
            "invalid group {label}: expected a{} string of at most {MAX_CONFIG_STRING_CHARS} characters without control characters",
            if allow_empty { "" } else { " non-empty" }
        )));
    }
    Ok(())
}

fn normalize_config_map(
    config: &BTreeMap<String, GroupConfig>,
) -> hass_core::Result<BTreeMap<String, GroupConfig>> {
    if config.len() > MAX_GROUPS {
        return Err(Error::InvalidInput(format!(
            "a group integration may contain at most {MAX_GROUPS} groups"
        )));
    }
    let mut normalized = BTreeMap::new();
    let mut total_members = 0usize;
    for (object_id, group) in config {
        let object_id = normalize_object_id(object_id)?;
        if group.entities.len() > MAX_MEMBERS_PER_GROUP {
            return Err(Error::InvalidInput(format!(
                "group {object_id:?} has more than {MAX_MEMBERS_PER_GROUP} members"
            )));
        }
        total_members = total_members.saturating_add(group.entities.len());
        if total_members > MAX_TOTAL_MEMBERS {
            return Err(Error::InvalidInput(format!(
                "group configuration has more than {MAX_TOTAL_MEMBERS} total members"
            )));
        }
        if let Some(name) = &group.name {
            validate_label(name, "name", false)?;
        }
        if let Some(icon) = &group.icon {
            validate_label(icon, "icon", false)?;
        }
        let mut normalized_group = group.clone();
        normalized_group.entities = group
            .entities
            .iter()
            .map(|entity_id| normalize_entity_id(entity_id))
            .collect::<hass_core::Result<Vec<_>>>()?;
        if normalized
            .insert(object_id.clone(), normalized_group)
            .is_some()
        {
            return Err(Error::InvalidInput(format!(
                "duplicate group object ID after normalization: {object_id:?}"
            )));
        }
    }
    Ok(normalized)
}

fn normalize_group_order(
    config: &BTreeMap<String, GroupConfig>,
    order: &[String],
) -> hass_core::Result<Vec<String>> {
    if order.len() != config.len() {
        return Err(Error::InvalidInput(
            "group order must list every configured object ID exactly once".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(order.len());
    for object_id in order {
        let object_id = normalize_object_id(object_id)?;
        if !config.contains_key(&object_id) || !seen.insert(object_id.clone()) {
            return Err(Error::InvalidInput(format!(
                "group order contains unknown or duplicate object ID {object_id:?}"
            )));
        }
        normalized.push(object_id);
    }
    Ok(normalized)
}

fn topological_order(groups: &BTreeMap<String, GroupConfig>) -> hass_core::Result<Vec<String>> {
    fn visit(
        object_id: &str,
        groups: &BTreeMap<String, GroupConfig>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        output: &mut Vec<String>,
        depth: usize,
    ) -> hass_core::Result<()> {
        if visited.contains(object_id) {
            return Ok(());
        }
        if depth > MAX_GROUP_DEPTH {
            return Err(Error::InvalidInput(format!(
                "group nesting exceeds {MAX_GROUP_DEPTH} levels at {object_id:?}"
            )));
        }
        if !visiting.insert(object_id.to_owned()) {
            return Err(Error::InvalidInput(format!(
                "group membership cycle includes {object_id:?}"
            )));
        }
        if let Some(group) = groups.get(object_id) {
            for entity_id in &group.entities {
                if let Some(child_id) = entity_id.strip_prefix("group.")
                    && groups.contains_key(child_id)
                {
                    visit(child_id, groups, visiting, visited, output, depth + 1)?;
                }
            }
        }
        visiting.remove(object_id);
        visited.insert(object_id.to_owned());
        output.push(object_id.to_owned());
        Ok(())
    }

    let mut output = Vec::with_capacity(groups.len());
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for object_id in groups.keys() {
        visit(
            object_id,
            groups,
            &mut visiting,
            &mut visited,
            &mut output,
            1,
        )?;
    }
    Ok(output)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedGroupIds {
    #[serde(default)]
    entity_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GroupRecord {
    config: GroupConfig,
    order: u64,
    created_by_service: bool,
}

impl GroupRecord {
    fn entity_id(object_id: &str) -> String {
        format!("{DOMAIN}.{object_id}")
    }

    fn display_name<'a>(&'a self, object_id: &'a str) -> &'a str {
        self.config.name.as_deref().unwrap_or(object_id)
    }
}

struct GroupIndex {
    groups: RwLock<BTreeMap<String, GroupRecord>>,
    active: AtomicBool,
}

impl GroupIndex {
    fn new(groups: BTreeMap<String, GroupRecord>) -> Self {
        Self {
            groups: RwLock::new(groups),
            active: AtomicBool::new(false),
        }
    }

    fn lock_groups(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, GroupRecord>> {
        self.groups
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_groups(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, GroupRecord>> {
        self.groups
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Weak view of active native group membership for generic service routing.
///
/// It holds no runtime or persistent store. A lookup stops resolving native
/// members as soon as its owning [`GroupIntegration`] is shut down or dropped.
#[derive(Clone, Default)]
pub struct GroupLookup {
    index: Weak<GroupIndex>,
}

impl GroupLookup {
    /// Return the configured direct members for an active native group.
    pub fn members(&self, entity_id: &str) -> Option<Vec<String>> {
        let index = self.index.upgrade()?;
        if !index.active.load(Ordering::Acquire) {
            return None;
        }
        let entity_id = entity_id.to_ascii_lowercase();
        let object_id = entity_id.strip_prefix("group.")?;
        let members = index
            .lock_groups()
            .get(object_id)
            .map(|record| record.config.entities.clone());
        if index.active.load(Ordering::Acquire) {
            members
        } else {
            None
        }
    }

    /// Expand nested active native groups, retaining unknown/non-group IDs.
    ///
    /// This method intentionally does not inspect runtime state attributes.
    /// Call the crate-level [`expand_entity_ids`] helper when Home Assistant's
    /// legacy state-attribute fallback is also required.
    pub fn expand_entity_ids(&self, ids: &[String]) -> hass_core::Result<Vec<String>> {
        expand_ids(ids, |entity_id| self.members(entity_id), None)
    }
}

/// Expand entity IDs using active native groups first and Home Assistant's
/// legacy `group.*` state `entity_id` attribute fallback second.
///
/// Unknown non-group IDs are retained. Home Assistant's `all`/`none` target
/// sentinels are omitted, IDs are lowercased, and output preserves the first
/// encounter order while removing duplicates. Cycles, excessive depth, and
/// excessive expansion fail with `InvalidInput`.
pub fn expand_entity_ids(
    runtime: &Runtime,
    lookup: &GroupLookup,
    ids: &[String],
) -> hass_core::Result<Vec<String>> {
    expand_ids(ids, |entity_id| lookup.members(entity_id), Some(runtime))
}

fn expand_ids<F>(
    ids: &[String],
    mut native_members: F,
    runtime: Option<&Runtime>,
) -> hass_core::Result<Vec<String>>
where
    F: FnMut(&str) -> Option<Vec<String>>,
{
    struct Expansion<'a, F> {
        native_members: &'a mut F,
        runtime: Option<&'a Runtime>,
        output: Vec<String>,
        output_seen: HashSet<String>,
        active_groups: HashSet<String>,
        expanded_groups: HashSet<String>,
        visited: usize,
    }

    impl<F> Expansion<'_, F>
    where
        F: FnMut(&str) -> Option<Vec<String>>,
    {
        fn visit(&mut self, value: &str, depth: usize) -> hass_core::Result<()> {
            self.visited = self.visited.saturating_add(1);
            if self.visited > MAX_EXPANDED_ENTITIES.saturating_mul(4) {
                return Err(Error::InvalidInput(
                    "group expansion exceeded its work limit".into(),
                ));
            }
            if depth > MAX_GROUP_DEPTH {
                return Err(Error::InvalidInput(format!(
                    "group expansion exceeds {MAX_GROUP_DEPTH} nested levels"
                )));
            }
            let entity_id = value.to_ascii_lowercase();
            if entity_id == "all" || entity_id == "none" {
                return Ok(());
            }
            if entity_id.starts_with("group.") {
                if self.active_groups.contains(&entity_id) {
                    return Err(Error::InvalidInput(format!(
                        "group expansion cycle includes {entity_id:?}"
                    )));
                }
                if self.expanded_groups.contains(&entity_id) {
                    return Ok(());
                }
                let members = if let Some(members) = (self.native_members)(&entity_id) {
                    Some(members)
                } else {
                    self.runtime
                        .and_then(|runtime| runtime.get_state(&entity_id))
                        .and_then(|state| {
                            state
                                .attributes
                                .get("entity_id")
                                .and_then(Value::as_array)
                                .map(|members| {
                                    members
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                        })
                };
                let Some(members) = members else {
                    // The pinned helper treats a missing or non-group group
                    // state as an empty membership list.
                    return Ok(());
                };
                if members.len() > MAX_MEMBERS_PER_GROUP {
                    return Err(Error::InvalidInput(format!(
                        "group {entity_id:?} has more than {MAX_MEMBERS_PER_GROUP} members"
                    )));
                }
                self.active_groups.insert(entity_id.clone());
                for member in members {
                    self.visit(&member, depth + 1)?;
                }
                self.active_groups.remove(&entity_id);
                self.expanded_groups.insert(entity_id);
                return Ok(());
            }
            if self.output_seen.insert(entity_id.clone()) {
                if self.output.len() >= MAX_EXPANDED_ENTITIES {
                    return Err(Error::InvalidInput(format!(
                        "group expansion exceeds {MAX_EXPANDED_ENTITIES} output entities"
                    )));
                }
                self.output.push(entity_id);
            }
            Ok(())
        }
    }

    if ids.len() > MAX_EXPANDED_ENTITIES {
        return Err(Error::InvalidInput(format!(
            "group expansion accepts at most {MAX_EXPANDED_ENTITIES} input entities"
        )));
    }
    let mut expansion = Expansion {
        native_members: &mut native_members,
        runtime,
        output: Vec::new(),
        output_seen: HashSet::new(),
        active_groups: HashSet::new(),
        expanded_groups: HashSet::new(),
        visited: 0,
    };
    for entity_id in ids {
        expansion.visit(entity_id, 0)?;
    }
    Ok(expansion.output)
}

struct IntegrationCore {
    runtime: Runtime,
    index: Arc<GroupIndex>,
    owner: String,
    static_count: usize,
    next_dynamic_order: Mutex<u64>,
    lifecycle: Mutex<()>,
}

impl IntegrationCore {
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn snapshot_groups(&self) -> BTreeMap<String, GroupRecord> {
        self.index.lock_groups().clone()
    }
}

/// The running native legacy group integration.
pub struct GroupIntegration {
    core: Arc<IntegrationCore>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    services_registered: bool,
}

impl GroupIntegration {
    /// Restore dynamic groups, publish configured group states, register
    /// `group.set`/`group.remove`, and start the single bounded event listener.
    pub async fn start(
        runtime: Runtime,
        config: BTreeMap<String, GroupConfig>,
    ) -> hass_core::Result<Self> {
        let order = config.keys().cloned().collect();
        Self::start_with_order(runtime, config, order).await
    }

    /// Start the integration with the original YAML mapping order.
    ///
    /// The order must contain each configured object ID exactly once. It is
    /// used for the observable group `order` attribute; dynamically created
    /// groups follow the static entries.
    pub async fn start_with_order(
        runtime: Runtime,
        config: BTreeMap<String, GroupConfig>,
        order: Vec<String>,
    ) -> hass_core::Result<Self> {
        validate(&config)?;
        let owned_before = runtime.get_data(OWNED_IDS_DATA_KEY)?;
        let owned: OwnedGroupIds = match owned_before.as_ref() {
            Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                Error::Storage(format!("invalid native group ownership metadata: {error}"))
            })?,
            None => OwnedGroupIds::default(),
        };
        if owned.entity_ids.len() > MAX_GROUPS {
            return Err(Error::Storage(format!(
                "native group ownership metadata exceeds {MAX_GROUPS} IDs"
            )));
        }
        let mut owned_object_ids = BTreeSet::new();
        for entity_id in &owned.entity_ids {
            let normalized = normalize_entity_id(entity_id).map_err(|error| {
                Error::Storage(format!("invalid owned native group ID: {error}"))
            })?;
            if normalized != *entity_id {
                return Err(Error::Storage(format!(
                    "owned native group ID is not canonical: {entity_id:?}"
                )));
            }
            let Some(object_id) = entity_id.strip_prefix("group.") else {
                return Err(Error::Storage(format!(
                    "owned entity {entity_id:?} is not a group"
                )));
            };
            owned_object_ids.insert(object_id.to_owned());
        }
        let yaml = normalize_config_map(&config)?;
        let static_order = normalize_group_order(&yaml, &order)?;
        let yaml_ids: BTreeSet<_> = yaml.keys().cloned().collect();

        let mut records = BTreeMap::new();
        for (position, object_id) in static_order.iter().enumerate() {
            records.insert(
                object_id.clone(),
                GroupRecord {
                    config: yaml[object_id].clone(),
                    order: position as u64,
                    created_by_service: false,
                },
            );
        }

        let index = Arc::new(GroupIndex::new(records));
        let owner = format!("hass-group/{}", uuid::Uuid::new_v4());
        let core = Arc::new(IntegrationCore {
            runtime: runtime.clone(),
            index: Arc::clone(&index),
            owner,
            static_count: yaml.len(),
            next_dynamic_order: Mutex::new(0),
            lifecycle: Mutex::new(()),
        });
        let (_snapshot, receiver) = runtime.subscribe_with_states();
        register_services(&runtime, &core)?;

        // Native Runtime state rows survive a process restart. Upstream
        // service-created groups do not, so remove only the group IDs this
        // integration previously created and left in that restored state map.
        // A YAML group with the same ID is retained and rewritten below.
        for object_id in owned_object_ids.difference(&yaml_ids) {
            if let Err(error) =
                runtime.remove_state(&GroupRecord::entity_id(object_id), Context::default())
            {
                let _ = runtime.unregister_owned_services(&core.owner);
                return Err(error);
            }
        }

        let initial_records = core.snapshot_groups();
        let current_owned = core.owned_group_metadata(&initial_records)?;
        let mut states_before = BTreeMap::new();
        for object_id in initial_records.keys() {
            let entity_id = GroupRecord::entity_id(object_id);
            states_before.insert(entity_id.clone(), runtime.get_state(&entity_id));
        }
        let initial_result = recompute_all(&runtime, &initial_records, &Context::default());
        if let Err(error) = initial_result {
            rollback_states(&runtime, states_before, &Context::default());
            let _ = runtime.unregister_owned_services(&core.owner);
            return Err(error);
        }
        let ownership_changed = owned_before
            .as_ref()
            .is_some_and(|previous| previous != &current_owned)
            || (owned_before.is_none() && metadata_has_group_ids(&current_owned));
        if ownership_changed
            && let Err(error) = write_owned_group_metadata(&runtime, &current_owned)
        {
            rollback_states(&runtime, states_before, &Context::default());
            let _ = runtime.unregister_owned_services(&core.owner);
            return Err(error);
        }
        index.active.store(true, Ordering::Release);
        let (stop, stop_rx) = oneshot::channel();
        let weak_core = Arc::downgrade(&core);
        let task = tokio::spawn(async move {
            state_event_loop(weak_core, receiver, stop_rx).await;
        });

        Ok(Self {
            core,
            stop: Some(stop),
            task: Some(task),
            services_registered: true,
        })
    }

    /// Return a weak membership view for generic service routing.
    pub fn lookup(&self) -> GroupLookup {
        GroupLookup {
            index: Arc::downgrade(&self.core.index),
        }
    }

    /// Stop event processing, join it, and unregister owned group services.
    pub async fn shutdown(mut self) -> hass_core::Result<()> {
        self.deactivate();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let mut first_error = None;
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            first_error = Some(Error::Service(format!(
                "failed to join native group state listener: {error}"
            )));
        }
        if self.services_registered {
            self.services_registered = false;
            if let Err(error) = self
                .core
                .runtime
                .unregister_owned_services(&self.core.owner)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn deactivate(&mut self) {
        let _lifecycle = self.core.lock_lifecycle();
        self.core.index.active.store(false, Ordering::Release);
    }
}

impl Drop for GroupIntegration {
    fn drop(&mut self) {
        self.deactivate();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.services_registered {
            self.services_registered = false;
            if let Err(error) = self
                .core
                .runtime
                .unregister_owned_services(&self.core.owner)
            {
                tracing::warn!(%error, "failed to unregister native group services during drop");
            }
        }
    }
}

fn register_services(runtime: &Runtime, core: &Arc<IntegrationCore>) -> hass_core::Result<()> {
    let set_handler = service_handler(Arc::downgrade(core), SERVICE_SET);
    runtime.register_owned_service(
        &core.owner,
        DOMAIN,
        SERVICE_SET,
        json!({"name": "Set group members"}),
        set_handler,
    )?;
    let remove_handler = service_handler(Arc::downgrade(core), SERVICE_REMOVE);
    if let Err(error) = runtime.register_owned_service(
        &core.owner,
        DOMAIN,
        SERVICE_REMOVE,
        json!({"name": "Remove a group"}),
        remove_handler,
    ) {
        let _ = runtime.unregister_owned_services(&core.owner);
        return Err(error);
    }
    Ok(())
}

fn service_handler(
    core: Weak<IntegrationCore>,
    service: &'static str,
) -> hass_core::ServiceHandler {
    Arc::new(move |_runtime, call| {
        let core = core
            .upgrade()
            .ok_or_else(|| Error::Service("native group integration is no longer active".into()))?;
        if !core.index.active.load(Ordering::Acquire) {
            return Err(Error::Service(
                "native group integration is not active".into(),
            ));
        }
        core.handle_service(call, service)
    })
}

impl IntegrationCore {
    fn handle_service(
        &self,
        call: &ServiceCall,
        service: &str,
    ) -> hass_core::Result<ServiceResult> {
        let _lifecycle = self.lock_lifecycle();
        if !self.index.active.load(Ordering::Acquire) {
            return Err(Error::Service(
                "native group integration is not active".into(),
            ));
        }
        let object_id = service_object_id(call)?;
        match service {
            SERVICE_SET => self.set_group(call, &object_id),
            SERVICE_REMOVE => self.remove_group(call, &object_id),
            _ => Err(Error::Service(format!(
                "unsupported group service {service:?}"
            ))),
        }
    }

    fn set_group(&self, call: &ServiceCall, object_id: &str) -> hass_core::Result<ServiceResult> {
        check_service_fields(call, SERVICE_SET)?;
        let old_groups = self.snapshot_groups();
        if let Some(existing) = old_groups.get(object_id) {
            let mut updated = existing.clone();
            let mut entities = updated.config.entities.clone();
            if let Some(add) = call.data.get("add_entities") {
                for member in normalize_member_list(&parse_entity_list(add)?)? {
                    if !entities.contains(&member) {
                        entities.push(member);
                    }
                }
            }
            if let Some(remove) = call.data.get("remove_entities") {
                let remove: HashSet<_> = normalize_member_list(&parse_entity_list(remove)?)?
                    .into_iter()
                    .collect();
                entities.retain(|member| !remove.contains(member));
            }
            if let Some(replacement) = call.data.get("entities") {
                entities = normalize_member_list(&parse_entity_list(replacement)?)?;
            }
            if let Some(name) = optional_string(&call.data, "name")? {
                updated.config.name = Some(name);
            }
            if call.data.contains_key("icon") {
                updated.config.icon = optional_string_field(&call.data, "icon")?;
            }
            if let Some(all) = optional_bool(&call.data, "all")? {
                updated.config.all = all;
            }
            updated.config.entities = entities;
            if updated == *existing {
                return Ok(service_result(Vec::new(), &call.context));
            }
            let mut candidate = old_groups.clone();
            candidate.insert(object_id.to_owned(), updated);
            self.commit_group_update(call, old_groups, candidate, object_id)
        } else {
            let mut config = GroupConfig {
                name: Some(
                    optional_string(&call.data, "name")?.unwrap_or_else(|| object_id.to_owned()),
                ),
                icon: optional_string_field(&call.data, "icon")?,
                all: optional_bool(&call.data, "all")?.unwrap_or(false),
                ..GroupConfig::default()
            };
            let initial_entities = call
                .data
                .get("entities")
                .map(parse_entity_list)
                .transpose()?
                .filter(|ids| !ids.is_empty())
                .or(call
                    .data
                    .get("add_entities")
                    .map(parse_entity_list)
                    .transpose()?)
                .unwrap_or_default();
            config.entities = normalize_member_list(&initial_entities)?;
            let next_dynamic_order = *self
                .next_dynamic_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if old_groups.len() >= MAX_GROUPS {
                return Err(Error::InvalidInput(format!(
                    "a group integration may contain at most {MAX_GROUPS} groups"
                )));
            }
            let mut candidate = old_groups.clone();
            candidate.insert(
                object_id.to_owned(),
                GroupRecord {
                    config,
                    order: self.static_count as u64 + next_dynamic_order,
                    created_by_service: true,
                },
            );
            let result = self.commit_group_update(call, old_groups, candidate, object_id);
            if result.is_ok() {
                let mut next_dynamic_order = self
                    .next_dynamic_order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *next_dynamic_order = next_dynamic_order.saturating_add(1);
            }
            result
        }
    }

    fn remove_group(
        &self,
        call: &ServiceCall,
        object_id: &str,
    ) -> hass_core::Result<ServiceResult> {
        check_service_fields(call, SERVICE_REMOVE)?;
        let old_groups = self.snapshot_groups();
        if !old_groups.contains_key(object_id) {
            return Ok(service_result(Vec::new(), &call.context));
        }
        let mut candidate = old_groups.clone();
        candidate.remove(object_id);
        validate_records(&candidate)?;
        let old_metadata = self.owned_group_metadata(&old_groups)?;
        let next_metadata = self.owned_group_metadata(&candidate)?;
        if old_metadata != next_metadata {
            write_owned_group_metadata(&self.runtime, &next_metadata)?;
        }

        let mut visible_groups = self.index.write_groups();
        let entity_id = GroupRecord::entity_id(object_id);
        let mut previous_states: BTreeMap<_, _> = candidate
            .keys()
            .map(|candidate_id| {
                let candidate_entity_id = GroupRecord::entity_id(candidate_id);
                (
                    candidate_entity_id.clone(),
                    self.runtime.get_state(&candidate_entity_id),
                )
            })
            .collect();
        previous_states.insert(entity_id.clone(), self.runtime.get_state(&entity_id));
        if let Err(error) = self.runtime.remove_state(&entity_id, call.context.clone()) {
            restore_metadata(&self.runtime, OWNED_IDS_DATA_KEY, &old_metadata);
            return Err(error);
        }
        let affected: BTreeSet<_> = candidate.keys().cloned().collect();
        let changed_states =
            match recompute_selected(&self.runtime, &candidate, &affected, &call.context) {
                Ok(states) => states,
                Err(error) => {
                    rollback_states(&self.runtime, previous_states, &call.context);
                    restore_metadata(&self.runtime, OWNED_IDS_DATA_KEY, &old_metadata);
                    return Err(error);
                }
            };
        *visible_groups = candidate;
        Ok(service_result(changed_states, &call.context))
    }

    fn commit_group_update(
        &self,
        call: &ServiceCall,
        old_groups: BTreeMap<String, GroupRecord>,
        candidate: BTreeMap<String, GroupRecord>,
        updated_object_id: &str,
    ) -> hass_core::Result<ServiceResult> {
        validate_records(&candidate)?;
        let old_metadata = self.owned_group_metadata(&old_groups)?;
        let next_metadata = self.owned_group_metadata(&candidate)?;
        if old_metadata != next_metadata {
            write_owned_group_metadata(&self.runtime, &next_metadata)?;
        }

        let mut visible_groups = self.index.write_groups();
        let trigger_entity = GroupRecord::entity_id(updated_object_id);
        let affected = affected_group_ids(&candidate, &trigger_entity, true)?;
        let previous_states: BTreeMap<_, _> = affected
            .iter()
            .map(|object_id| {
                let entity_id = GroupRecord::entity_id(object_id);
                (entity_id.clone(), self.runtime.get_state(&entity_id))
            })
            .collect();
        let changed_states =
            match recompute_selected(&self.runtime, &candidate, &affected, &call.context) {
                Ok(states) => states,
                Err(error) => {
                    rollback_states(&self.runtime, previous_states, &call.context);
                    restore_metadata(&self.runtime, OWNED_IDS_DATA_KEY, &old_metadata);
                    return Err(error);
                }
            };
        *visible_groups = candidate;
        Ok(service_result(changed_states, &call.context))
    }

    fn owned_group_metadata(
        &self,
        groups: &BTreeMap<String, GroupRecord>,
    ) -> hass_core::Result<Value> {
        let mut owned = OwnedGroupIds::default();
        for object_id in groups.keys() {
            owned.entity_ids.insert(GroupRecord::entity_id(object_id));
        }
        serde_json::to_value(owned).map_err(|error| {
            Error::Storage(format!(
                "cannot encode native group ownership metadata: {error}"
            ))
        })
    }
}

fn service_result(changed_states: Vec<State>, context: &Context) -> ServiceResult {
    ServiceResult {
        changed_states,
        response: None,
        context: context.clone(),
    }
}

fn service_object_id(call: &ServiceCall) -> hass_core::Result<String> {
    if let Some(value) = call.data.get("object_id") {
        let object_id = value.as_str().ok_or_else(|| {
            Error::InvalidInput("group service object_id must be a string".into())
        })?;
        return normalize_object_id(object_id);
    }
    Err(Error::InvalidInput(
        "group service requires object_id".into(),
    ))
}

fn check_service_fields(call: &ServiceCall, service: &str) -> hass_core::Result<()> {
    if let Some(key) = call.target.keys().next() {
        return Err(Error::InvalidInput(format!(
            "unsupported group service target field {key:?}"
        )));
    }
    let allowed: &[&str] = if service == SERVICE_SET {
        &[
            "object_id",
            "entities",
            "add_entities",
            "remove_entities",
            "name",
            "icon",
            "all",
        ]
    } else {
        &["object_id"]
    };
    if let Some(key) = call
        .data
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        return Err(Error::InvalidInput(format!(
            "unsupported group.{service} field {key:?}"
        )));
    }
    if service == SERVICE_SET {
        let membership_fields = ["entities", "add_entities", "remove_entities"]
            .iter()
            .filter(|field| call.data.contains_key(**field))
            .count();
        if membership_fields > 1 {
            return Err(Error::InvalidInput(
                "group.set accepts only one of entities, add_entities, or remove_entities".into(),
            ));
        }
    }
    Ok(())
}

fn parse_entity_list(value: &Value) -> hass_core::Result<Vec<String>> {
    let entity_ids = match value {
        Value::Null => Vec::new(),
        Value::String(entity_id) => vec![entity_id.clone()],
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::InvalidInput("group member lists must contain only strings".into())
                })
            })
            .collect::<hass_core::Result<Vec<_>>>()?,
        _ => {
            return Err(Error::InvalidInput(
                "group member list must be a string or array of strings".into(),
            ));
        }
    };
    if entity_ids.len() > MAX_MEMBERS_PER_GROUP {
        return Err(Error::InvalidInput(format!(
            "a group may contain at most {MAX_MEMBERS_PER_GROUP} members"
        )));
    }
    Ok(entity_ids)
}

fn normalize_member_list(values: &[String]) -> hass_core::Result<Vec<String>> {
    values
        .iter()
        .map(|value| normalize_entity_id(value))
        .collect()
}

fn optional_string(data: &Map<String, Value>, key: &str) -> hass_core::Result<Option<String>> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| Error::InvalidInput(format!("group service {key} must be a string")))?;
    validate_label(value, key, false)?;
    Ok(Some(value.to_owned()))
}

fn optional_string_field(
    data: &Map<String, Value>,
    key: &str,
) -> hass_core::Result<Option<String>> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_str().ok_or_else(|| {
        Error::InvalidInput(format!("group service {key} must be a string or null"))
    })?;
    validate_label(value, key, false)?;
    Ok(Some(value.to_owned()))
}

fn optional_bool(data: &Map<String, Value>, key: &str) -> hass_core::Result<Option<bool>> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| Error::InvalidInput(format!("group service {key} must be a boolean")))
}

fn validate_records(groups: &BTreeMap<String, GroupRecord>) -> hass_core::Result<()> {
    let config = groups
        .iter()
        .map(|(id, record)| (id.clone(), record.config.clone()))
        .collect();
    validate(&config)
}

fn write_owned_group_metadata(runtime: &Runtime, value: &Value) -> hass_core::Result<()> {
    let entity_ids = value
        .get("entity_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Storage("invalid native group ownership metadata".into()))?;
    if entity_ids.is_empty() {
        runtime.delete_data(OWNED_IDS_DATA_KEY).map(|_| ())
    } else {
        runtime.set_data(OWNED_IDS_DATA_KEY, value)
    }
}

fn metadata_has_group_ids(value: &Value) -> bool {
    value
        .get("entity_ids")
        .and_then(Value::as_array)
        .is_some_and(|entity_ids| !entity_ids.is_empty())
}

fn restore_metadata(runtime: &Runtime, key: &str, old: &Value) {
    let restored = write_owned_group_metadata(runtime, old);
    if let Err(error) = restored {
        tracing::error!(%error, %key, "failed to roll back persisted native group metadata");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StateType {
    on: &'static str,
    off: &'static str,
}

#[derive(Clone, Copy)]
struct DomainStates {
    domain: &'static str,
    active: &'static [&'static str],
    state_type: StateType,
}

const DEFAULT_STATE_TYPE: StateType = StateType {
    on: "on",
    off: "off",
};

// This is the immutable legacy registry table from the pinned group
// integration. New platform group variants must not be inferred from these
// generic state names; each variant needs an actual provider port.
const DOMAIN_STATES: &[DomainStates] = &[
    DomainStates {
        domain: "alarm_control_panel",
        active: &[
            "on",
            "armed_away",
            "armed_custom_bypass",
            "armed_home",
            "armed_night",
            "armed_vacation",
            "triggered",
        ],
        state_type: DEFAULT_STATE_TYPE,
    },
    DomainStates {
        domain: "climate",
        active: &["on", "heat", "cool", "heat_cool", "auto", "fan_only"],
        state_type: DEFAULT_STATE_TYPE,
    },
    DomainStates {
        domain: "cover",
        active: &["open"],
        state_type: StateType {
            on: "open",
            off: "closed",
        },
    },
    DomainStates {
        domain: "device_tracker",
        active: &["home"],
        state_type: StateType {
            on: "home",
            off: "not_home",
        },
    },
    DomainStates {
        domain: "lock",
        active: &["locking", "open", "opening", "unlocked", "unlocking"],
        state_type: StateType {
            on: "unlocked",
            off: "locked",
        },
    },
    DomainStates {
        domain: "media_player",
        active: &["on", "paused", "playing", "idle"],
        state_type: DEFAULT_STATE_TYPE,
    },
    DomainStates {
        domain: "person",
        active: &["home"],
        state_type: StateType {
            on: "home",
            off: "not_home",
        },
    },
    DomainStates {
        domain: "plant",
        active: &["problem"],
        state_type: StateType {
            on: "problem",
            off: "ok",
        },
    },
    DomainStates {
        domain: "vacuum",
        active: &["on", "cleaning", "returning", "error"],
        state_type: DEFAULT_STATE_TYPE,
    },
    DomainStates {
        domain: "water_heater",
        active: &[
            "on",
            "eco",
            "electric",
            "performance",
            "high_demand",
            "heat_pump",
            "gas",
        ],
        state_type: DEFAULT_STATE_TYPE,
    },
];

const EXCLUDED_DOMAINS: &[&str] = &["air_quality", "sensor", "weather"];

fn domain_states(domain: &str) -> Option<&'static DomainStates> {
    DOMAIN_STATES.iter().find(|entry| entry.domain == domain)
}

fn global_on_state(state: &str) -> Option<&'static str> {
    if state == "on" {
        return Some("on");
    }
    DOMAIN_STATES
        .iter()
        .flat_map(|entry| entry.active.iter().copied())
        .find(|candidate| *candidate == state)
}

fn global_active_for_off(state: &str) -> Option<&'static str> {
    if state == "off" {
        return Some("on");
    }
    for entry in DOMAIN_STATES {
        if state == entry.state_type.off {
            return Some(entry.state_type.on);
        }
    }
    None
}

fn is_excluded_member(entity_id: &str) -> bool {
    entity_id
        .split_once('.')
        .is_some_and(|(domain, _)| EXCLUDED_DOMAINS.contains(&domain))
}

fn record_configs(groups: &BTreeMap<String, GroupRecord>) -> BTreeMap<String, GroupConfig> {
    groups
        .iter()
        .map(|(object_id, record)| (object_id.clone(), record.config.clone()))
        .collect()
}

fn group_state_types(
    groups: &BTreeMap<String, GroupRecord>,
) -> hass_core::Result<BTreeMap<String, Option<StateType>>> {
    let configs = record_configs(groups);
    let order = topological_order(&configs)?;
    let mut state_types = BTreeMap::new();
    for object_id in order {
        let config = &configs[&object_id];
        let mut types = BTreeSet::new();
        for entity_id in &config.entities {
            let (domain, member_object_id) = entity_id.split_once('.').ok_or_else(|| {
                Error::InvalidInput(format!("invalid member entity ID {entity_id:?}"))
            })?;
            if domain == DOMAIN {
                if let Some(Some(state_type)) = state_types.get(member_object_id) {
                    types.insert(*state_type);
                }
            } else if let Some(entry) = domain_states(domain) {
                types.insert(entry.state_type);
            } else {
                types.insert(DEFAULT_STATE_TYPE);
            }
        }
        let value = if types.len() == 1 {
            types.first().copied()
        } else {
            None
        };
        state_types.insert(object_id, value);
    }
    Ok(state_types)
}

fn aggregate_group(
    runtime: &Runtime,
    object_id: &str,
    record: &GroupRecord,
    state_types: &BTreeMap<String, Option<StateType>>,
) -> hass_core::Result<(String, Map<String, Value>)> {
    let mut tracked = Vec::new();
    let mut known_on_states = BTreeSet::new();
    for entity_id in &record.config.entities {
        if is_excluded_member(entity_id) {
            continue;
        }
        let Some(state) = runtime.get_state(entity_id) else {
            continue;
        };
        let state_value = state.state.as_str();
        let is_on = if let Some(domain) = entity_id.split_once('.').map(|(domain, _)| domain)
            && let Some(entry) = domain_states(domain)
        {
            known_on_states.extend(entry.active.iter().copied());
            entry.active.contains(&state_value)
        } else {
            if let Some(active_state) = global_on_state(state_value) {
                known_on_states.insert(active_state);
            } else if let Some(active_state) = global_active_for_off(state_value) {
                known_on_states.insert(active_state);
            }
            global_on_state(state_value).is_some()
        };
        let assumed = state
            .attributes
            .get("assumed_state")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        tracked.push((is_on, assumed));
    }

    let state = if tracked.is_empty() || known_on_states.is_empty() {
        "unknown".to_owned()
    } else {
        let on = if record.config.all {
            tracked.iter().all(|(is_on, _)| *is_on)
        } else {
            tracked.iter().any(|(is_on, _)| *is_on)
        };
        match state_types.get(object_id).copied().flatten() {
            Some(state_type) if on => state_type.on.to_owned(),
            Some(state_type) => state_type.off.to_owned(),
            None if on => "on".to_owned(),
            None => "off".to_owned(),
        }
    };

    let mut attributes = Map::new();
    attributes.insert(
        "entity_id".into(),
        Value::Array(
            record
                .config
                .entities
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    attributes.insert("order".into(), json!(record.order));
    attributes.insert(
        "friendly_name".into(),
        Value::String(record.display_name(object_id).to_owned()),
    );
    if let Some(icon) = &record.config.icon {
        attributes.insert("icon".into(), Value::String(icon.clone()));
    }
    if record.created_by_service {
        attributes.insert("auto".into(), Value::Bool(true));
    }
    let assumed_state = !tracked.is_empty()
        && if record.config.all {
            tracked.iter().all(|(_, assumed)| *assumed)
        } else {
            tracked.iter().any(|(_, assumed)| *assumed)
        };
    if assumed_state {
        attributes.insert("assumed_state".into(), Value::Bool(true));
    }
    Ok((state, attributes))
}

fn recompute_all(
    runtime: &Runtime,
    groups: &BTreeMap<String, GroupRecord>,
    context: &Context,
) -> hass_core::Result<Vec<State>> {
    let all: BTreeSet<_> = groups.keys().cloned().collect();
    recompute_selected(runtime, groups, &all, context)
}

fn recompute_selected(
    runtime: &Runtime,
    groups: &BTreeMap<String, GroupRecord>,
    selected: &BTreeSet<String>,
    context: &Context,
) -> hass_core::Result<Vec<State>> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let configs = record_configs(groups);
    let order = topological_order(&configs)?;
    let state_types = group_state_types(groups)?;
    let mut changed_states = Vec::new();
    for object_id in order {
        if !selected.contains(&object_id) {
            continue;
        }
        let record = &groups[&object_id];
        let (state, attributes) = aggregate_group(runtime, &object_id, record, &state_types)?;
        let entity_id = GroupRecord::entity_id(&object_id);
        let change = runtime.set_state(&entity_id, &state, attributes, context.clone(), false)?;
        if change.changed {
            changed_states.push(change.state);
        }
    }
    Ok(changed_states)
}

fn affected_group_ids(
    groups: &BTreeMap<String, GroupRecord>,
    trigger_entity_id: &str,
    include_trigger_group: bool,
) -> hass_core::Result<BTreeSet<String>> {
    let mut reverse: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (object_id, record) in groups {
        for member in &record.config.entities {
            reverse
                .entry(member.clone())
                .or_default()
                .push(object_id.clone());
        }
    }
    let mut affected = BTreeSet::new();
    let mut queue = vec![trigger_entity_id.to_ascii_lowercase()];
    if include_trigger_group
        && let Some(object_id) = queue[0].strip_prefix("group.")
        && groups.contains_key(object_id)
    {
        affected.insert(object_id.to_owned());
    }
    let mut cursor = 0;
    let mut depth = 0;
    while cursor < queue.len() {
        depth += 1;
        if depth > MAX_GROUPS + 1 {
            return Err(Error::InvalidInput(
                "group dependency traversal exceeded its configured bound".into(),
            ));
        }
        let entity_id = &queue[cursor];
        cursor += 1;
        if let Some(parents) = reverse.get(entity_id) {
            for parent in parents {
                if affected.insert(parent.clone()) {
                    queue.push(GroupRecord::entity_id(parent));
                }
            }
        }
    }
    Ok(affected)
}

fn rollback_states(runtime: &Runtime, states: BTreeMap<String, Option<State>>, context: &Context) {
    for (entity_id, state) in states {
        let result = match state {
            Some(state) => runtime
                .set_state(
                    &entity_id,
                    &state.state,
                    state.attributes,
                    state.context,
                    true,
                )
                .map(|_| ()),
            None => runtime
                .remove_state(&entity_id, context.clone())
                .map(|_| ()),
        };
        if let Err(error) = result {
            tracing::error!(%error, %entity_id, "failed to roll back native group state");
        }
    }
}

async fn state_event_loop(
    core: Weak<IntegrationCore>,
    mut receiver: broadcast::Receiver<Event>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop => break,
            event = receiver.recv() => {
                match event {
                    Ok(event) => {
                        if event.event_type == "state_changed" {
                            process_state_event(&core, &event, false);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        process_state_event(&core, &Event {
                            event_type: "state_changed".into(),
                            data: Map::new(),
                            origin: "LOCAL".into(),
                            time_fired: chrono::Utc::now(),
                            context: latest_context(&core),
                        }, true);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn latest_context(core: &Weak<IntegrationCore>) -> Context {
    let Some(core) = core.upgrade() else {
        return Context::default();
    };
    core.runtime
        .states()
        .into_iter()
        .max_by_key(|state| state.last_updated)
        .map(|state| state.context)
        .unwrap_or_default()
}

fn process_state_event(core: &Weak<IntegrationCore>, event: &Event, all: bool) {
    let Some(core) = core.upgrade() else {
        return;
    };
    let _lifecycle = core.lock_lifecycle();
    if !core.index.active.load(Ordering::Acquire) {
        return;
    }
    let groups = core.snapshot_groups();
    let affected = if all {
        groups.keys().cloned().collect()
    } else {
        let Some(entity_id) = event.data.get("entity_id").and_then(Value::as_str) else {
            return;
        };
        match affected_group_ids(&groups, entity_id, false) {
            Ok(affected) => affected,
            Err(error) => {
                tracing::error!(%error, "cannot compute native group dependencies");
                return;
            }
        }
    };
    if affected.is_empty() {
        return;
    }
    if let Err(error) = recompute_selected(&core.runtime, &groups, &affected, &event.context) {
        tracing::error!(%error, "failed to update native groups from member state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use hass_core::{
        EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, Store,
    };
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    fn empty_attributes() -> Map<String, Value> {
        Map::new()
    }

    fn group_config(entities: &[&str], all: bool) -> GroupConfig {
        GroupConfig {
            name: None,
            entities: entities
                .iter()
                .map(|entity_id| (*entity_id).into())
                .collect(),
            icon: None,
            all,
        }
    }

    fn service_call(service: &str, data: Value, context: Context) -> ServiceCall {
        ServiceCall {
            domain: DOMAIN.into(),
            service: service.into(),
            data: data.as_object().expect("service data is an object").clone(),
            target: Map::new(),
            context,
        }
    }

    async fn wait_for_state(runtime: &Runtime, entity_id: &str, value: &str) -> State {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(state) = runtime.get_state(entity_id)
                    && state.state == value
                {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("group state update timed out")
    }

    #[test]
    fn config_accepts_legacy_entity_shorthand_and_rejects_unknown_fields() {
        let one: GroupConfig = serde_yaml::from_str("light.kitchen").unwrap();
        assert_eq!(one.entities, ["light.kitchen"]);
        let list: GroupConfig = serde_yaml::from_str("- light.kitchen\n- light.hall\n").unwrap();
        assert_eq!(list.entities, ["light.kitchen", "light.hall"]);
        let object: GroupConfig = serde_yaml::from_str(
            "name: Kitchen\nentities: light.kitchen\nicon: mdi:lightbulb\nall: true\n",
        )
        .unwrap();
        assert_eq!(object.name.as_deref(), Some("Kitchen"));
        assert_eq!(object.entities, ["light.kitchen"]);
        assert!(object.all);
        assert!(
            serde_yaml::from_str::<GroupConfig>(
                "name: Kitchen\nentities: [light.kitchen]\nplatform: group\n"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn validation_rejects_cycles_before_state_mutation() {
        let config = BTreeMap::from([
            ("one".into(), group_config(&["group.two"], false)),
            ("two".into(), group_config(&["group.one"], false)),
        ]);
        assert!(matches!(validate(&config), Err(Error::InvalidInput(_))));
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        assert!(
            GroupIntegration::start(runtime.clone(), config)
                .await
                .is_err()
        );
        assert!(runtime.states().is_empty());
    }

    #[tokio::test]
    async fn actual_member_updates_aggregate_nested_all_unknown_and_typed_states() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime
            .set_state(
                "light.kitchen",
                "off",
                json!({"assumed_state": true}).as_object().unwrap().clone(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "light.hall",
                "on",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "person.alice",
                "home",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "person.bob",
                "not_home",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "sensor.temperature",
                "unavailable",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();

        let mut any_lights = group_config(&["Light.Kitchen", "light.hall"], false);
        any_lights.name = Some("Kitchen lights".into());
        any_lights.icon = Some("mdi:lightbulb-group".into());
        let mut all_lights = any_lights.clone();
        all_lights.name = Some("All kitchen lights".into());
        all_lights.all = true;
        let config = BTreeMap::from([
            ("any_lights".into(), any_lights),
            ("all_lights".into(), all_lights),
            (
                "people_inner".into(),
                group_config(&["person.alice"], false),
            ),
            (
                "people_outer".into(),
                group_config(&["group.people_inner", "person.bob"], true),
            ),
            (
                "people".into(),
                group_config(&["person.alice", "person.bob"], true),
            ),
            (
                "sensor_only".into(),
                group_config(&["sensor.temperature"], false),
            ),
        ]);
        let integration = GroupIntegration::start_with_order(
            runtime.clone(),
            config,
            vec![
                "any_lights".into(),
                "all_lights".into(),
                "people_inner".into(),
                "people_outer".into(),
                "people".into(),
                "sensor_only".into(),
            ],
        )
        .await
        .unwrap();
        let lookup = integration.lookup();

        assert_eq!(runtime.get_state("group.any_lights").unwrap().state, "on");
        assert_eq!(runtime.get_state("group.all_lights").unwrap().state, "off");
        assert_eq!(runtime.get_state("group.people").unwrap().state, "not_home");
        assert_eq!(
            runtime.get_state("group.people_inner").unwrap().state,
            "home"
        );
        assert_eq!(
            runtime.get_state("group.people_outer").unwrap().state,
            "not_home"
        );
        assert_eq!(
            runtime.get_state("group.sensor_only").unwrap().state,
            "unknown"
        );
        let state = runtime.get_state("group.any_lights").unwrap();
        assert_eq!(
            state.attributes["entity_id"],
            json!(["light.kitchen", "light.hall"])
        );
        assert_eq!(state.attributes["friendly_name"], "Kitchen lights");
        assert_eq!(state.attributes["icon"], "mdi:lightbulb-group");
        assert_eq!(state.attributes["order"], 0);
        assert_eq!(state.attributes["assumed_state"], true);
        assert_eq!(
            lookup.members("group.any_lights").unwrap(),
            ["light.kitchen", "light.hall"]
        );

        let context = Context {
            id: "light-update-context".into(),
            parent_id: Some("parent-action".into()),
            user_id: Some("test-user".into()),
        };
        runtime
            .set_state(
                "light.kitchen",
                "on",
                empty_attributes(),
                context.clone(),
                false,
            )
            .unwrap();
        let all_state = wait_for_state(&runtime, "group.all_lights", "on").await;
        assert_eq!(all_state.context, context);

        runtime
            .set_state(
                "person.bob",
                "home",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        assert_eq!(
            wait_for_state(&runtime, "group.people", "home").await.state,
            "home"
        );
        assert_eq!(
            wait_for_state(&runtime, "group.people_outer", "home")
                .await
                .state,
            "home"
        );

        runtime
            .set_state(
                "light.kitchen",
                "unavailable",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "light.hall",
                "unavailable",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        assert_eq!(
            wait_for_state(&runtime, "group.any_lights", "unknown")
                .await
                .state,
            "unknown"
        );
        assert_eq!(
            wait_for_state(&runtime, "group.all_lights", "unknown")
                .await
                .state,
            "unknown"
        );

        integration.shutdown().await.unwrap();
        assert!(lookup.members("group.any_lights").is_none());
    }

    #[tokio::test]
    async fn dynamic_groups_remove_on_restart_and_yaml_definition_wins() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        runtime
            .set_state(
                "switch.lamp",
                "off",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = GroupIntegration::start(runtime.clone(), BTreeMap::new())
            .await
            .unwrap();
        let lookup = integration.lookup();
        runtime
            .call_service(service_call(
                SERVICE_SET,
                json!({
                    "object_id": "temporary",
                    "name": "Temporary group",
                    "icon": "mdi:lamp",
                    "entities": ["switch.lamp"]
                }),
                Context::default(),
            ))
            .unwrap();
        assert_eq!(lookup.members("group.temporary").unwrap(), ["switch.lamp"]);
        let state = runtime.get_state("group.temporary").unwrap();
        assert_eq!(state.state, "off");
        assert_eq!(state.attributes["auto"], true);
        assert_eq!(state.attributes["friendly_name"], "Temporary group");
        assert_eq!(state.attributes["order"], 0);
        runtime
            .call_service(service_call(
                SERVICE_SET,
                json!({
                    "object_id": "temporary",
                    "add_entities": ["switch.fan"]
                }),
                Context::default(),
            ))
            .unwrap();
        assert_eq!(
            lookup.members("group.temporary").unwrap(),
            ["switch.lamp", "switch.fan"]
        );
        runtime
            .call_service(service_call(
                SERVICE_SET,
                json!({
                    "object_id": "other",
                    "entities": ["group.temporary"]
                }),
                Context::default(),
            ))
            .unwrap();
        assert!(
            runtime
                .call_service(service_call(
                    SERVICE_SET,
                    json!({
                        "object_id": "temporary",
                        "entities": ["group.other"]
                    }),
                    Context::default(),
                ))
                .is_err(),
            "dynamic membership cycles are rejected"
        );
        assert_eq!(
            lookup.members("group.temporary").unwrap(),
            ["switch.lamp", "switch.fan"]
        );
        runtime
            .call_service(service_call(
                SERVICE_REMOVE,
                json!({"object_id": "other"}),
                Context::default(),
            ))
            .unwrap();
        assert!(
            runtime
                .call_service(service_call(
                    SERVICE_SET,
                    json!({
                        "object_id": "temporary",
                        "entities": ["switch.lamp"],
                        "add_entities": ["switch.fan"]
                    }),
                    Context::default(),
                ))
                .is_err(),
            "pinned group.set rejects multiple membership fields"
        );
        // The strict service schema accepts only one membership operation at
        // a time. The rejected mixed request leaves the active list intact.
        runtime
            .call_service(service_call(
                SERVICE_REMOVE,
                json!({"object_id": "temporary"}),
                Context::default(),
            ))
            .unwrap();
        assert!(runtime.get_state("group.temporary").is_none());
        assert!(lookup.members("group.temporary").is_none());
        runtime
            .call_service(service_call(
                SERVICE_SET,
                json!({
                    "object_id": "temporary",
                    "name": "Temporary group",
                    "entities": ["switch.lamp"]
                }),
                Context::default(),
            ))
            .unwrap();
        integration.shutdown().await.unwrap();
        drop(runtime);

        let runtime = Runtime::new(store.clone()).unwrap();
        assert!(runtime.get_state("group.temporary").is_some());
        let yaml = BTreeMap::from([("temporary".into(), {
            let mut config = group_config(&["switch.fan"], false);
            config.name = Some("YAML group".into());
            config
        })]);
        let yaml_run = GroupIntegration::start(runtime.clone(), yaml)
            .await
            .unwrap();
        let yaml_lookup = yaml_run.lookup();
        assert_eq!(
            yaml_lookup.members("group.temporary").unwrap(),
            ["switch.fan"]
        );
        let state = runtime.get_state("group.temporary").unwrap();
        assert_eq!(state.attributes["friendly_name"], "YAML group");
        assert!(state.attributes.get("auto").is_none());
        yaml_run.shutdown().await.unwrap();
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let final_run = GroupIntegration::start(runtime.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert!(runtime.get_state("group.temporary").is_none());
        assert!(final_run.lookup().members("group.temporary").is_none());
        final_run.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_metadata_write_does_not_publish_dynamic_membership() {
        let store = Arc::new(FailingMetadataStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = GroupIntegration::start(runtime.clone(), BTreeMap::new())
            .await
            .unwrap();
        let lookup = integration.lookup();
        store.fail_metadata.store(true, Ordering::Release);
        assert!(
            runtime
                .call_service(service_call(
                    SERVICE_SET,
                    json!({"object_id": "will_fail", "entities": ["light.a"]}),
                    Context::default(),
                ))
                .is_err()
        );
        assert!(lookup.members("group.will_fail").is_none());
        assert!(runtime.get_state("group.will_fail").is_none());
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_cleanup_touches_only_group_ids_owned_by_this_integration() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime
            .set_state(
                "group.unrelated",
                "on",
                json!({"entity_id": ["light.keep"]})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_data("other.integration.metadata", &json!({"keep": true}))
            .unwrap();
        let integration = GroupIntegration::start(runtime.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(runtime.get_state("group.unrelated").unwrap().state, "on");
        assert_eq!(
            runtime.get_data("other.integration.metadata").unwrap(),
            Some(json!({"keep": true}))
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn expansion_prefers_active_groups_then_legacy_state_attributes() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime
            .set_state(
                "group.legacy",
                "on",
                json!({"entity_id": ["light.one", "group.inner"]})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state(
                "group.inner",
                "on",
                json!({"entity_id": ["switch.two"]})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
                false,
            )
            .unwrap();
        let integration = GroupIntegration::start(
            runtime.clone(),
            BTreeMap::from([(
                "native".into(),
                group_config(&["light.native", "group.legacy"], false),
            )]),
        )
        .await
        .unwrap();
        let ids = vec![
            "group.native".into(),
            "group.legacy".into(),
            "light.one".into(),
            "unknown.sensor".into(),
            "all".into(),
        ];
        assert_eq!(
            expand_entity_ids(&runtime, &integration.lookup(), &ids).unwrap(),
            ["light.native", "light.one", "switch.two", "unknown.sensor"]
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_and_drop_stop_callbacks_services_and_membership_leases() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        runtime
            .set_state(
                "switch.lamp",
                "off",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        let config = BTreeMap::from([("lamps".into(), group_config(&["switch.lamp"], false))]);
        let integration = GroupIntegration::start(runtime.clone(), config.clone())
            .await
            .unwrap();
        let lookup = integration.lookup();
        integration.shutdown().await.unwrap();
        assert!(lookup.members("group.lamps").is_none());
        assert!(matches!(
            runtime.call_service(service_call(
                SERVICE_SET,
                json!({"object_id": "not_registered"}),
                Context::default(),
            )),
            Err(Error::Service(_))
        ));
        runtime
            .set_state(
                "switch.lamp",
                "on",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(runtime.get_state("group.lamps").unwrap().state, "off");

        let integration = GroupIntegration::start(runtime.clone(), config)
            .await
            .unwrap();
        let lookup = integration.lookup();
        drop(integration);
        assert!(lookup.members("group.lamps").is_none());
        runtime
            .set_state(
                "switch.lamp",
                "off",
                empty_attributes(),
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(runtime.get_state("group.lamps").unwrap().state, "on");
        assert!(Runtime::new(store.clone()).is_err());
        drop(runtime);
        assert!(Runtime::new(store).is_ok());
    }

    #[derive(Default)]
    struct FailingMetadataStore {
        inner: MemoryStore,
        fail_metadata: AtomicBool,
    }

    impl Store for FailingMetadataStore {
        fn load_states(&self) -> hass_core::Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> hass_core::Result<()> {
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> hass_core::Result<()> {
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> hass_core::Result<()> {
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
            if key == OWNED_IDS_DATA_KEY && self.fail_metadata.load(Ordering::Acquire) {
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
            start: DateTime<Utc>,
            end: DateTime<Utc>,
        ) -> hass_core::Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }

        fn history_query(&self, query: &HistoryQuery) -> hass_core::Result<Vec<HistorySeries>> {
            self.inner.history_query(query)
        }

        fn event_history_query(&self, query: &EventHistoryQuery) -> hass_core::Result<Vec<Event>> {
            self.inner.event_history_query(query)
        }

        fn purge_before(&self, cutoff: DateTime<Utc>) -> hass_core::Result<PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn repack(&self) -> hass_core::Result<()> {
            self.inner.repack()
        }
    }
}
