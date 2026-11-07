//! Native Repairs and issue registry System.
//!
//! The strong [`RepairsIntegration`] owns persisted registry and process-local
//! fix-flow state. Its cloneable [`RepairsManager`] is a weak application
//! handle. Registry mutations persist through the shared Runtime metadata API
//! and announce through Runtime's ordinary public event bus.

mod flows;
mod registry;

pub use flows::{
    FixFlowRequest, NextFlow, NextFlowType, RepairsFlow, RepairsFlowContext, RepairsFlowProgress,
    RepairsFlowProgressStep, RepairsFlowStep, RepairsPlatform,
};
pub use registry::{
    EVENT_REPAIRS_ISSUE_REGISTRY_UPDATED, IssueDateTime, IssueDraft, IssueEntry, IssueRefresh,
    IssueSeverity, IssueSummary, RefreshScope, STORAGE_KEY, STORAGE_MAJOR, STORAGE_MINOR,
};

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak, atomic::AtomicU64};

use hass_config::{ConfigEntries, ConfigFlowType};
use hass_core::{Error, Runtime};
use serde_json::Value;

use crate::flows::{FlowSession, PlatformRegistration};
use crate::registry::RegistryState;

/// Result type returned by the native Repairs System.
pub type RepairsResult<T> = std::result::Result<T, RepairsError>;

/// Errors that transport adapters can map to Home Assistant flow/API errors.
#[derive(Debug)]
pub enum RepairsError {
    /// Error returned by a shared core subsystem.
    Core(Error),
    /// No issue exists for the supplied identity.
    UnknownIssue { domain: String, issue_id: String },
    /// The issue exists but cannot be fixed.
    NotFixable { domain: String, issue_id: String },
    /// A flow ID does not identify an active Repairs flow.
    UnknownFlow(String),
    /// A requested flow step or handler cannot be started.
    UnknownStep(String),
    /// A config entry referenced by an issue or next flow is missing.
    UnknownEntry(String),
    /// A required native resolver capability is unavailable.
    Unsupported(String),
}

impl fmt::Display for RepairsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(error) => write!(f, "{error}"),
            Self::UnknownIssue { issue_id, .. } => {
                write!(f, "Issue '{issue_id}' not found")
            }
            Self::NotFixable { issue_id, .. } => {
                write!(f, "Issue id {issue_id} is not fixable")
            }
            Self::UnknownFlow(flow_id) => write!(f, "Unknown flow: {flow_id}"),
            Self::UnknownStep(message) => f.write_str(message),
            Self::UnknownEntry(entry_id) => write!(f, "Config entry {entry_id} not found"),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RepairsError {}

impl From<Error> for RepairsError {
    fn from(value: Error) -> Self {
        Self::Core(value)
    }
}

/// The minimum config-entry information Repairs needs; private data/options
/// never cross this resolver boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigEntryInfo {
    pub entry_id: String,
    pub disabled_by: Option<String>,
    pub public_json: Value,
}

/// Access to initialized native config-entry state and live config flows.
///
/// The root adapter may implement this trait directly. The supplied
/// `ConfigEntries` implementation reads the actual manager and never fabricates
/// an entry or subentry ID.
pub trait ConfigEntryResolver: Send + Sync {
    fn get_entry(&self, entry_id: &str) -> Option<ConfigEntryInfo>;

    /// Return a flow result without asserting which manager owns it.
    ///
    /// This is retained for callers that need an untyped lookup. Repairs
    /// `next_flow` validation must use [`Self::active_flow_result`] instead.
    fn flow_result(&self, flow_id: &str) -> RepairsResult<Option<Value>>;

    /// Return a currently active result only when `flow_id` belongs to the
    /// manager named by `flow_type`.
    fn active_flow_result(
        &self,
        _flow_type: NextFlowType,
        _flow_id: &str,
    ) -> RepairsResult<Option<Value>> {
        Err(RepairsError::Unsupported(
            "typed native config flow lookup is unavailable".into(),
        ))
    }

    /// Derive the related config-entry ID from the active child's source
    /// metadata. A new config flow has no linked entry and returns `None`.
    fn next_flow_entry_id(
        &self,
        flow_type: NextFlowType,
        flow_result: &Value,
    ) -> RepairsResult<Option<String>> {
        let entry_id = match flow_type {
            NextFlowType::ConfigFlow => {
                let context = flow_result
                    .get("context")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid_flow_metadata("config flow context is missing"))?;
                match context.get("entry_id") {
                    Some(value) => optional_flow_entry_id(value)?.map(str::to_owned),
                    None => None,
                }
            }
            NextFlowType::OptionsFlow => Some(required_flow_entry_id(
                flow_result
                    .get("handler")
                    .ok_or_else(|| invalid_flow_metadata("options flow handler is missing"))?,
            )?),
            NextFlowType::ConfigSubentriesFlow => {
                let handler = flow_result
                    .get("handler")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_flow_metadata("subentry flow handler is missing"))?;
                if handler.len() != 2 {
                    return Err(invalid_flow_metadata(
                        "subentry flow handler must identify an entry and subentry type",
                    ));
                }
                if !handler[1].is_string() {
                    return Err(invalid_flow_metadata(
                        "subentry flow handler type must be a string",
                    ));
                }
                Some(required_flow_entry_id(&handler[0])?)
            }
        };
        Ok(entry_id)
    }
}

fn invalid_flow_metadata(message: &str) -> RepairsError {
    Error::InvalidInput(message.into()).into()
}

fn optional_flow_entry_id(value: &Value) -> RepairsResult<Option<&str>> {
    match value {
        Value::Null => Ok(None),
        Value::String(entry_id) => Ok(Some(entry_id)),
        _ => Err(invalid_flow_metadata(
            "flow entry ID must be a string or null",
        )),
    }
}

fn required_flow_entry_id(value: &Value) -> RepairsResult<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_flow_metadata("flow handler must identify a config entry"))
}

/// Optional capability for real config-subentry managers.
pub trait ConfigSubentryResolver: Send + Sync {
    fn subentry_exists(&self, entry_id: &str, subentry_id: &str) -> bool;
}

impl ConfigEntryResolver for ConfigEntries {
    fn get_entry(&self, entry_id: &str) -> Option<ConfigEntryInfo> {
        let entry = ConfigEntries::get_entry(self, entry_id)?;
        Some(ConfigEntryInfo {
            entry_id: entry.entry_id.clone(),
            disabled_by: entry.disabled_by.clone(),
            public_json: entry.public_json(),
        })
    }

    fn flow_result(&self, flow_id: &str) -> RepairsResult<Option<Value>> {
        match ConfigEntries::flow_result(self, flow_id) {
            Ok(result) => Ok(Some(result)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn active_flow_result(
        &self,
        flow_type: NextFlowType,
        flow_id: &str,
    ) -> RepairsResult<Option<Value>> {
        let flow_type = match flow_type {
            NextFlowType::ConfigFlow => ConfigFlowType::Config,
            NextFlowType::OptionsFlow => ConfigFlowType::Options,
            NextFlowType::ConfigSubentriesFlow => ConfigFlowType::ConfigSubentries,
        };
        match ConfigEntries::active_flow(self, flow_type, flow_id) {
            Ok(flow) => Ok(Some(flow.result)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl ConfigSubentryResolver for ConfigEntries {
    fn subentry_exists(&self, entry_id: &str, subentry_id: &str) -> bool {
        ConfigEntries::subentry_exists(self, entry_id, subentry_id)
    }
}

/// Strong process owner for the Repairs System.
pub struct RepairsIntegration {
    shared: Arc<Shared>,
}

/// Cloneable, non-owning handle for integrations and API adapters.
#[derive(Clone)]
pub struct RepairsManager {
    shared: Weak<Shared>,
}

/// Owns one Repairs platform registration and unregisters it when dropped.
///
/// The guard holds only a weak reference to the Repairs integration, so keeping
/// it alive does not keep the integration or its Runtime alive.
#[must_use = "dropping the registration guard unregisters the Repairs platform"]
pub struct RepairsPlatformRegistrationGuard {
    shared: Weak<Shared>,
    domain: String,
    registration_id: u64,
}

pub(crate) struct Shared {
    pub(crate) runtime: Runtime,
    pub(crate) entries: Arc<dyn ConfigEntryResolver>,
    pub(crate) subentries: Option<Arc<dyn ConfigSubentryResolver>>,
    pub(crate) current_version: String,
    pub(crate) registry_gate: Mutex<()>,
    pub(crate) registry: Mutex<RegistryState>,
    pub(crate) platforms: RwLock<std::collections::HashMap<String, PlatformRegistration>>,
    pub(crate) next_platform_registration_id: AtomicU64,
    pub(crate) flows: Mutex<std::collections::HashMap<String, Arc<Mutex<FlowSession>>>>,
}

impl RepairsIntegration {
    /// Load an issue registry using the application's initialized config entries.
    pub fn start(
        runtime: Runtime,
        config_entries: ConfigEntries,
        current_version: impl Into<String>,
    ) -> RepairsResult<Self> {
        let entries = Arc::new(config_entries.clone());
        let subentries = Arc::new(config_entries);
        Self::start_with_resolvers(runtime, entries, Some(subentries), current_version)
    }

    /// Start with explicit parent/subentry resolvers for native hosts that own
    /// those capabilities outside `hass-config`.
    pub fn start_with_resolvers(
        runtime: Runtime,
        entries: Arc<dyn ConfigEntryResolver>,
        subentries: Option<Arc<dyn ConfigSubentryResolver>>,
        current_version: impl Into<String>,
    ) -> RepairsResult<Self> {
        let current_version = current_version.into();
        if current_version.is_empty() {
            return Err(Error::InvalidInput("current version must not be empty".into()).into());
        }
        let stored = runtime.get_data(STORAGE_KEY)?;
        let (registry, needs_save, startup_changes) =
            registry::load_registry(stored, entries.as_ref(), subentries.as_deref())?;
        if needs_save {
            runtime.set_data(STORAGE_KEY, &registry::storage_value(&registry)?)?;
        }
        for change in startup_changes {
            registry::dispatch_change(&runtime, change)?;
        }
        Ok(Self {
            shared: Arc::new(Shared {
                runtime,
                entries,
                subentries,
                current_version,
                registry_gate: Mutex::new(()),
                registry: Mutex::new(registry),
                platforms: RwLock::new(std::collections::HashMap::new()),
                next_platform_registration_id: AtomicU64::new(1),
                flows: Mutex::new(std::collections::HashMap::new()),
            }),
        })
    }

    /// Return the weak API/integration handle.
    pub fn manager(&self) -> RepairsManager {
        RepairsManager {
            shared: Arc::downgrade(&self.shared),
        }
    }
}

impl RepairsManager {
    pub(crate) fn live(&self) -> RepairsResult<Arc<Shared>> {
        self.shared
            .upgrade()
            .ok_or_else(|| Error::Service("repairs integration is closed".into()).into())
    }
}

impl Drop for RepairsIntegration {
    fn drop(&mut self) {
        // Weak handles naturally fail after this owner releases its Arc. Flow
        // sessions are process-local and are dropped with the same owner.
    }
}

impl Drop for RepairsPlatformRegistrationGuard {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut platforms = write_lock(&shared.platforms);
        let owns_current_registration = platforms
            .get(&self.domain)
            .is_some_and(|registration| registration.id == self.registration_id);
        if owns_current_registration {
            platforms.remove(&self.domain);
        }
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
