use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{RepairsError, RepairsManager, RepairsResult, Shared, lock, read_lock};

/// Input delivered to a registered native Repairs handler. `issue_data` comes
/// from the stored issue and is never synthesized from HTTP input.
#[derive(Clone, Debug, PartialEq)]
pub struct FixFlowRequest {
    pub handler: String,
    pub issue_id: String,
    pub issue_data: Option<Map<String, Value>>,
}

/// Context shared across all steps of one Repairs fix flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairsFlowContext {
    pub handler: String,
    pub issue_id: String,
    /// Caller context retained across steps, including the authoritative ID.
    pub metadata: Map<String, Value>,
    /// Initial input, with context issue_id taking precedence over legacy data.
    /// Native callbacks receive this separately from later submitted input.
    pub initial_data: Map<String, Value>,
}

/// A native flow callback returns one typed data-entry-flow result.
pub trait RepairsFlow: Send {
    fn step(
        &mut self,
        context: &RepairsFlowContext,
        user_input: Option<Value>,
    ) -> RepairsResult<RepairsFlowStep>;
}

/// Native platform that can construct a flow for a fixable issue.
pub trait RepairsPlatform: Send + Sync {
    fn create_fix_flow(&self, request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>>;
}

/// A progress state returned by a native fix handler.
#[derive(Clone, Debug, PartialEq)]
pub enum RepairsFlowStep {
    Form {
        step_id: String,
        data_schema: Option<Value>,
        errors: Option<Map<String, Value>>,
        description_placeholders: Option<Map<String, Value>>,
    },
    CreateEntry {
        title: Option<String>,
        data: Map<String, Value>,
        description: Option<String>,
        description_placeholders: Option<Map<String, Value>>,
        next_flow: Option<NextFlow>,
    },
    Abort {
        reason: String,
        description_placeholders: Option<Map<String, Value>>,
        translation_domain: Option<String>,
        next_flow: Option<NextFlow>,
    },
}

/// Source metadata for a child flow owned by hass-config.
///
/// A Repairs flow returns this as metadata on its terminal create/abort result;
/// the child flow continues independently through its own flow manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextFlow {
    pub flow_type: NextFlowType,
    pub flow_id: String,
}

impl NextFlow {
    pub fn new(flow_type: NextFlowType, flow_id: impl Into<String>) -> Self {
        Self {
            flow_type,
            flow_id: flow_id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextFlowType {
    ConfigFlow,
    OptionsFlow,
    ConfigSubentriesFlow,
}

/// HTTP-ready, typed progress data. The protocol adapter should serialize this
/// DTO and apply the route's authorization policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepairsFlowProgress {
    pub flow_id: String,
    pub handler: String,
    pub context: Map<String, Value>,
    #[serde(flatten)]
    pub step: RepairsFlowProgressStep,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepairsFlowProgressStep {
    Form {
        step_id: String,
        data_schema: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        errors: Option<Map<String, Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description_placeholders: Option<Map<String, Value>>,
    },
    CreateEntry {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        data: Map<String, Value>,
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description_placeholders: Option<Map<String, Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_flow: Option<(NextFlowType, String)>,
    },
    Abort {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description_placeholders: Option<Map<String, Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        translation_domain: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_flow: Option<(NextFlowType, String)>,
    },
}

pub(crate) struct PlatformRegistration {
    pub(crate) id: u64,
    pub(crate) platform: Arc<dyn RepairsPlatform>,
}

impl PlatformRegistration {
    pub(crate) fn new(id: u64, platform: Arc<dyn RepairsPlatform>) -> Self {
        Self { id, platform }
    }
}

pub(crate) struct FlowSession {
    pub(crate) handler: String,
    pub(crate) issue_id: String,
    pub(crate) flow: Option<Box<dyn RepairsFlow>>,
    pub(crate) step: RepairsFlowStep,
    pub(crate) terminal: bool,
    pub(crate) processing: bool,
    pub(crate) context: RepairsFlowContext,
}

struct ConfirmationFlow {
    description_placeholders: Option<Map<String, Value>>,
}

impl RepairsFlow for ConfirmationFlow {
    fn step(
        &mut self,
        _context: &RepairsFlowContext,
        user_input: Option<Value>,
    ) -> RepairsResult<RepairsFlowStep> {
        if user_input.is_none() {
            return Ok(RepairsFlowStep::Form {
                step_id: "confirm".into(),
                data_schema: Some(serde_json::json!([])),
                errors: None,
                description_placeholders: self.description_placeholders.clone(),
            });
        }
        Ok(RepairsFlowStep::CreateEntry {
            title: None,
            data: Map::new(),
            description: None,
            description_placeholders: None,
            next_flow: None,
        })
    }
}

impl RepairsManager {
    /// Start the registered handler flow, or the upstream confirmation-only
    /// fallback when no `{handler}.repairs` platform is installed.
    pub fn start_fix_flow(
        &self,
        handler: &str,
        issue_id: &str,
    ) -> RepairsResult<RepairsFlowProgress> {
        self.init_fix_flow(
            handler,
            Some(Map::from_iter([(
                "issue_id".into(),
                Value::String(issue_id.into()),
            )])),
            None,
        )
    }

    /// Initialize with source context/data precedence. The data fallback is
    /// accepted when context lacks issue_id, including an otherwise nonempty
    /// context. Producer issue data remains separate from this initial input.
    pub fn init_fix_flow(
        &self,
        handler: &str,
        context: Option<Map<String, Value>>,
        data: Option<Map<String, Value>>,
    ) -> RepairsResult<RepairsFlowProgress> {
        let mut metadata = context.unwrap_or_default();
        if !metadata.contains_key("issue_id")
            && let Some(issue_id) = data.as_ref().and_then(|data| data.get("issue_id"))
        {
            metadata.insert("issue_id".into(), issue_id.clone());
            tracing::warn!(
                integration = handler,
                "repair flow issue_id in data is deprecated; use context before 2027.12.0"
            );
        }
        let issue_id = metadata
            .get("issue_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RepairsError::UnknownStep("issue_id was not set in context".into()))?
            .to_owned();
        let mut initial_data = data.unwrap_or_default();
        initial_data.insert("issue_id".into(), Value::String(issue_id.clone()));
        self.start_fix_flow_context(RepairsFlowContext {
            handler: handler.into(),
            issue_id,
            metadata,
            initial_data,
        })
    }

    fn start_fix_flow_context(
        &self,
        context: RepairsFlowContext,
    ) -> RepairsResult<RepairsFlowProgress> {
        let handler = context.handler.as_str();
        let issue_id = context.issue_id.as_str();
        let shared = self.live()?;
        let issue =
            self.get_issue(handler, issue_id)?
                .ok_or_else(|| RepairsError::UnknownIssue {
                    domain: handler.into(),
                    issue_id: issue_id.into(),
                })?;
        if issue.is_fixable != Some(true) {
            return Err(RepairsError::NotFixable {
                domain: handler.into(),
                issue_id: issue_id.into(),
            });
        }
        let registration = read_lock(&shared.platforms)
            .get(handler)
            .map(|registration| registration.platform.clone());
        let mut flow = if let Some(platform) = registration {
            // Platform callbacks run outside both the registry and platform
            // registration locks.
            platform.create_fix_flow(FixFlowRequest {
                handler: handler.into(),
                issue_id: issue_id.into(),
                issue_data: issue.data,
            })?
        } else {
            Box::new(ConfirmationFlow {
                description_placeholders: issue.translation_placeholders,
            }) as Box<dyn RepairsFlow>
        };
        let initial = flow.step(&context, None)?;
        let flow_id = Uuid::new_v4().to_string();
        let progress = progress_value(&shared, &flow_id, &context, &initial)?;
        let terminal = initial.is_terminal();
        if initial.is_success_terminal() {
            self.delete_issue(handler, issue_id)?;
        }
        if !terminal {
            lock(&shared.flows).insert(
                flow_id.clone(),
                Arc::new(Mutex::new(FlowSession {
                    handler: handler.into(),
                    issue_id: issue_id.into(),
                    flow: Some(flow),
                    step: initial,
                    terminal: false,
                    processing: false,
                    context,
                })),
            );
        }
        Ok(progress)
    }

    /// Return an active Repairs flow state.
    pub fn flow_result(&self, flow_id: &str) -> RepairsResult<RepairsFlowProgress> {
        let shared = self.live()?;
        let session = flow_session(&shared, flow_id)?;
        let (context, step) = {
            let session = lock(&session);
            if session.terminal {
                return Err(RepairsError::UnknownFlow(flow_id.into()));
            }
            (session.context.clone(), session.step.clone())
        };
        progress_value(&shared, flow_id, &context, &step)
    }

    /// Submit a form step to an active Repairs flow.
    pub fn submit_step(
        &self,
        flow_id: &str,
        user_input: Value,
    ) -> RepairsResult<RepairsFlowProgress> {
        let shared = self.live()?;
        let session_arc = flow_session(&shared, flow_id)?;
        let (handler, issue_id, step, context, mut flow) = {
            let mut session = lock(&session_arc);
            if session.terminal {
                return Err(RepairsError::UnknownFlow(flow_id.into()));
            }
            if session.processing {
                return Err(RepairsError::UnknownStep(
                    "flow step is already being processed".into(),
                ));
            }
            let step = session.step.clone();
            session.processing = true;
            let flow = session.flow.take();
            (
                session.handler.clone(),
                session.issue_id.clone(),
                step,
                session.context.clone(),
                flow,
            )
        };
        let next_step_result = match step {
            RepairsFlowStep::Form { .. } => match flow.as_mut() {
                Some(flow) => flow.step(&context, Some(user_input)),
                None => Err(RepairsError::UnknownStep(
                    "flow handler is unavailable".into(),
                )),
            },
            RepairsFlowStep::CreateEntry { .. } | RepairsFlowStep::Abort { .. } => {
                Err(RepairsError::UnknownFlow(flow_id.into()))
            }
        };
        let next_step = match next_step_result {
            Ok(next_step) => next_step,
            Err(error) => {
                restore_processing_flow(&session_arc, flow);
                return Err(error);
            }
        };
        let progress = match progress_value(&shared, flow_id, &context, &next_step) {
            Ok(progress) => progress,
            Err(error) => {
                restore_processing_flow(&session_arc, flow);
                return Err(error);
            }
        };
        let terminal = next_step.is_terminal();
        if next_step.is_success_terminal()
            && let Err(error) = self.delete_issue(&handler, &issue_id)
        {
            restore_processing_flow(&session_arc, flow);
            return Err(error);
        }
        {
            let mut session = lock(&session_arc);
            session.step = next_step.clone();
            session.terminal = terminal;
            session.processing = false;
            if terminal {
                session.flow = None;
            } else if flow.is_some() {
                session.flow = flow;
            }
        }
        if terminal {
            lock(&shared.flows).remove(flow_id);
        }
        Ok(progress)
    }

    /// Abort an active Repairs session. The source issue and any independent
    /// child config flow are retained.
    pub fn abort_flow(&self, flow_id: &str) -> RepairsResult<()> {
        let shared = self.live()?;
        let session_arc = flow_session(&shared, flow_id)?;
        {
            let mut session = lock(&session_arc);
            if session.terminal {
                return Err(RepairsError::UnknownFlow(flow_id.into()));
            }
            if session.processing {
                return Err(RepairsError::UnknownStep(
                    "flow step is already being processed".into(),
                ));
            }
            session.processing = true;
        }
        {
            let mut session = lock(&session_arc);
            session.flow = None;
            session.terminal = true;
            session.processing = false;
        }
        lock(&shared.flows).remove(flow_id);
        Ok(())
    }
}

impl RepairsFlowStep {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::CreateEntry { .. } | Self::Abort { .. })
    }

    fn is_success_terminal(&self) -> bool {
        matches!(self, Self::CreateEntry { .. })
    }
}

fn restore_processing_flow(
    session_arc: &Arc<Mutex<FlowSession>>,
    flow: Option<Box<dyn RepairsFlow>>,
) {
    let mut session = lock(session_arc);
    if flow.is_some() {
        session.flow = flow;
    }
    session.processing = false;
}

impl RepairsFlowProgress {
    pub fn to_value(&self) -> RepairsResult<Value> {
        serde_json::to_value(self)
            .map_err(|error| hass_core::Error::InvalidInput(error.to_string()).into())
    }
}

fn flow_session(shared: &Shared, flow_id: &str) -> RepairsResult<Arc<Mutex<FlowSession>>> {
    lock(&shared.flows)
        .get(flow_id)
        .cloned()
        .ok_or_else(|| RepairsError::UnknownFlow(flow_id.into()))
}

fn linked_entry_result(shared: &Shared, next: Option<&NextFlow>) -> RepairsResult<Option<Value>> {
    let Some(next) = next else {
        return Ok(None);
    };
    if next.flow_id.is_empty() {
        return Err(RepairsError::UnknownFlow(next.flow_id.clone()));
    }
    let flow_result = shared
        .entries
        .active_flow_result(next.flow_type, &next.flow_id)?
        .ok_or_else(|| RepairsError::UnknownFlow(next.flow_id.clone()))?;
    let entry_id = shared
        .entries
        .next_flow_entry_id(next.flow_type, &flow_result)?;
    let linked_entry = match entry_id {
        Some(entry_id) => Some(
            shared
                .entries
                .get_entry(&entry_id)
                .ok_or_else(|| RepairsError::UnknownEntry(entry_id.clone()))?
                .public_json,
        ),
        None => Some(Value::Null),
    };
    Ok(linked_entry)
}

fn progress_value(
    shared: &Shared,
    flow_id: &str,
    context: &RepairsFlowContext,
    step: &RepairsFlowStep,
) -> RepairsResult<RepairsFlowProgress> {
    let step = match step {
        RepairsFlowStep::Form {
            step_id,
            data_schema,
            errors,
            description_placeholders,
        } => RepairsFlowProgressStep::Form {
            step_id: step_id.clone(),
            data_schema: data_schema.clone(),
            errors: errors.clone(),
            description_placeholders: description_placeholders.clone(),
        },
        RepairsFlowStep::CreateEntry {
            title,
            data,
            description,
            description_placeholders,
            next_flow,
        } => RepairsFlowProgressStep::CreateEntry {
            title: title.clone(),
            data: data.clone(),
            description: description.clone(),
            description_placeholders: description_placeholders.clone(),
            result: linked_entry_result(shared, next_flow.as_ref())?,
            next_flow: next_flow
                .as_ref()
                .map(|next| (next.flow_type, next.flow_id.clone())),
        },
        RepairsFlowStep::Abort {
            reason,
            description_placeholders,
            translation_domain,
            next_flow,
        } => RepairsFlowProgressStep::Abort {
            reason: reason.clone(),
            description_placeholders: description_placeholders.clone(),
            translation_domain: translation_domain.clone(),
            result: linked_entry_result(shared, next_flow.as_ref())?,
            next_flow: next_flow
                .as_ref()
                .map(|next| (next.flow_type, next.flow_id.clone())),
        },
    };
    Ok(RepairsFlowProgress {
        flow_id: flow_id.into(),
        handler: context.handler.clone(),
        context: context.metadata.clone(),
        step,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use hass_config::{
        ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
        ConfigSubentryFlowRequest, ConfigSubentryFlowResult, ConfigSubentryTypeInfo, SetupOutcome,
    };
    use hass_core::{MemoryStore, Runtime};
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::{IssueDraft, RepairsIntegration};

    struct FixturePlatform {
        calls: Arc<AtomicUsize>,
    }
    impl RepairsPlatform for FixturePlatform {
        fn create_fix_flow(&self, request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
            assert_eq!(request.handler, "fixture");
            assert_eq!(request.issue_id, "repair");
            assert_eq!(
                request.issue_data.as_ref().unwrap()["secret_hint"],
                "safe-note"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FixtureFlow { calls: 0 }))
        }
    }

    struct FixtureFlow {
        calls: usize,
    }
    impl RepairsFlow for FixtureFlow {
        fn step(
            &mut self,
            context: &RepairsFlowContext,
            user_input: Option<Value>,
        ) -> RepairsResult<RepairsFlowStep> {
            assert_eq!(context.issue_id, "repair");
            self.calls += 1;
            if user_input.is_none() {
                assert_eq!(context.initial_data["issue_id"], "repair");
                assert_eq!(context.initial_data["extra"], "caller-data");
                assert_eq!(context.metadata["caller"], "caller-context");
                return Ok(RepairsFlowStep::Form {
                    step_id: "confirm".into(),
                    data_schema: Some(json!([{"name":"confirm","type":"boolean"}])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            if user_input
                .as_ref()
                .and_then(|input| input.get("confirm"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                Ok(RepairsFlowStep::CreateEntry {
                    title: Some("Fixed".into()),
                    data: Map::from_iter([("ok".into(), json!(true))]),
                    description: None,
                    description_placeholders: None,
                    next_flow: None,
                })
            } else {
                Ok(RepairsFlowStep::Abort {
                    reason: "user".into(),
                    description_placeholders: None,
                    translation_domain: None,
                    next_flow: None,
                })
            }
        }
    }

    fn setup() -> (RepairsIntegration, crate::RepairsManager) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config = ConfigEntries::new(runtime.clone()).unwrap();
        let integration = RepairsIntegration::start(runtime, config, "2026.12.0.dev0").unwrap();
        let manager = integration.manager();
        let mut draft = IssueDraft::new("fixture", "repair", true, "error", "repair_me");
        draft.is_persistent = true;
        draft.data = Some(Map::from_iter([("secret_hint".into(), json!("safe-note"))]));
        draft.translation_placeholders =
            Some(Map::from_iter([("label".into(), json!("safe-note"))]));
        manager.create_issue(draft).unwrap();
        (integration, manager)
    }

    struct NativeConfigDriver;

    impl ConfigEntryDriver for NativeConfigDriver {
        fn domain(&self) -> &str {
            "native_fixture"
        }

        fn supports_options(&self) -> bool {
            true
        }

        fn setup(
            &self,
            _runtime: &Runtime,
            _entry: &ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
            Ok(())
        }

        fn config_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            let Some(input) = request.user_input.as_ref() else {
                return Ok(ConfigFlowResult::Form {
                    step_id: request.step_id.clone(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            };
            if request.source == "reauth" {
                return Ok(ConfigFlowResult::Form {
                    step_id: "reauth".into(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            let marker = input
                .get("marker")
                .and_then(Value::as_str)
                .unwrap_or("default");
            Ok(ConfigFlowResult::CreateEntry {
                title: format!("Native {marker}"),
                data: Map::from_iter([("private_config".into(), json!("secret-config"))]),
                options: Map::from_iter([("private_option".into(), json!("secret-option"))]),
                unique_id: Some(marker.into()),
                version: 1,
                minor_version: 1,
            })
        }

        fn options_flow(&self, request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            if request.user_input.is_none() {
                return Ok(ConfigFlowResult::Form {
                    step_id: "init".into(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            Ok(ConfigFlowResult::CreateEntry {
                title: "Native options".into(),
                data: Map::from_iter([("private_option".into(), json!("updated-secret"))]),
                options: Map::new(),
                unique_id: None,
                version: 1,
                minor_version: 1,
            })
        }

        fn supported_subentry_types(
            &self,
            _entry: &ConfigEntry,
        ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
            BTreeMap::from([("child".into(), ConfigSubentryTypeInfo::default())])
        }

        fn subentry_flow(
            &self,
            request: &ConfigSubentryFlowRequest,
        ) -> hass_core::Result<ConfigSubentryFlowResult> {
            if request.user_input.is_none() {
                return Ok(ConfigSubentryFlowResult::Form {
                    step_id: "user".into(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            Ok(ConfigSubentryFlowResult::CreateEntry {
                title: "Native child".into(),
                data: Map::from_iter([("private_child".into(), json!("secret-child"))]),
                unique_id: None,
            })
        }
    }

    fn native_config() -> (Runtime, ConfigEntries) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let config = ConfigEntries::new(runtime.clone()).unwrap();
        config
            .register_driver(Arc::new(NativeConfigDriver))
            .unwrap();
        (runtime, config)
    }

    fn native_entry(config: &ConfigEntries, marker: &str) -> String {
        let started = config
            .init_config_flow("native_fixture", "user", Map::new())
            .unwrap();
        let flow_id = started["flow_id"].as_str().unwrap();
        let result = config
            .submit_flow(flow_id, Map::from_iter([("marker".into(), json!(marker))]))
            .unwrap();
        result["result"]["entry_id"].as_str().unwrap().to_owned()
    }

    #[derive(Clone, Copy)]
    enum TerminalOutcome {
        CreateEntry,
        Abort,
    }

    struct TerminalLinkPlatform {
        next_flow: NextFlow,
        outcome: TerminalOutcome,
    }

    impl RepairsPlatform for TerminalLinkPlatform {
        fn create_fix_flow(&self, _request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
            Ok(Box::new(TerminalLinkFlow {
                next_flow: self.next_flow.clone(),
                outcome: self.outcome,
            }))
        }
    }

    struct TerminalLinkFlow {
        next_flow: NextFlow,
        outcome: TerminalOutcome,
    }

    impl RepairsFlow for TerminalLinkFlow {
        fn step(
            &mut self,
            _context: &RepairsFlowContext,
            user_input: Option<Value>,
        ) -> RepairsResult<RepairsFlowStep> {
            if user_input.is_none() {
                return Ok(RepairsFlowStep::Form {
                    step_id: "confirm".into(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            Ok(match self.outcome {
                TerminalOutcome::CreateEntry => RepairsFlowStep::CreateEntry {
                    title: Some("Fixed".into()),
                    data: Map::from_iter([("fixed".into(), json!(true))]),
                    description: None,
                    description_placeholders: None,
                    next_flow: Some(self.next_flow.clone()),
                },
                TerminalOutcome::Abort => RepairsFlowStep::Abort {
                    reason: "left_unfixed".into(),
                    description_placeholders: None,
                    translation_domain: None,
                    next_flow: Some(self.next_flow.clone()),
                },
            })
        }
    }

    fn setup_native_repairs(
        runtime: Runtime,
        config: ConfigEntries,
        issue_id: &str,
        next_flow: NextFlow,
        outcome: TerminalOutcome,
    ) -> (RepairsIntegration, crate::RepairsManager) {
        let integration = RepairsIntegration::start(runtime, config, "2026.12.0.dev0").unwrap();
        let manager = integration.manager();
        let mut draft = IssueDraft::new("repairs_fixture", issue_id, true, "error", "repair_me");
        draft.is_persistent = true;
        manager.create_issue(draft).unwrap();
        manager
            .register_platform(
                "repairs_fixture",
                Arc::new(TerminalLinkPlatform { next_flow, outcome }),
            )
            .unwrap();
        (integration, manager)
    }

    struct ImmediatePlatform(RepairsFlowStep);

    impl RepairsPlatform for ImmediatePlatform {
        fn create_fix_flow(&self, _request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
            Ok(Box::new(ImmediateFlow(self.0.clone())))
        }
    }

    struct ImmediateFlow(RepairsFlowStep);

    impl RepairsFlow for ImmediateFlow {
        fn step(
            &mut self,
            _context: &RepairsFlowContext,
            _user_input: Option<Value>,
        ) -> RepairsResult<RepairsFlowStep> {
            Ok(self.0.clone())
        }
    }

    struct GatedPlatform {
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl RepairsPlatform for GatedPlatform {
        fn create_fix_flow(&self, _request: FixFlowRequest) -> RepairsResult<Box<dyn RepairsFlow>> {
            Ok(Box::new(GatedFlow {
                entered: self.entered.lock().unwrap().take().unwrap(),
                release: self.release.lock().unwrap().take().unwrap(),
            }))
        }
    }

    struct GatedFlow {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl RepairsFlow for GatedFlow {
        fn step(
            &mut self,
            _context: &RepairsFlowContext,
            user_input: Option<Value>,
        ) -> RepairsResult<RepairsFlowStep> {
            if user_input.is_none() {
                return Ok(RepairsFlowStep::Form {
                    step_id: "blocking".into(),
                    data_schema: Some(json!([])),
                    errors: None,
                    description_placeholders: None,
                });
            }
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(RepairsFlowStep::CreateEntry {
                title: None,
                data: Map::new(),
                description: None,
                description_placeholders: None,
                next_flow: None,
            })
        }
    }

    #[test]
    fn registered_fix_flow_receives_issue_data_and_success_removes_issue() {
        let (_integration, manager) = setup();
        let calls = Arc::new(AtomicUsize::new(0));
        manager
            .register_platform(
                "fixture",
                Arc::new(FixturePlatform {
                    calls: calls.clone(),
                }),
            )
            .unwrap();
        let initial = manager
            .init_fix_flow(
                "fixture",
                Some(Map::from_iter([
                    ("issue_id".into(), json!("repair")),
                    ("caller".into(), json!("caller-context")),
                ])),
                Some(Map::from_iter([
                    ("issue_id".into(), json!("legacy-issue")),
                    ("extra".into(), json!("caller-data")),
                ])),
            )
            .unwrap();
        assert!(matches!(initial.step, RepairsFlowProgressStep::Form { .. }));
        assert!(initial.to_value().unwrap()["data_schema"].is_array());
        assert_eq!(initial.to_value().unwrap()["context"]["issue_id"], "repair");
        assert_eq!(
            initial.to_value().unwrap()["context"]["caller"],
            "caller-context"
        );
        let result = manager
            .submit_step(&initial.flow_id, json!({"confirm":true}))
            .unwrap();
        assert!(matches!(
            result.step,
            RepairsFlowProgressStep::CreateEntry { .. }
        ));
        assert_eq!(manager.get_issue("fixture", "repair").unwrap(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let value = result.to_value().unwrap();
        assert_eq!(value["data"]["ok"], true);
        assert_eq!(value["context"]["caller"], "caller-context");
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.submit_step(&initial.flow_id, json!({})),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.abort_flow(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
    }

    #[test]
    fn terminal_create_links_real_options_flow_and_keeps_child_independent() {
        let (runtime, config) = native_config();
        let entry_a = native_entry(&config, "entry-a");
        let entry_b = native_entry(&config, "entry-b");
        assert_ne!(entry_a, entry_b);
        let child = config.init_options_flow(&entry_a).unwrap();
        let child_id = child["flow_id"].as_str().unwrap().to_owned();
        assert_eq!(child["handler"], entry_a);

        let (_integration, manager) = setup_native_repairs(
            runtime,
            config.clone(),
            "repair-options",
            NextFlow::new(NextFlowType::OptionsFlow, child_id.clone()),
            TerminalOutcome::CreateEntry,
        );
        let initial = manager
            .start_fix_flow("repairs_fixture", "repair-options")
            .unwrap();
        let finished = manager.submit_step(&initial.flow_id, json!({})).unwrap();
        let response = finished.to_value().unwrap();
        assert_eq!(response["type"], "create_entry");
        assert_eq!(response["context"]["issue_id"], "repair-options");
        assert_eq!(response["data"]["fixed"], true);
        assert_eq!(response["next_flow"], json!(["options_flow", child_id]));
        assert_eq!(response["result"]["entry_id"], entry_a);
        assert_ne!(response["result"]["entry_id"], entry_b);
        assert!(response.to_string().find("secret-config").is_none());
        assert!(response.to_string().find("secret-option").is_none());
        assert!(
            manager
                .get_issue("repairs_fixture", "repair-options")
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.abort_flow(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.submit_step(&initial.flow_id, json!({})),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert_eq!(
            config
                .active_flow(hass_config::ConfigFlowType::Options, &child_id)
                .unwrap()
                .result["type"],
            "form"
        );
        let child_done = config.submit_flow(&child_id, Map::new()).unwrap();
        assert_eq!(child_done["type"], "create_entry");
        assert!(
            config
                .active_flow(hass_config::ConfigFlowType::Options, &child_id)
                .is_err()
        );
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
    }

    #[test]
    fn default_confirmation_completes_on_any_submission_and_preserves_placeholders() {
        let (_integration, manager) = setup();
        let initial = manager.start_fix_flow("fixture", "repair").unwrap();
        let first = initial.to_value().unwrap();
        assert_eq!(first["data_schema"], json!([]));
        assert_eq!(first["description_placeholders"]["label"], "safe-note");
        let finished = manager
            .submit_step(&initial.flow_id, json!({"anything":false}))
            .unwrap();
        assert!(matches!(
            finished.step,
            RepairsFlowProgressStep::CreateEntry { .. }
        ));
        assert_eq!(finished.to_value().unwrap()["data"], json!({}));
        assert!(manager.get_issue("fixture", "repair").unwrap().is_none());
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        let mut draft = IssueDraft::new("fixture", "not_fixable", false, "warning", "x");
        manager.create_issue(draft.clone()).unwrap();
        assert!(matches!(
            manager.start_fix_flow("fixture", "not_fixable"),
            Err(RepairsError::NotFixable { .. })
        ));
        draft.is_fixable = true;
        manager.create_issue(draft).unwrap();
    }

    #[test]
    fn immediate_terminal_flows_are_returned_but_not_retained() {
        let (_integration, manager) = setup();
        manager
            .create_issue(IssueDraft::new(
                "immediate_success",
                "success",
                true,
                "error",
                "fix",
            ))
            .unwrap();
        manager
            .create_issue(IssueDraft::new(
                "immediate_abort",
                "abort",
                true,
                "error",
                "fix",
            ))
            .unwrap();
        manager
            .register_platform(
                "immediate_success",
                Arc::new(ImmediatePlatform(RepairsFlowStep::CreateEntry {
                    title: None,
                    data: Map::new(),
                    description: None,
                    description_placeholders: None,
                    next_flow: None,
                })),
            )
            .unwrap();
        manager
            .register_platform(
                "immediate_abort",
                Arc::new(ImmediatePlatform(RepairsFlowStep::Abort {
                    reason: "already_done".into(),
                    description_placeholders: None,
                    translation_domain: None,
                    next_flow: None,
                })),
            )
            .unwrap();

        let success = manager
            .start_fix_flow("immediate_success", "success")
            .unwrap();
        assert_eq!(success.to_value().unwrap()["type"], "create_entry");
        assert!(
            manager
                .get_issue("immediate_success", "success")
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            manager.flow_result(&success.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));

        let aborted = manager.start_fix_flow("immediate_abort", "abort").unwrap();
        assert_eq!(aborted.to_value().unwrap()["type"], "abort");
        assert!(
            manager
                .get_issue("immediate_abort", "abort")
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            manager.flow_result(&aborted.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(lock(&manager.live().unwrap().flows).is_empty());
    }

    #[test]
    fn terminal_create_with_config_flow_uses_context_entry_and_null_for_new_entry() {
        let (runtime, config) = native_config();
        let entry_id = native_entry(&config, "config-parent");
        let reauth = config.begin_reauth_flow(&entry_id).unwrap();
        let reauth_id = reauth["flow_id"].as_str().unwrap().to_owned();
        assert_eq!(reauth["context"]["entry_id"], entry_id);

        let (_integration, manager) = setup_native_repairs(
            runtime.clone(),
            config.clone(),
            "repair-config-linked",
            NextFlow::new(NextFlowType::ConfigFlow, reauth_id.clone()),
            TerminalOutcome::CreateEntry,
        );
        let initial = manager
            .start_fix_flow("repairs_fixture", "repair-config-linked")
            .unwrap();
        let finished = manager.submit_step(&initial.flow_id, json!({})).unwrap();
        assert_eq!(finished.to_value().unwrap()["result"]["entry_id"], entry_id);
        assert_eq!(
            finished.to_value().unwrap()["next_flow"],
            json!(["config_flow", reauth_id])
        );

        let new_flow = config
            .init_config_flow("native_fixture", "user", Map::new())
            .unwrap();
        let new_flow_id = new_flow["flow_id"].as_str().unwrap().to_owned();
        let (_integration, manager) = setup_native_repairs(
            runtime,
            config.clone(),
            "repair-config-new",
            NextFlow::new(NextFlowType::ConfigFlow, new_flow_id.clone()),
            TerminalOutcome::Abort,
        );
        let initial = manager
            .start_fix_flow("repairs_fixture", "repair-config-new")
            .unwrap();
        let aborted = manager.submit_step(&initial.flow_id, json!({})).unwrap();
        let response = aborted.to_value().unwrap();
        assert_eq!(response["type"], "abort");
        assert!(response["result"].is_null());
        assert_eq!(response["next_flow"], json!(["config_flow", new_flow_id]));
        assert!(
            manager
                .get_issue("repairs_fixture", "repair-config-new")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn terminal_abort_keeps_real_subentry_flow_independent_and_links_parent_entry() {
        let (runtime, config) = native_config();
        let entry_id = native_entry(&config, "subentry-parent");
        let child = config
            .init_subentry_flow(&entry_id, "child", "user", None, Map::new(), None)
            .unwrap();
        let child_id = child["flow_id"].as_str().unwrap().to_owned();
        assert_eq!(child["handler"], json!([entry_id, "child"]));

        let (_integration, manager) = setup_native_repairs(
            runtime,
            config.clone(),
            "repair-subentry",
            NextFlow::new(NextFlowType::ConfigSubentriesFlow, child_id.clone()),
            TerminalOutcome::Abort,
        );
        let initial = manager
            .start_fix_flow("repairs_fixture", "repair-subentry")
            .unwrap();
        let aborted = manager.submit_step(&initial.flow_id, json!({})).unwrap();
        let response = aborted.to_value().unwrap();
        assert_eq!(response["type"], "abort");
        assert_eq!(
            response["next_flow"],
            json!(["config_subentries_flow", child_id])
        );
        assert_eq!(response["result"]["entry_id"], entry_id);
        assert!(
            manager
                .get_issue("repairs_fixture", "repair-subentry")
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.abort_flow(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert_eq!(
            config
                .active_flow(hass_config::ConfigFlowType::ConfigSubentries, &child_id)
                .unwrap()
                .result["type"],
            "form"
        );
        assert_eq!(
            config.submit_flow(&child_id, Map::new()).unwrap()["type"],
            "create_entry"
        );
        assert!(
            manager
                .get_issue("repairs_fixture", "repair-subentry")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn mismatched_manager_is_unknown_and_outer_abort_does_not_abort_child() {
        let (runtime, config) = native_config();
        let entry_id = native_entry(&config, "wrong-manager");
        let child = config
            .init_config_flow(
                "native_fixture",
                "user",
                Map::from_iter([("entry_id".into(), json!(entry_id))]),
            )
            .unwrap();
        let child_id = child["flow_id"].as_str().unwrap().to_owned();

        let (_integration, manager) = setup_native_repairs(
            runtime,
            config.clone(),
            "repair-wrong-manager",
            NextFlow::new(NextFlowType::OptionsFlow, child_id.clone()),
            TerminalOutcome::CreateEntry,
        );
        let initial = manager
            .start_fix_flow("repairs_fixture", "repair-wrong-manager")
            .unwrap();
        assert!(matches!(
            manager.submit_step(&initial.flow_id, json!({})),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert!(matches!(
            manager.flow_result(&initial.flow_id).unwrap().step,
            RepairsFlowProgressStep::Form { .. }
        ));
        assert!(
            manager
                .get_issue("repairs_fixture", "repair-wrong-manager")
                .unwrap()
                .is_some()
        );
        manager.abort_flow(&initial.flow_id).unwrap();
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::UnknownFlow(_))
        ));
        assert_eq!(
            config
                .active_flow(hass_config::ConfigFlowType::Config, &child_id)
                .unwrap()
                .result["context"]["entry_id"],
            entry_id
        );
        assert!(
            config
                .active_flow(hass_config::ConfigFlowType::Options, &child_id)
                .is_err()
        );
    }

    #[test]
    fn processing_guard_serializes_callbacks_and_owner_drop_closes_after_inflight_work() {
        let (integration, manager) = setup();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        manager
            .register_platform(
                "fixture",
                Arc::new(GatedPlatform {
                    entered: Mutex::new(Some(entered_tx)),
                    release: Mutex::new(Some(release_rx)),
                }),
            )
            .unwrap();
        let initial = manager.start_fix_flow("fixture", "repair").unwrap();
        let worker_manager = manager.clone();
        let flow_id = initial.flow_id.clone();
        let worker =
            thread::spawn(move || worker_manager.submit_step(&flow_id, json!({"go":true})));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            manager.submit_step(&initial.flow_id, json!({})),
            Err(RepairsError::UnknownStep(_))
        ));
        assert!(matches!(
            manager.abort_flow(&initial.flow_id),
            Err(RepairsError::UnknownStep(_))
        ));
        assert!(matches!(
            manager.flow_result(&initial.flow_id).unwrap().step,
            RepairsFlowProgressStep::Form { .. }
        ));

        drop(integration);
        release_tx.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap().unwrap().step,
            RepairsFlowProgressStep::CreateEntry { .. }
        ));
        assert!(matches!(
            manager.flow_result(&initial.flow_id),
            Err(RepairsError::Core(hass_core::Error::Service(_)))
        ));
    }

    #[test]
    fn owner_drop_closes_manager_and_platform_unregister_is_scoped() {
        let (integration, manager) = setup();
        let calls = Arc::new(AtomicUsize::new(0));
        manager
            .register_platform("fixture", Arc::new(FixturePlatform { calls }))
            .unwrap();
        assert!(manager.unregister_platform("fixture").unwrap());
        assert!(!manager.unregister_platform("fixture").unwrap());
        let weak = manager.clone();
        drop(integration);
        assert!(matches!(
            weak.start_fix_flow("fixture", "repair"),
            Err(RepairsError::Core(hass_core::Error::Service(_)))
        ));
    }

    #[test]
    fn platform_registration_guard_unregisters_its_registration_on_drop() {
        let (_integration, manager) = setup();
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = manager
            .register_platform_guard("fixture", Arc::new(FixturePlatform { calls }))
            .unwrap();

        drop(registration);

        assert!(!manager.unregister_platform("fixture").unwrap());
    }

    #[test]
    fn stale_platform_registration_guard_does_not_remove_replacement() {
        let (_integration, manager) = setup();
        let old_calls = Arc::new(AtomicUsize::new(0));
        let stale_registration = manager
            .register_platform_guard("fixture", Arc::new(FixturePlatform { calls: old_calls }))
            .unwrap();

        assert!(manager.unregister_platform("fixture").unwrap());
        let replacement_calls = Arc::new(AtomicUsize::new(0));
        let replacement_registration = manager
            .register_platform_guard(
                "fixture",
                Arc::new(FixturePlatform {
                    calls: replacement_calls,
                }),
            )
            .unwrap();

        drop(stale_registration);

        assert!(manager.unregister_platform("fixture").unwrap());
        drop(replacement_registration);
        assert!(!manager.unregister_platform("fixture").unwrap());
    }

    #[test]
    fn platform_registration_guard_is_safe_after_owner_drop() {
        let (integration, manager) = setup();
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = manager
            .register_platform_guard("fixture", Arc::new(FixturePlatform { calls }))
            .unwrap();

        drop(integration);

        assert!(matches!(
            manager.unregister_platform("fixture"),
            Err(RepairsError::Core(hass_core::Error::Service(_)))
        ));
        drop(registration);
    }
}
