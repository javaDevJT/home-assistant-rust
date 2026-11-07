use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use hass_core::{Context, Error};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{RepairsError, RepairsManager, RepairsResult, Shared, lock};

pub const STORAGE_KEY: &str = "repairs.issue_registry";
pub const STORAGE_MAJOR: u64 = 1;
pub const STORAGE_MINOR: u64 = 4;
pub const EVENT_REPAIRS_ISSUE_REGISTRY_UPDATED: &str = "repairs_issue_registry_updated";
pub type IssueDateTime = DateTime<FixedOffset>;

/// Upstream deliberately accepts arbitrary severity strings; newer or custom
/// values therefore remain representable at the native boundary.
pub type IssueSeverity = String;

/// Producer-supplied issue fields. The existing issue's creation and dismissal
/// metadata are retained when this draft replaces the same identity.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueDraft {
    pub domain: String,
    pub issue_id: String,
    pub is_fixable: bool,
    pub severity: Option<IssueSeverity>,
    pub translation_key: Option<String>,
    pub is_persistent: bool,
    pub breaks_in_ha_version: Option<String>,
    pub config_entry_id: Option<String>,
    pub config_subentry_id: Option<String>,
    pub data: Option<Map<String, Value>>,
    pub issue_domain: Option<String>,
    pub learn_more_url: Option<String>,
    pub translation_placeholders: Option<Map<String, Value>>,
}

impl IssueDraft {
    pub fn new(
        domain: impl Into<String>,
        issue_id: impl Into<String>,
        is_fixable: bool,
        severity: impl Into<String>,
        translation_key: impl Into<String>,
    ) -> Self {
        Self {
            domain: domain.into(),
            issue_id: issue_id.into(),
            is_fixable,
            severity: Some(severity.into()),
            translation_key: Some(translation_key.into()),
            is_persistent: false,
            breaks_in_ha_version: None,
            config_entry_id: None,
            config_subentry_id: None,
            data: None,
            issue_domain: None,
            learn_more_url: None,
            translation_placeholders: None,
        }
    }
}

/// Complete in-memory issue. `active` is intentionally omitted from storage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueEntry {
    pub active: bool,
    pub breaks_in_ha_version: Option<String>,
    pub config_entry_id: Option<String>,
    pub config_subentry_id: Option<String>,
    pub created: DateTime<FixedOffset>,
    pub data: Option<Map<String, Value>>,
    pub dismissed_version: Option<String>,
    pub domain: String,
    pub is_fixable: Option<bool>,
    pub is_persistent: bool,
    pub issue_domain: Option<String>,
    pub issue_id: String,
    pub learn_more_url: Option<String>,
    pub severity: Option<IssueSeverity>,
    pub translation_key: Option<String>,
    pub translation_placeholders: Option<Map<String, Value>>,
}

impl IssueEntry {
    pub fn summary(&self) -> IssueSummary {
        IssueSummary {
            breaks_in_ha_version: self.breaks_in_ha_version.clone(),
            created: self.created,
            dismissed_version: self.dismissed_version.clone(),
            ignored: self.dismissed_version.is_some(),
            domain: self.domain.clone(),
            is_fixable: self.is_fixable.unwrap_or(false),
            issue_domain: self.issue_domain.clone(),
            issue_id: self.issue_id.clone(),
            learn_more_url: self.learn_more_url.clone(),
            severity: self.severity.clone(),
            translation_key: self.translation_key.clone(),
            translation_placeholders: string_placeholders(&self.translation_placeholders),
        }
    }
}

/// The public Repairs list item. Issue data and config-entry IDs never appear.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub breaks_in_ha_version: Option<String>,
    pub created: DateTime<FixedOffset>,
    pub dismissed_version: Option<String>,
    pub ignored: bool,
    pub domain: String,
    pub is_fixable: bool,
    pub issue_domain: Option<String>,
    pub issue_id: String,
    pub learn_more_url: Option<String>,
    pub severity: Option<IssueSeverity>,
    pub translation_key: Option<String>,
    pub translation_placeholders: Option<BTreeStringMap>,
}

/// JSON string map used by the translation API DTO.
pub type BTreeStringMap = std::collections::BTreeMap<String, String>;

type IssueKey = (String, String);

#[derive(Clone)]
struct ActiveRefresh {
    selected: HashSet<IssueKey>,
    seen: HashSet<IssueKey>,
}

#[derive(Clone)]
pub(crate) struct RegistryState {
    issues: Vec<IssueEntry>,
    refreshes: HashMap<u64, ActiveRefresh>,
    deferred_deletes: HashSet<IssueKey>,
    next_refresh: u64,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            issues: Vec::new(),
            refreshes: HashMap::new(),
            deferred_deletes: HashSet::new(),
            next_refresh: 1,
        }
    }
}

/// A refresh selects existing identities once, then removes only untouched
/// rows that are still owned by the newest overlapping refresh.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshScope {
    pub domain: String,
    pub issue_ids: Option<Vec<String>>,
    pub pattern: Option<String>,
    pub config_entry_id: Option<String>,
}

impl RefreshScope {
    pub fn all(domain: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            ..Self::default()
        }
    }

    pub fn ids(domain: impl Into<String>, issue_ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            domain: domain.into(),
            issue_ids: Some(issue_ids.into_iter().collect()),
            ..Self::default()
        }
    }

    pub fn matching(domain: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            pattern: Some(pattern.into()),
            ..Self::default()
        }
    }

    pub fn for_config_entry(mut self, entry_id: impl Into<String>) -> Self {
        self.config_entry_id = Some(entry_id.into());
        self
    }
}

/// RAII refresh scope. Dropping it cancels cleanup but applies explicitly
/// deferred deletes in its scope unless a newer matching refresh still owns them.
pub struct IssueRefresh {
    manager: RepairsManager,
    id: u64,
    domain: String,
    finished: bool,
}

impl IssueRefresh {
    /// Mark an ID as seen in this refresh and older matching scopes.
    pub fn keep(&self, issue_id: &str) -> RepairsResult<()> {
        if self.finished {
            return Ok(());
        }
        let shared = self.manager.live()?;
        keep_in_refresh(
            &shared,
            self.id,
            &(self.domain.clone(), issue_id.to_owned()),
        );
        Ok(())
    }

    /// Commit untouched-row cleanup. Repeating finish is harmless.
    pub fn finish(&mut self) -> RepairsResult<()> {
        if self.finished {
            return Ok(());
        }
        finish_refresh(&self.manager, self.id, true)?;
        self.finished = true;
        Ok(())
    }

    /// Cancel cleanup, preserving the refresh's untouched issues.
    pub fn cancel(&mut self) -> RepairsResult<()> {
        if self.finished {
            return Ok(());
        }
        finish_refresh(&self.manager, self.id, false)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for IssueRefresh {
    fn drop(&mut self) {
        if !self.finished {
            let _ = finish_refresh(&self.manager, self.id, false);
            self.finished = true;
        }
    }
}

pub(crate) fn load_registry(
    stored: Option<Value>,
    entries: &dyn crate::ConfigEntryResolver,
    subentries: Option<&dyn crate::ConfigSubentryResolver>,
) -> RepairsResult<(RegistryState, bool, Vec<RegistryChange>)> {
    let Some(value) = stored else {
        return Ok((RegistryState::default(), false, Vec::new()));
    };
    let object = value
        .as_object()
        .ok_or_else(|| invalid("issue registry must be an object"))?;
    let major = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("issue registry version must be an integer"))?;
    let minor = match object.get("minor_version") {
        None => 1,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid("issue registry minor_version must be an integer"))?,
    };
    if major != STORAGE_MAJOR || !(1..=STORAGE_MINOR).contains(&minor) {
        return Err(invalid(format!(
            "unsupported issue registry version {major}.{minor}"
        )));
    }
    let stored_issues = object
        .get("data")
        .and_then(|data| data.get("issues"))
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("issue registry data.issues must be an array"))?;
    let mut registry = RegistryState::default();
    for raw in stored_issues {
        let issue = parse_stored_issue(raw, minor)?;
        let key = (issue.domain.clone(), issue.issue_id.clone());
        if let Some(existing) = registry
            .issues
            .iter_mut()
            .find(|entry| key_of(entry) == key)
        {
            *existing = issue;
        } else {
            registry.issues.push(issue);
        }
    }
    // ConfigEntries is synchronously loaded before this manager is started.
    // Subentries are pruned only when a real resolver can confirm absence.
    let before = registry.issues.len();
    registry.issues.retain(|issue| {
        let Some(entry_id) = issue.config_entry_id.as_deref() else {
            return true;
        };
        if entries.get_entry(entry_id).is_none() {
            return false;
        }
        match (issue.config_subentry_id.as_deref(), subentries) {
            (Some(subentry_id), Some(resolver)) => resolver.subentry_exists(entry_id, subentry_id),
            _ => true,
        }
    });
    let mut changes = Vec::new();
    if before != registry.issues.len() {
        let removed: HashSet<IssueKey> = parse_issue_keys(stored_issues)?
            .into_iter()
            .filter(|key| !registry.issues.iter().any(|e| key_of(e) == *key))
            .collect();
        changes.extend(
            removed
                .into_iter()
                .map(|(domain, issue_id)| RegistryChange::new("remove", domain, issue_id)),
        );
    }
    let canonical = storage_document(&registry.issues);
    Ok((registry, canonical != value, changes))
}

fn parse_issue_keys(stored_issues: &[Value]) -> RepairsResult<Vec<IssueKey>> {
    stored_issues
        .iter()
        .map(|raw| {
            let object = raw
                .as_object()
                .ok_or_else(|| invalid("issue row must be an object"))?;
            Ok((
                required_string(object, "domain")?.to_owned(),
                required_string(object, "issue_id")?.to_owned(),
            ))
        })
        .collect()
}

fn parse_stored_issue(raw: &Value, minor: u64) -> RepairsResult<IssueEntry> {
    let object = raw
        .as_object()
        .ok_or_else(|| invalid("issue row must be an object"))?;
    let domain = required_string(object, "domain")?.to_owned();
    let issue_id = required_string(object, "issue_id")?.to_owned();
    let created = required_string(object, "created")?;
    let created = DateTime::parse_from_rfc3339(created)
        .map_err(|_| invalid("issue created must be an ISO datetime"))?;
    let is_persistent = match object.get("is_persistent") {
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("issue is_persistent must be a boolean")),
        None if minor == 1 => false,
        None => return Err(invalid("issue is_persistent is required")),
    };
    if is_persistent && !object.contains_key("is_fixable") {
        return Err(invalid("persistent issue is missing is_fixable"));
    }
    let config_entry_id = if minor < 2 {
        None
    } else {
        optional_string(object, "config_entry_id")?
    };
    let config_subentry_id = if minor < 3 {
        None
    } else {
        optional_string(object, "config_subentry_id")?
    };
    let dismissed_version = optional_string(object, "dismissed_version")?;
    let mut issue = IssueEntry {
        active: is_persistent,
        breaks_in_ha_version: optional_string(object, "breaks_in_ha_version")?,
        config_entry_id,
        config_subentry_id,
        created,
        data: optional_value_map(object, "data")?,
        dismissed_version,
        domain,
        is_fixable: if is_persistent {
            optional_bool(object, "is_fixable")?
        } else {
            None
        },
        is_persistent,
        issue_domain: optional_string(object, "issue_domain")?,
        issue_id,
        learn_more_url: optional_string(object, "learn_more_url")?,
        severity: optional_string(object, "severity")?,
        translation_key: optional_string(object, "translation_key")?,
        translation_placeholders: optional_value_map(object, "translation_placeholders")?,
    };
    if !is_persistent {
        issue.active = false;
        issue.breaks_in_ha_version = None;
        issue.data = None;
        issue.is_fixable = None;
        issue.issue_domain = None;
        issue.learn_more_url = None;
        issue.severity = None;
        issue.translation_key = None;
        issue.translation_placeholders = None;
    }
    if issue.config_subentry_id.is_some() && issue.config_entry_id.is_none() {
        return Err(invalid(
            "stored config_subentry_id requires config_entry_id",
        ));
    }
    if issue
        .data
        .as_ref()
        .is_some_and(|values| values.values().any(|value| !is_issue_data_value(value)))
    {
        return Err(invalid(
            "stored issue data must contain only strings, numbers, or null",
        ));
    }
    if issue
        .translation_placeholders
        .as_ref()
        .is_some_and(|values| values.values().any(|value| !value.is_string()))
    {
        return Err(invalid(
            "stored translation placeholder values must be strings",
        ));
    }
    Ok(issue)
}

fn optional_value_map(
    object: &Map<String, Value>,
    key: &str,
) -> RepairsResult<Option<Map<String, Value>>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(format!("issue {key} must be an object or null"))),
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> RepairsResult<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(format!("issue {key} must be a string or null"))),
    }
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> RepairsResult<Option<bool>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid(format!("issue {key} must be a boolean"))),
    }
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> RepairsResult<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("issue {key} must be a string")))
}

pub(crate) fn storage_value(registry: &RegistryState) -> RepairsResult<Value> {
    Ok(storage_document(&registry.issues))
}

fn storage_document(issues: &[IssueEntry]) -> Value {
    json!({
        "version": STORAGE_MAJOR,
        "minor_version": STORAGE_MINOR,
        "data": {"issues": issues.iter().map(issue_storage_row).collect::<Vec<_>>()},
    })
}

fn issue_storage_row(issue: &IssueEntry) -> Value {
    let mut row = Map::new();
    row.insert(
        "config_entry_id".into(),
        option_json(issue.config_entry_id.clone()),
    );
    row.insert(
        "config_subentry_id".into(),
        option_json(issue.config_subentry_id.clone()),
    );
    row.insert(
        "created".into(),
        json!(issue.created.to_rfc3339_opts(SecondsFormat::AutoSi, false)),
    );
    row.insert(
        "dismissed_version".into(),
        option_json(issue.dismissed_version.clone()),
    );
    row.insert("domain".into(), json!(issue.domain));
    row.insert("is_persistent".into(), json!(issue.is_persistent));
    row.insert("issue_id".into(), json!(issue.issue_id));
    if issue.is_persistent {
        row.insert(
            "breaks_in_ha_version".into(),
            option_json(issue.breaks_in_ha_version.clone()),
        );
        row.insert(
            "data".into(),
            issue.data.clone().map(Value::Object).unwrap_or(Value::Null),
        );
        row.insert("is_fixable".into(), json!(issue.is_fixable));
        row.insert(
            "issue_domain".into(),
            option_json(issue.issue_domain.clone()),
        );
        row.insert(
            "learn_more_url".into(),
            option_json(issue.learn_more_url.clone()),
        );
        row.insert("severity".into(), option_json(issue.severity.clone()));
        row.insert(
            "translation_key".into(),
            option_json(issue.translation_key.clone()),
        );
        row.insert(
            "translation_placeholders".into(),
            issue
                .translation_placeholders
                .clone()
                .map(Value::Object)
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(row)
}

fn option_json(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

pub(crate) fn dispatch_change(
    runtime: &hass_core::Runtime,
    change: RegistryChange,
) -> RepairsResult<()> {
    runtime.fire_event(
        EVENT_REPAIRS_ISSUE_REGISTRY_UPDATED,
        json!({"action":change.action,"domain":change.domain,"issue_id":change.issue_id})
            .as_object()
            .expect("change payload is an object")
            .clone(),
        Context::default(),
    )?;
    Ok(())
}

pub(crate) struct RegistryChange {
    action: &'static str,
    domain: String,
    issue_id: String,
}

impl RegistryChange {
    fn new(action: &'static str, domain: String, issue_id: String) -> Self {
        Self {
            action,
            domain,
            issue_id,
        }
    }
}

impl RepairsManager {
    pub fn create_issue(&self, draft: IssueDraft) -> RepairsResult<IssueEntry> {
        validate_draft(&draft)?;
        let shared = self.live()?;
        validate_links(&shared, &draft.config_entry_id, &draft.config_subentry_id)?;
        let key = (draft.domain.clone(), draft.issue_id.clone());
        let _gate = lock(&shared.registry_gate);
        let current = lock(&shared.registry).clone();
        let existing = current
            .issues
            .iter()
            .find(|issue| key_of(issue) == key)
            .cloned();
        let replacement = entry_from_draft(draft, existing.as_ref());
        if existing.as_ref().is_some_and(|issue| issue == &replacement) {
            let mut state = lock(&shared.registry);
            touch(&mut state, &key);
            return Ok(replacement);
        }
        let mut candidate = current;
        if let Some(index) = candidate
            .issues
            .iter()
            .position(|issue| key_of(issue) == key)
        {
            candidate.issues[index] = replacement.clone();
        } else {
            candidate.issues.push(replacement.clone());
        }
        commit(&shared, candidate)?;
        let mut state = lock(&shared.registry);
        touch(&mut state, &key);
        cancel_deferred(&mut state, &key);
        drop(state);
        dispatch_change(
            &shared.runtime,
            RegistryChange::new(
                if existing.is_none() {
                    "create"
                } else {
                    "update"
                },
                key.0,
                key.1,
            ),
        )?;
        Ok(replacement)
    }

    pub fn get_issue(&self, domain: &str, issue_id: &str) -> RepairsResult<Option<IssueEntry>> {
        let shared = self.live()?;
        Ok(lock(&shared.registry)
            .issues
            .iter()
            .find(|issue| issue.domain == domain && issue.issue_id == issue_id)
            .cloned())
    }

    pub fn get_issue_data(&self, domain: &str, issue_id: &str) -> RepairsResult<Value> {
        let issue =
            self.get_issue(domain, issue_id)?
                .ok_or_else(|| RepairsError::UnknownIssue {
                    domain: domain.into(),
                    issue_id: issue_id.into(),
                })?;
        Ok(issue.data.map(Value::Object).unwrap_or(Value::Null))
    }

    pub fn list_issues(&self) -> RepairsResult<Vec<IssueSummary>> {
        let shared = self.live()?;
        let issues = lock(&shared.registry).issues.clone();
        Ok(issues
            .into_iter()
            .filter(|issue| issue.active)
            .filter(|issue| {
                issue
                    .config_entry_id
                    .as_deref()
                    .and_then(|id| shared.entries.get_entry(id))
                    .is_none_or(|entry| entry.disabled_by.is_none())
            })
            .map(|issue| issue.summary())
            .collect())
    }

    pub fn ignore_issue(&self, domain: &str, issue_id: &str, ignore: bool) -> RepairsResult<()> {
        let shared = self.live()?;
        let key = (domain.to_owned(), issue_id.to_owned());
        let _gate = lock(&shared.registry_gate);
        let mut candidate = lock(&shared.registry).clone();
        let Some(index) = candidate
            .issues
            .iter()
            .position(|issue| key_of(issue) == key)
        else {
            return Err(unknown_issue(domain, issue_id));
        };
        let dismissed = if ignore {
            Some(shared.current_version.clone())
        } else {
            None
        };
        if candidate.issues[index].dismissed_version == dismissed {
            return Ok(());
        }
        candidate.issues[index].dismissed_version = dismissed;
        commit(&shared, candidate)?;
        {
            let mut registry = lock(&shared.registry);
            touch(&mut registry, &key);
        }
        dispatch_change(&shared.runtime, RegistryChange::new("update", key.0, key.1))
    }

    pub fn delete_issue(&self, domain: &str, issue_id: &str) -> RepairsResult<bool> {
        self.delete_issue_inner(domain, issue_id, false)
    }

    /// Request deletion after the latest active overlapping refresh has had a
    /// chance to recreate or keep the identity.
    pub fn delete_issue_deferred(&self, domain: &str, issue_id: &str) -> RepairsResult<bool> {
        self.delete_issue_inner(domain, issue_id, true)
    }

    fn delete_issue_inner(&self, domain: &str, issue_id: &str, defer: bool) -> RepairsResult<bool> {
        let shared = self.live()?;
        let key = (domain.to_owned(), issue_id.to_owned());
        let _gate = lock(&shared.registry_gate);
        let mut candidate = lock(&shared.registry).clone();
        let Some(index) = candidate
            .issues
            .iter()
            .position(|issue| key_of(issue) == key)
        else {
            return Ok(false);
        };
        if defer {
            let owner = candidate
                .refreshes
                .iter()
                .filter(|(_, refresh)| refresh.selected.contains(&key))
                .max_by_key(|(id, _)| *id);
            if let Some((_, owner)) = owner {
                if !owner.seen.contains(&key) {
                    candidate.deferred_deletes.insert(key.clone());
                }
                *lock(&shared.registry) = candidate;
                return Ok(true);
            }
        }
        candidate.issues.remove(index);
        touch(&mut candidate, &key);
        cancel_deferred(&mut candidate, &key);
        commit(&shared, candidate)?;
        dispatch_change(&shared.runtime, RegistryChange::new("remove", key.0, key.1))?;
        Ok(true)
    }

    /// Mark an identity seen by every active refresh that selected it.
    pub fn keep_issue(&self, domain: &str, issue_id: &str) -> RepairsResult<()> {
        let shared = self.live()?;
        let key = (domain.to_owned(), issue_id.to_owned());
        let _gate = lock(&shared.registry_gate);
        let mut state = lock(&shared.registry);
        touch(&mut state, &key);
        cancel_deferred(&mut state, &key);
        Ok(())
    }

    pub fn refresh_issues(&self, scope: RefreshScope) -> RepairsResult<IssueRefresh> {
        let shared = self.live()?;
        if scope.domain.is_empty() {
            return Err(invalid("refresh domain must not be empty"));
        }
        if scope.pattern.is_some() && scope.issue_ids.is_some() {
            return Err(invalid(
                "refresh pattern and issue_ids are mutually exclusive",
            ));
        }
        let compiled = scope
            .pattern
            .as_ref()
            .map(|pattern| {
                Regex::new(pattern)
                    .map_err(|error| invalid(format!("invalid refresh pattern: {error}")))
            })
            .transpose()?;
        let _gate = lock(&shared.registry_gate);
        let mut state = lock(&shared.registry);
        let selected = state
            .issues
            .iter()
            .filter(|issue| issue.domain == scope.domain)
            .filter(|issue| {
                scope
                    .config_entry_id
                    .as_ref()
                    .is_none_or(|id| issue.config_entry_id.as_ref() == Some(id))
            })
            .filter(|issue| {
                scope
                    .issue_ids
                    .as_ref()
                    .is_none_or(|ids| ids.iter().any(|id| id == &issue.issue_id))
            })
            .filter(|issue| {
                compiled.as_ref().is_none_or(|pattern| {
                    pattern
                        .find(&issue.issue_id)
                        .is_some_and(|matched| matched.start() == 0)
                })
            })
            .map(key_of)
            .collect();
        let id = state.next_refresh;
        state.next_refresh = state.next_refresh.wrapping_add(1).max(1);
        state.refreshes.insert(
            id,
            ActiveRefresh {
                selected,
                seen: HashSet::new(),
            },
        );
        Ok(IssueRefresh {
            manager: self.clone(),
            id,
            domain: scope.domain,
            finished: false,
        })
    }

    /// Notify active issues linked to an entry after its enabled state changes.
    pub fn config_entry_disabled_by_changed(&self, entry_id: &str) -> RepairsResult<()> {
        let shared = self.live()?;
        if shared.entries.get_entry(entry_id).is_none() {
            return Err(RepairsError::UnknownEntry(entry_id.into()));
        }
        let issues = lock(&shared.registry).issues.clone();
        for issue in issues
            .into_iter()
            .filter(|issue| issue.config_entry_id.as_deref() == Some(entry_id))
        {
            dispatch_change(
                &shared.runtime,
                RegistryChange::new("update", issue.domain, issue.issue_id),
            )?;
        }
        Ok(())
    }

    pub fn clear_config_entry(&self, entry_id: &str) -> RepairsResult<usize> {
        self.clear_config_link(entry_id, None)
    }

    pub fn clear_config_subentry(&self, entry_id: &str, subentry_id: &str) -> RepairsResult<usize> {
        self.clear_config_link(entry_id, Some(subentry_id))
    }

    fn clear_config_link(&self, entry_id: &str, subentry_id: Option<&str>) -> RepairsResult<usize> {
        let shared = self.live()?;
        let _gate = lock(&shared.registry_gate);
        let mut candidate = lock(&shared.registry).clone();
        let removed: Vec<_> = candidate
            .issues
            .iter()
            .filter(|issue| {
                issue.config_entry_id.as_deref() == Some(entry_id)
                    && subentry_id
                        .is_none_or(|sub| issue.config_subentry_id.as_deref() == Some(sub))
            })
            .map(key_of)
            .collect();
        if removed.is_empty() {
            return Ok(0);
        }
        candidate
            .issues
            .retain(|issue| !removed.contains(&key_of(issue)));
        for key in &removed {
            touch(&mut candidate, key);
            cancel_deferred(&mut candidate, key);
        }
        commit(&shared, candidate)?;
        for (domain, issue_id) in &removed {
            dispatch_change(
                &shared.runtime,
                RegistryChange::new("remove", domain.clone(), issue_id.clone()),
            )?;
        }
        Ok(removed.len())
    }

    /// Register one native issue handler for a producer domain.
    pub fn register_platform(
        &self,
        domain: impl Into<String>,
        platform: std::sync::Arc<dyn crate::RepairsPlatform>,
    ) -> RepairsResult<()> {
        self.register_platform_inner(domain, platform).map(|_| ())
    }

    /// Register one native issue handler and unregister this exact registration
    /// when the returned guard is dropped.
    pub fn register_platform_guard(
        &self,
        domain: impl Into<String>,
        platform: std::sync::Arc<dyn crate::RepairsPlatform>,
    ) -> RepairsResult<crate::RepairsPlatformRegistrationGuard> {
        let (shared, domain, registration_id) = self.register_platform_inner(domain, platform)?;
        Ok(crate::RepairsPlatformRegistrationGuard {
            shared: std::sync::Arc::downgrade(&shared),
            domain,
            registration_id,
        })
    }

    fn register_platform_inner(
        &self,
        domain: impl Into<String>,
        platform: std::sync::Arc<dyn crate::RepairsPlatform>,
    ) -> RepairsResult<(std::sync::Arc<crate::Shared>, String, u64)> {
        let shared = self.live()?;
        let domain = domain.into();
        if domain.is_empty() {
            return Err(invalid("platform domain must not be empty"));
        }
        let mut platforms = crate::write_lock(&shared.platforms);
        if platforms.contains_key(&domain) {
            return Err(Error::Service(format!(
                "Repairs platform already registered for {domain}"
            ))
            .into());
        }
        let registration_id = shared
            .next_platform_registration_id
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |next| next.checked_add(1),
            )
            .map_err(|_| Error::Service("Repairs platform registration IDs exhausted".into()))?;
        platforms.insert(
            domain.clone(),
            crate::flows::PlatformRegistration::new(registration_id, platform),
        );
        drop(platforms);
        Ok((shared, domain, registration_id))
    }

    pub fn unregister_platform(&self, domain: &str) -> RepairsResult<bool> {
        let shared = self.live()?;
        Ok(crate::write_lock(&shared.platforms)
            .remove(domain)
            .is_some())
    }
}

fn finish_refresh(manager: &RepairsManager, id: u64, clean: bool) -> RepairsResult<()> {
    let shared = manager.live()?;
    let _gate = lock(&shared.registry_gate);
    let mut candidate = lock(&shared.registry).clone();
    let Some(refresh) = candidate.refreshes.remove(&id) else {
        return Ok(());
    };
    let mut remove = HashSet::new();
    for key in &refresh.selected {
        let claimed_by_newer = candidate
            .refreshes
            .iter()
            .any(|(other_id, other)| *other_id > id && other.selected.contains(key));
        if claimed_by_newer {
            continue;
        }
        if (clean && !refresh.seen.contains(key))
            || (!clean && candidate.deferred_deletes.contains(key))
        {
            remove.insert(key.clone());
        }
    }
    if remove.is_empty() {
        *lock(&shared.registry) = candidate;
        return Ok(());
    }
    for key in &remove {
        touch(&mut candidate, key);
    }
    let actual: Vec<_> = candidate
        .issues
        .iter()
        .filter(|issue| remove.contains(&key_of(issue)))
        .map(key_of)
        .collect();
    if actual.is_empty() {
        *lock(&shared.registry) = candidate;
        return Ok(());
    }
    candidate
        .issues
        .retain(|issue| !remove.contains(&key_of(issue)));
    commit(&shared, candidate)?;
    for (domain, issue_id) in actual {
        dispatch_change(
            &shared.runtime,
            RegistryChange::new("remove", domain, issue_id),
        )?;
    }
    Ok(())
}

fn validate_draft(draft: &IssueDraft) -> RepairsResult<()> {
    if draft.domain.is_empty() || draft.issue_id.is_empty() {
        return Err(invalid("issue domain and issue_id must not be empty"));
    }
    if draft.config_subentry_id.is_some() && draft.config_entry_id.is_none() {
        return Err(invalid("config_subentry_id requires config_entry_id"));
    }
    if let Some(version) = draft
        .breaks_in_ha_version
        .as_deref()
        .filter(|version| !version.is_empty())
        && !is_calver(version)
    {
        return Err(invalid("breaks_in_ha_version must be a CALVER version"));
    }
    if let Some(data) = &draft.data {
        for (key, value) in data {
            if !is_issue_data_value(value) {
                return Err(invalid(format!(
                    "issue data value `{key}` must be a string, number, or null"
                )));
            }
        }
    }
    if let Some(placeholders) = &draft.translation_placeholders {
        for value in placeholders.values() {
            if !value.is_string() {
                return Err(invalid("translation placeholder values must be strings"));
            }
        }
    }
    Ok(())
}

/// Match the pinned AwesomeVersion 25.8.0 CalVer strategy used by Repairs.
/// The source strategy accepts two- or four-digit years, optional sections and
/// modifiers, and a v/V prefix, after trimming surrounding whitespace.
fn is_calver(value: &str) -> bool {
    static CALVER: OnceLock<Regex> = OnceLock::new();
    const CALVER_PATTERN: &str = r"^(\d{2}|\d{4})\.\d{1,2}?(\.?\d{1,2}?\.?)?(\.\d)?(\d*(\w+\d+)?)$";
    let pattern = CALVER.get_or_init(|| {
        Regex::new(CALVER_PATTERN).expect("the pinned AwesomeVersion CalVer pattern is valid")
    });
    let version = value.trim();
    let version = ["v.", "V.", "v", "V"]
        .iter()
        .find_map(|prefix| version.strip_prefix(prefix))
        .unwrap_or(version);
    pattern.is_match(version)
}

fn is_issue_data_value(value: &Value) -> bool {
    matches!(value, Value::Null | Value::String(_) | Value::Number(_))
        && value
            .as_number()
            .is_none_or(|number| number.as_f64().is_some_and(f64::is_finite))
}

fn validate_links(
    shared: &Shared,
    entry_id: &Option<String>,
    subentry_id: &Option<String>,
) -> RepairsResult<()> {
    match (entry_id.as_deref(), subentry_id.as_deref()) {
        (None, Some(_)) => Err(invalid("config_subentry_id requires config_entry_id")),
        (Some(entry_id), _) if shared.entries.get_entry(entry_id).is_none() => {
            Err(RepairsError::UnknownEntry(entry_id.into()))
        }
        (Some(entry_id), Some(subentry_id)) => match shared.subentries.as_deref() {
            Some(resolver) if resolver.subentry_exists(entry_id, subentry_id) => Ok(()),
            Some(_) => Err(RepairsError::UnknownEntry(format!(
                "config subentry {entry_id}/{subentry_id}"
            ))),
            None => Err(RepairsError::Unsupported(
                "config subentry validation is not available".into(),
            )),
        },
        _ => Ok(()),
    }
}

fn entry_from_draft(draft: IssueDraft, existing: Option<&IssueEntry>) -> IssueEntry {
    IssueEntry {
        active: true,
        breaks_in_ha_version: draft.breaks_in_ha_version,
        config_entry_id: draft.config_entry_id,
        config_subentry_id: draft.config_subentry_id,
        created: existing
            .map(|issue| issue.created)
            .unwrap_or_else(|| Utc::now().fixed_offset()),
        data: draft.data,
        dismissed_version: existing.and_then(|issue| issue.dismissed_version.clone()),
        domain: draft.domain,
        is_fixable: Some(draft.is_fixable),
        is_persistent: draft.is_persistent,
        issue_domain: draft.issue_domain,
        issue_id: draft.issue_id,
        learn_more_url: draft.learn_more_url,
        severity: draft.severity,
        translation_key: draft.translation_key,
        translation_placeholders: draft.translation_placeholders,
    }
}

fn commit(shared: &Shared, candidate: RegistryState) -> RepairsResult<()> {
    let value = storage_value(&candidate)?;
    shared.runtime.set_data(STORAGE_KEY, &value)?;
    *lock(&shared.registry) = candidate;
    Ok(())
}

fn touch(state: &mut RegistryState, key: &IssueKey) {
    for refresh in state.refreshes.values_mut() {
        if refresh.selected.contains(key) {
            refresh.seen.insert(key.clone());
        }
    }
    cancel_deferred(state, key);
}

fn keep_in_refresh(shared: &Shared, refresh_id: u64, key: &IssueKey) {
    let _gate = lock(&shared.registry_gate);
    let mut state = lock(&shared.registry);
    let Some(refresh) = state.refreshes.get(&refresh_id) else {
        return;
    };
    if !refresh.selected.contains(key) {
        return;
    }
    let is_newest_owner = state
        .refreshes
        .iter()
        .filter(|(_, refresh)| refresh.selected.contains(key))
        .map(|(id, _)| *id)
        .max()
        == Some(refresh_id);
    for (id, refresh) in &mut state.refreshes {
        if *id <= refresh_id && refresh.selected.contains(key) {
            refresh.seen.insert(key.clone());
        }
    }
    if is_newest_owner {
        state.deferred_deletes.remove(key);
    }
}

fn cancel_deferred(state: &mut RegistryState, key: &IssueKey) {
    state.deferred_deletes.remove(key);
}

fn key_of(issue: &IssueEntry) -> IssueKey {
    (issue.domain.clone(), issue.issue_id.clone())
}

fn string_placeholders(value: &Option<Map<String, Value>>) -> Option<BTreeStringMap> {
    value.as_ref().map(|map| {
        map.iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
            .collect()
    })
}

fn unknown_issue(domain: &str, issue_id: &str) -> RepairsError {
    RepairsError::UnknownIssue {
        domain: domain.into(),
        issue_id: issue_id.into(),
    }
}

fn invalid(message: impl Into<String>) -> RepairsError {
    Error::InvalidInput(message.into()).into()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use hass_config::{ConfigEntries, NewConfigEntry, NewConfigSubentry};
    use hass_core::{MemoryStore, Runtime};
    use serde_json::json;

    use super::*;
    use crate::{ConfigEntryInfo, ConfigEntryResolver, RepairsIntegration, RepairsResult};

    #[derive(Default)]
    struct Entries(Mutex<HashMap<String, ConfigEntryInfo>>);

    impl ConfigEntryResolver for Entries {
        fn get_entry(&self, entry_id: &str) -> Option<ConfigEntryInfo> {
            lock(&self.0).get(entry_id).cloned()
        }
        fn flow_result(&self, _flow_id: &str) -> RepairsResult<Option<Value>> {
            Ok(None)
        }
    }

    fn system() -> (RepairsIntegration, Runtime) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config = hass_config::ConfigEntries::new(runtime.clone()).unwrap();
        (
            RepairsIntegration::start(runtime.clone(), config, "2026.11.6").unwrap(),
            runtime,
        )
    }

    fn draft(domain: &str, issue_id: &str) -> IssueDraft {
        let mut draft = IssueDraft::new(domain, issue_id, true, "warning", "test_issue");
        draft.data = Some(Map::from_iter([(String::from("value"), json!(3))]));
        draft.is_persistent = true;
        draft
    }

    #[test]
    fn migrations_canonicalize_session_shells_and_unknown_fields() {
        let entries = Entries::default();
        let old = json!({"version":1,"minor_version":1,"data":{"issues":[
            {"domain":"a","issue_id":"session","created":"2026-01-01T00:00:00+02:00","future":true,"data":{"secret":"x"}},
            {"domain":"a","issue_id":"persistent","created":"2026-01-01T00:00:00+02:00","is_persistent":true,"is_fixable":true,"severity":"other","translation_key":"key"}
        ]}});
        let (state, needs_save, _) = load_registry(Some(old), &entries, None).unwrap();
        assert!(needs_save);
        let session = state
            .issues
            .iter()
            .find(|issue| issue.issue_id == "session")
            .unwrap();
        assert!(!session.active);
        assert_eq!(session.data, None);
        assert_eq!(session.is_fixable, None);
        assert_eq!(session.created.offset().local_minus_utc(), 7200);
        let persistent = state
            .issues
            .iter()
            .find(|issue| issue.issue_id == "persistent")
            .unwrap();
        assert!(persistent.active);
        assert_eq!(persistent.severity.as_deref(), Some("other"));
        let stored = storage_value(&state).unwrap();
        assert_eq!(stored["minor_version"], 4);
        assert!(stored["data"]["issues"][0].get("future").is_none());
        assert!(stored["data"]["issues"][0].get("active").is_none());
    }

    #[test]
    fn every_supported_minor_version_migrates_and_duplicate_keys_keep_order() {
        let entries = Entries::default();
        for (minor, row) in [
            (
                1,
                json!({"domain":"a","issue_id":"same","created":"2026-01-01T00:00:00Z"}),
            ),
            (
                2,
                json!({"domain":"a","issue_id":"same","created":"2026-01-01T00:00:01Z","is_persistent":false,"config_entry_id":null}),
            ),
            (
                3,
                json!({"domain":"a","issue_id":"same","created":"2026-01-01T00:00:02Z","is_persistent":false,"config_entry_id":null,"config_subentry_id":null}),
            ),
        ] {
            let stored = json!({"version":1,"minor_version":minor,"data":{"issues":[row]}});
            let (state, needs_save, _) = load_registry(Some(stored), &entries, None).unwrap();
            assert!(needs_save);
            assert_eq!(state.issues.len(), 1);
            assert!(!state.issues[0].active);
            assert_eq!(state.issues[0].config_entry_id, None);
            assert_eq!(state.issues[0].config_subentry_id, None);
        }

        let duplicate = json!({"version":1,"minor_version":4,"data":{"issues":[
            {"domain":"a","issue_id":"same","created":"2026-01-01T00:00:00Z","is_persistent":false,"config_entry_id":null,"config_subentry_id":null},
            {"domain":"b","issue_id":"same","created":"2026-01-01T00:00:01Z","is_persistent":false,"config_entry_id":null,"config_subentry_id":null},
            {"domain":"a","issue_id":"same","created":"2026-01-01T00:00:02Z","is_persistent":false,"config_entry_id":null,"config_subentry_id":null}
        ]}});
        let (state, _, _) = load_registry(Some(duplicate), &entries, None).unwrap();
        assert_eq!(
            state
                .issues
                .iter()
                .map(|issue| issue.domain.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(state.issues[0].created.timestamp(), 1_767_225_602);
        assert!(
            load_registry(
                Some(json!({"version":2,"minor_version":0,"data":{"issues":[]}})),
                &entries,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn missing_minor_version_defaults_to_one_and_startup_saves_migration() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .set_data(
                STORAGE_KEY,
                &json!({"version":1,"data":{"issues":[
                    {"domain":"fixture","issue_id":"legacy","created":"2026-01-01T00:00:00Z"}
                ]}}),
            )
            .unwrap();
        let config_entries = ConfigEntries::new(runtime.clone()).unwrap();
        let integration =
            RepairsIntegration::start(runtime.clone(), config_entries, "2026.11.6").unwrap();
        let manager = integration.manager();
        let issue = manager.get_issue("fixture", "legacy").unwrap().unwrap();
        assert!(!issue.active);
        assert_eq!(issue.is_fixable, None);
        let stored = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
        assert_eq!(stored["minor_version"], STORAGE_MINOR);
        assert_eq!(stored["data"]["issues"][0]["is_persistent"], false);
        assert_eq!(stored["data"]["issues"][0]["config_entry_id"], Value::Null);
        assert_eq!(
            stored["data"]["issues"][0]["config_subentry_id"],
            Value::Null
        );
    }

    #[test]
    fn persistence_preserves_order_and_scope_identity() {
        let (integration, runtime) = system();
        let manager = integration.manager();
        manager.create_issue(draft("one", "same")).unwrap();
        manager.create_issue(draft("two", "same")).unwrap();
        manager.create_issue(draft("one", "last")).unwrap();
        let listed = manager.list_issues().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|issue| issue.domain.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "one"]
        );
        let saved = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["minor_version"], 4);
        assert_eq!(saved["data"]["issues"][0]["data"]["value"], 3);
        assert!(saved["data"]["issues"][0].get("active").is_none());
    }

    #[test]
    fn public_create_validates_nonempty_calver_without_revalidating_stored_rows() {
        let (integration, _) = system();
        let manager = integration.manager();
        for (index, version) in [
            "2026.11",
            "20.12.0",
            "2026.11.6b0",
            "V.2026.11.6rc0",
            " 2026.11.6 ",
        ]
        .into_iter()
        .enumerate()
        {
            let mut issue = draft("fixture", &format!("valid_{index}"));
            issue.breaks_in_ha_version = Some(version.into());
            manager.create_issue(issue).unwrap();
        }
        for (index, version) in ["not-a-version", "2026", "2026.11.x"]
            .into_iter()
            .enumerate()
        {
            let mut issue = draft("fixture", &format!("invalid_{index}"));
            issue.breaks_in_ha_version = Some(version.into());
            assert!(manager.create_issue(issue).is_err());
        }
        let mut empty = draft("fixture", "empty_version");
        empty.breaks_in_ha_version = Some(String::new());
        manager.create_issue(empty).unwrap();

        let stored = json!({"version":1,"minor_version":4,"data":{"issues":[
            {"domain":"fixture","issue_id":"legacy_version","created":"2026-01-01T00:00:00Z","is_persistent":true,"is_fixable":true,"breaks_in_ha_version":"not-a-version"}
        ]}});
        let (state, _, _) = load_registry(Some(stored), &Entries::default(), None).unwrap();
        assert_eq!(
            state.issues[0].breaks_in_ha_version.as_deref(),
            Some("not-a-version")
        );
    }

    #[test]
    fn session_issue_restarts_inactive_and_recreation_keeps_identity_and_dismissal() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = hass_config::ConfigEntries::new(runtime.clone()).unwrap();
        let integration = RepairsIntegration::start(runtime, config, "2026.11.6").unwrap();
        let manager = integration.manager();
        let mut session = draft("fixture", "session");
        session.is_persistent = false;
        let active = manager.create_issue(session.clone()).unwrap();
        assert_eq!(active.is_fixable, Some(true));
        assert_eq!(serde_json::to_value(&active).unwrap()["is_fixable"], true);
        assert_eq!(
            serde_json::to_value(manager.list_issues().unwrap()).unwrap()[0]["is_fixable"],
            true
        );
        manager.ignore_issue("fixture", "session", true).unwrap();
        drop(integration);
        let runtime = Runtime::new(store).unwrap();
        let config = hass_config::ConfigEntries::new(runtime.clone()).unwrap();
        let integration = RepairsIntegration::start(runtime, config, "2026.11.6").unwrap();
        let manager = integration.manager();
        let shell = manager.get_issue("fixture", "session").unwrap().unwrap();
        assert!(!shell.active);
        assert_eq!(shell.is_fixable, None);
        assert_eq!(shell.data, None);
        assert_eq!(shell.dismissed_version.as_deref(), Some("2026.11.6"));
        let created = shell.created;
        manager.create_issue(session).unwrap();
        let restored = manager.get_issue("fixture", "session").unwrap().unwrap();
        assert_eq!(restored.created, created);
        assert_eq!(restored.dismissed_version.as_deref(), Some("2026.11.6"));
        assert!(restored.active);
    }

    #[test]
    fn persistence_failure_does_not_publish_or_notify() {
        struct FailingStore {
            inner: hass_core::MemoryStore,
            fail_metadata: AtomicBool,
        }
        impl hass_core::Store for FailingStore {
            fn load_states(&self) -> hass_core::Result<Vec<hass_core::State>> {
                hass_core::Store::load_states(&self.inner)
            }
            fn put_state(
                &self,
                state: &hass_core::State,
                event: Option<&hass_core::Event>,
            ) -> hass_core::Result<()> {
                hass_core::Store::put_state(&self.inner, state, event)
            }
            fn remove_state(
                &self,
                entity_id: &str,
                event: &hass_core::Event,
            ) -> hass_core::Result<()> {
                hass_core::Store::remove_state(&self.inner, entity_id, event)
            }
            fn append_event(&self, event: &hass_core::Event) -> hass_core::Result<()> {
                hass_core::Store::append_event(&self.inner, event)
            }
            fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
                hass_core::Store::get_data(&self.inner, key)
            }
            fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
                if self.fail_metadata.load(Ordering::SeqCst) {
                    Err(Error::Storage("no".into()))
                } else {
                    hass_core::Store::set_data(&self.inner, key, value)
                }
            }
            fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
                hass_core::Store::delete_data(&self.inner, key)
            }
            fn history(
                &self,
                entity_id: &str,
                start: DateTime<Utc>,
                end: DateTime<Utc>,
            ) -> hass_core::Result<Vec<hass_core::State>> {
                hass_core::Store::history(&self.inner, entity_id, start, end)
            }
            fn history_query(
                &self,
                query: &hass_core::HistoryQuery,
            ) -> hass_core::Result<Vec<hass_core::HistorySeries>> {
                hass_core::Store::history_query(&self.inner, query)
            }
            fn event_history_query(
                &self,
                query: &hass_core::EventHistoryQuery,
            ) -> hass_core::Result<Vec<hass_core::Event>> {
                hass_core::Store::event_history_query(&self.inner, query)
            }
            fn purge_before(
                &self,
                cutoff: DateTime<Utc>,
            ) -> hass_core::Result<hass_core::PurgeResult> {
                hass_core::Store::purge_before(&self.inner, cutoff)
            }
            fn repack(&self) -> hass_core::Result<()> {
                hass_core::Store::repack(&self.inner)
            }
        }
        let store = Arc::new(FailingStore {
            inner: hass_core::MemoryStore::default(),
            fail_metadata: AtomicBool::new(false),
        });
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = hass_config::ConfigEntries::new(runtime.clone()).unwrap();
        let integration = RepairsIntegration::start(runtime.clone(), config, "2026.11.6").unwrap();
        let manager = integration.manager();
        manager.create_issue(draft("fixture", "existing")).unwrap();
        store.fail_metadata.store(true, Ordering::SeqCst);
        let mut events = runtime.subscribe();
        let mut private_signals = runtime.subscribe_signals();
        let mut replacement = draft("fixture", "existing");
        replacement.severity = Some("critical".into());
        assert!(manager.create_issue(replacement).is_err());
        assert_eq!(
            manager
                .get_issue("fixture", "existing")
                .unwrap()
                .unwrap()
                .severity
                .as_deref(),
            Some("warning")
        );
        assert!(events.try_recv().is_err());
        assert!(private_signals.try_recv().is_err());
    }

    #[test]
    fn no_op_ignore_and_recreation_do_not_emit_and_changed_update_preserves_created() {
        let (integration, runtime) = system();
        let manager = integration.manager();
        let mut events = runtime.subscribe();
        let mut private_signals = runtime.subscribe_signals();
        let issue = manager.create_issue(draft("fixture", "one")).unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, EVENT_REPAIRS_ISSUE_REGISTRY_UPDATED);
        assert_eq!(event.data.len(), 3);
        assert_eq!(event.data.get("action"), Some(&json!("create")));
        assert_eq!(event.data.get("domain"), Some(&json!("fixture")));
        assert_eq!(event.data.get("issue_id"), Some(&json!("one")));
        manager.create_issue(draft("fixture", "one")).unwrap();
        assert!(events.try_recv().is_err());
        manager.ignore_issue("fixture", "one", true).unwrap();
        assert_eq!(events.try_recv().unwrap().data["action"], "update");
        manager.ignore_issue("fixture", "one", true).unwrap();
        assert!(events.try_recv().is_err());
        let mut replacement = draft("fixture", "one");
        replacement.severity = Some("other".into());
        let updated = manager.create_issue(replacement).unwrap();
        assert_eq!(updated.created, issue.created);
        assert_eq!(updated.dismissed_version.as_deref(), Some("2026.11.6"));
        assert_eq!(events.try_recv().unwrap().data["action"], "update");
        assert!(manager.delete_issue("fixture", "one").unwrap());
        assert_eq!(events.try_recv().unwrap().data["action"], "remove");
        assert!(private_signals.try_recv().is_err());

        let history = runtime
            .event_history_query(&hass_core::EventHistoryQuery {
                start: Utc::now() - chrono::Duration::seconds(5),
                end: Utc::now() + chrono::Duration::seconds(5),
                event_types: Some(vec![EVENT_REPAIRS_ISSUE_REGISTRY_UPDATED.into()]),
            })
            .unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(
            history
                .iter()
                .map(|event| event.data["action"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["create", "update", "update", "remove"]
        );
        assert!(
            history
                .iter()
                .all(|event| event.data.len() == 3 && event.data.get("data").is_none())
        );
    }

    #[test]
    fn refresh_selection_overlap_cancel_and_deferred_delete() {
        let (integration, _) = system();
        let manager = integration.manager();
        for id in ["alpha", "alphabet", "beta", "gamma", "xalpha"] {
            manager.create_issue(draft("fixture", id)).unwrap();
        }
        let mut older = manager
            .refresh_issues(RefreshScope::matching("fixture", "alpha"))
            .unwrap();
        let mut newer = manager
            .refresh_issues(RefreshScope::ids("fixture", ["alpha".into()]))
            .unwrap();
        newer.keep("alpha").unwrap();
        older.finish().unwrap();
        newer.finish().unwrap();
        assert!(manager.get_issue("fixture", "alpha").unwrap().is_some());
        assert!(manager.get_issue("fixture", "alphabet").unwrap().is_none());
        assert!(manager.get_issue("fixture", "xalpha").unwrap().is_some());
        let mut deferred = manager
            .refresh_issues(RefreshScope::ids("fixture", ["beta".into()]))
            .unwrap();
        manager.delete_issue_deferred("fixture", "beta").unwrap();
        deferred.cancel().unwrap();
        assert!(manager.get_issue("fixture", "beta").unwrap().is_none());
        let mut cancelled = manager
            .refresh_issues(RefreshScope::all("fixture"))
            .unwrap();
        cancelled.cancel().unwrap();
        assert!(manager.get_issue("fixture", "gamma").unwrap().is_some());
    }

    #[test]
    fn refresh_keep_respects_its_scope_and_generation() {
        let (integration, _) = system();
        let manager = integration.manager();
        manager.create_issue(draft("fixture", "issue_1")).unwrap();
        manager.create_issue(draft("fixture", "issue_2")).unwrap();

        let mut older = manager
            .refresh_issues(RefreshScope::ids("fixture", ["issue_1".into()]))
            .unwrap();
        let mut newer = manager
            .refresh_issues(RefreshScope::ids("fixture", ["issue_2".into()]))
            .unwrap();
        manager.delete_issue_deferred("fixture", "issue_1").unwrap();
        newer.keep("issue_1").unwrap();
        older.finish().unwrap();
        assert!(manager.get_issue("fixture", "issue_1").unwrap().is_none());
        newer.finish().unwrap();
        assert!(manager.get_issue("fixture", "issue_2").unwrap().is_none());

        manager.create_issue(draft("fixture", "overlap")).unwrap();
        let mut older = manager
            .refresh_issues(RefreshScope::ids("fixture", ["overlap".into()]))
            .unwrap();
        let mut newer = manager
            .refresh_issues(RefreshScope::ids("fixture", ["overlap".into()]))
            .unwrap();
        older.keep("overlap").unwrap();
        manager.delete_issue_deferred("fixture", "overlap").unwrap();
        newer.finish().unwrap();
        assert!(manager.get_issue("fixture", "overlap").unwrap().is_none());
        older.finish().unwrap();
    }

    #[test]
    fn deferred_delete_is_owned_by_the_latest_untouched_refresh() {
        let (integration, _) = system();
        let manager = integration.manager();
        manager.create_issue(draft("fixture", "issue")).unwrap();
        let mut older = manager
            .refresh_issues(RefreshScope::ids("fixture", ["issue".into()]))
            .unwrap();
        let mut newer = manager
            .refresh_issues(RefreshScope::ids("fixture", ["issue".into()]))
            .unwrap();
        manager.delete_issue_deferred("fixture", "issue").unwrap();
        older.finish().unwrap();
        assert!(manager.get_issue("fixture", "issue").unwrap().is_some());
        newer.cancel().unwrap();
        assert!(manager.get_issue("fixture", "issue").unwrap().is_none());

        manager.create_issue(draft("fixture", "kept")).unwrap();
        let mut refreshed = manager
            .refresh_issues(RefreshScope::ids("fixture", ["kept".into()]))
            .unwrap();
        manager.create_issue(draft("fixture", "kept")).unwrap();
        manager.delete_issue_deferred("fixture", "kept").unwrap();
        assert!(manager.get_issue("fixture", "kept").unwrap().is_some());
        refreshed.cancel().unwrap();
        assert!(manager.get_issue("fixture", "kept").unwrap().is_some());

        manager
            .create_issue(draft("fixture", "global_keep"))
            .unwrap();
        let mut older = manager
            .refresh_issues(RefreshScope::ids("fixture", ["global_keep".into()]))
            .unwrap();
        let mut newer = manager
            .refresh_issues(RefreshScope::ids("fixture", ["global_keep".into()]))
            .unwrap();
        manager.keep_issue("fixture", "global_keep").unwrap();
        manager
            .delete_issue_deferred("fixture", "global_keep")
            .unwrap();
        newer.cancel().unwrap();
        older.finish().unwrap();
        assert!(
            manager
                .get_issue("fixture", "global_keep")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn builtin_start_uses_actual_config_subentries_for_validation_and_pruning() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config_entries = ConfigEntries::new(runtime.clone()).unwrap();
        let first_parent = config_entries
            .create_entry(NewConfigEntry::new("fixture", "First", Map::new()))
            .unwrap();
        let second_parent = config_entries
            .create_entry(NewConfigEntry::new("fixture", "Second", Map::new()))
            .unwrap();
        let first_child = config_entries
            .create_subentry(
                &first_parent.entry_id,
                NewConfigSubentry::new("child", "First child", Map::new()),
            )
            .unwrap();
        let second_child = config_entries
            .create_subentry(
                &second_parent.entry_id,
                NewConfigSubentry::new("child", "Second child", Map::new()),
            )
            .unwrap();
        let saved = json!({"version":1,"minor_version":4,"data":{"issues":[
            {"domain":"fixture","issue_id":"valid_link","created":"2026-01-01T00:00:00Z","is_persistent":false,"config_entry_id":first_parent.entry_id,"config_subentry_id":first_child.subentry_id},
            {"domain":"fixture","issue_id":"wrong_parent","created":"2026-01-01T00:00:01Z","is_persistent":false,"config_entry_id":first_parent.entry_id,"config_subentry_id":second_child.subentry_id}
        ]}});
        runtime.set_data(STORAGE_KEY, &saved).unwrap();

        let integration =
            RepairsIntegration::start(runtime, config_entries.clone(), "2026.11.6").unwrap();
        let manager = integration.manager();
        assert!(
            manager
                .get_issue("fixture", "valid_link")
                .unwrap()
                .is_some()
        );
        assert!(
            manager
                .get_issue("fixture", "wrong_parent")
                .unwrap()
                .is_none()
        );

        let mut valid = draft("fixture", "new_valid_link");
        valid.config_entry_id = Some(first_parent.entry_id.clone());
        valid.config_subentry_id = Some(first_child.subentry_id.clone());
        manager.create_issue(valid).unwrap();
        let mut wrong_parent = draft("fixture", "new_wrong_parent");
        wrong_parent.config_entry_id = Some(first_parent.entry_id);
        wrong_parent.config_subentry_id = Some(second_child.subentry_id);
        assert!(matches!(
            manager.create_issue(wrong_parent),
            Err(RepairsError::UnknownEntry(_))
        ));
    }

    #[test]
    fn disabled_linked_issue_is_hidden_and_missing_parent_is_rejected() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = Arc::new(Entries::default());
        lock(&entries.0).insert(
            "entry".into(),
            ConfigEntryInfo {
                entry_id: "entry".into(),
                disabled_by: Some("user".into()),
                public_json: json!({"entry_id":"entry"}),
            },
        );
        let integration =
            RepairsIntegration::start_with_resolvers(runtime.clone(), entries, None, "2026.11.6")
                .unwrap();
        let mut linked = draft("fixture", "linked");
        linked.config_entry_id = Some("entry".into());
        let manager = integration.manager();
        let mut events = runtime.subscribe();
        manager.create_issue(linked).unwrap();
        assert_eq!(
            events.try_recv().unwrap().data.get("action"),
            Some(&json!("create"))
        );
        assert!(integration.manager().list_issues().unwrap().is_empty());
        manager.config_entry_disabled_by_changed("entry").unwrap();
        assert_eq!(
            events.try_recv().unwrap().data.get("action"),
            Some(&json!("update"))
        );
        assert_eq!(manager.clear_config_entry("entry").unwrap(), 1);
        assert_eq!(
            events.try_recv().unwrap().data.get("action"),
            Some(&json!("remove"))
        );
        assert!(manager.get_issue("fixture", "linked").unwrap().is_none());
        let mut missing = draft("fixture", "missing");
        missing.config_entry_id = Some("nope".into());
        assert!(matches!(
            integration.manager().create_issue(missing),
            Err(RepairsError::UnknownEntry(_))
        ));
        let mut subentry = draft("fixture", "subentry");
        subentry.config_entry_id = Some("entry".into());
        subentry.config_subentry_id = Some("sub".into());
        assert!(matches!(
            integration.manager().create_issue(subentry),
            Err(RepairsError::Unsupported(_))
        ));
    }

    #[test]
    fn list_dto_hides_data_and_config_ids_and_invalid_data_is_rejected() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = Arc::new(Entries::default());
        lock(&entries.0).insert(
            "entry".into(),
            ConfigEntryInfo {
                entry_id: "entry".into(),
                disabled_by: None,
                public_json: json!({"entry_id":"entry"}),
            },
        );
        let integration =
            RepairsIntegration::start_with_resolvers(runtime, entries, None, "2026.11.6").unwrap();
        let manager = integration.manager();
        let mut linked = draft("fixture", "private");
        linked.config_entry_id = Some("entry".into());
        linked.data = Some(Map::from_iter([(
            "credential".into(),
            json!("private-data"),
        )]));
        manager.create_issue(linked).unwrap();
        let list = manager.list_issues().unwrap();
        let serialized = serde_json::to_value(&list[0]).unwrap();
        assert!(serialized.get("data").is_none());
        assert!(serialized.get("config_entry_id").is_none());
        assert_eq!(
            manager.get_issue_data("fixture", "private").unwrap()["credential"],
            "private-data"
        );
        let mut invalid = draft("fixture", "invalid");
        invalid.data = Some(Map::from_iter([("nested".into(), json!({"secret":"x"}))]));
        assert!(manager.create_issue(invalid).is_err());
    }
}
