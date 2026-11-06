//! Native, bounded scripts and scenes built on the shared runtime services.
//!
//! The supported YAML subset and its limits are documented in `README.md`.

use chrono::{DateTime, Utc};
use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult, State};
use hass_template::TemplateEngine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::{mpsc as async_mpsc, watch},
    task::JoinSet,
    time::{Instant, sleep_until},
};
use uuid::Uuid;

const MAX_ACTIONS: usize = 256;
const MAX_SEQUENCE_ACTIONS: usize = 64;
const MAX_CONDITION_NODES: usize = 64;
const MAX_NESTING: usize = 12;
const MAX_REPEAT: u32 = 100;
const MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_RUN_TIME: Duration = Duration::from_secs(60);
const MAX_SCRIPT_CONCURRENCY: usize = 32;
const MAX_ACTIVE_RUNS: usize = 64;
const MAX_PENDING_RUNS: usize = 128;
const MAX_SCENES: usize = 256;
const MAX_SCENE_ENTITIES: usize = 64;
const MAX_COMMANDS: usize = 128;
const SERVICE_REPLY_TIMEOUT: Duration = Duration::from_secs(65);
const SCENE_METADATA_KEY: &str = "hass-script.dynamic-scenes.v1";

thread_local! {
    /// Carries script ancestry through the synchronous Runtime service bridge.
    static SCRIPT_LINEAGE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Script execution mode, matching Home Assistant's four scheduling modes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMode {
    /// Ignore a new run while one is active.
    #[default]
    Single,
    /// Cancel prior runs and begin the newest request.
    Restart,
    /// Run one request at a time and queue up to `max` requests.
    Queued,
    /// Run up to `max` requests at the same time.
    Parallel,
}

/// One strictly decoded script definition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScriptConfig {
    /// Optional display name; the mapping key remains the stable object ID.
    pub alias: Option<String>,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional script entity icon.
    pub icon: Option<String>,
    /// Scheduling policy for this script.
    pub mode: ScriptMode,
    /// Maximum active/queued runs for `queued` and `parallel` modes.
    pub max: usize,
    /// Initial variables available to the action sequence.
    pub variables: Map<String, Value>,
    /// Ordered script steps.
    pub sequence: Vec<Action>,
}

impl Default for ScriptConfig {
    fn default() -> Self {
        Self {
            alias: None,
            description: None,
            icon: None,
            mode: ScriptMode::Single,
            max: 10,
            variables: Map::new(),
            sequence: Vec::new(),
        }
    }
}

/// A bounded action schema. Exactly one action kind must be specified.
///
/// Service steps accept both current `action:` and legacy `service:` names.
/// Unknown YAML fields are rejected by serde.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Action {
    /// Optional step name.
    pub alias: Option<String>,
    /// Current service spelling, for example `light.turn_on`.
    pub action: Option<String>,
    /// Legacy service spelling, for example `light.turn_on`.
    pub service: Option<String>,
    /// Service payload values.
    pub data: Option<Map<String, Value>>,
    /// Legacy service payload spelling.
    pub service_data: Option<Map<String, Value>>,
    /// Service target fields, usually `entity_id`.
    pub target: Option<Map<String, Value>>,
    /// A delay in seconds, `HH:MM:SS`, or a duration component map.
    pub delay: Option<DelayValue>,
    /// Condition action discriminator (`state`, `numeric_state`, `template`,
    /// `and`, `or`, or `not`).
    pub condition: Option<String>,
    /// Entity IDs for state and numeric-state conditions.
    pub entity_id: Option<EntityIds>,
    /// Expected state for a state condition.
    pub state: Option<String>,
    /// Strict numeric-state lower bound.
    pub above: Option<f64>,
    /// Strict numeric-state upper bound.
    pub below: Option<f64>,
    /// Template condition expression.
    pub value_template: Option<String>,
    /// Alternate template expression spelling.
    pub template: Option<String>,
    /// Child conditions for `and`, `or`, and `not` condition actions.
    pub conditions: Option<Vec<Condition>>,
    /// Ordered choose branches.
    pub choose: Option<Vec<ChooseOption>>,
    /// Actions to run when no choose branch matches.
    pub default: Option<Vec<Action>>,
    /// Bounded repeat configuration.
    pub repeat: Option<RepeatConfig>,
    /// Values assigned to the script's current variable scope.
    pub variables: Option<Map<String, Value>>,
    /// Stop this script normally, optionally carrying a message.
    pub stop: Option<String>,
    /// Treat `stop` as an error.
    pub error: Option<bool>,
}

/// A condition supported in choose branches and repeat guards.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    /// Require every listed entity to have this exact state.
    State { entity_id: EntityIds, state: String },
    /// Require a numeric state to fall strictly between the configured bounds.
    NumericState {
        entity_id: EntityIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
    },
    /// Evaluate a Jinja expression using the template worker.
    Template { value_template: String },
    /// Require every nested condition.
    And { conditions: Vec<Condition> },
    /// Require at least one nested condition.
    Or { conditions: Vec<Condition> },
    /// Invert one nested condition.
    Not { conditions: Vec<Condition> },
}

/// One or more entity IDs accepted in target and condition fields.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EntityIds {
    /// A single entity ID.
    One(String),
    /// Several entity IDs.
    Many(Vec<String>),
}

impl EntityIds {
    fn values(&self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value.clone()],
            Self::Many(values) => values.clone(),
        }
    }
}

/// A conditional choose branch.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChooseOption {
    /// Optional branch name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// All branch conditions must pass.
    pub conditions: Vec<Condition>,
    /// Actions to execute for the first matching branch.
    pub sequence: Vec<Action>,
}

/// Repeat sequence with exactly one finite count or state guard.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatConfig {
    /// Fixed iteration count, limited to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Continue while all conditions are true.
    #[serde(default, rename = "while", skip_serializing_if = "Option::is_none")]
    pub while_conditions: Option<Vec<Condition>>,
    /// Stop after one iteration once all conditions are true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Vec<Condition>>,
    /// Actions to execute on each iteration.
    pub sequence: Vec<Action>,
}

/// Delay duration accepted by a script step.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum DelayValue {
    /// Seconds, including fractional values.
    Seconds(f64),
    /// `HH:MM:SS` text.
    Text(String),
    /// Duration components.
    Parts(DelayParts),
}

/// Duration components for a delay.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelayParts {
    /// Hours.
    pub hours: Option<f64>,
    /// Minutes.
    pub minutes: Option<f64>,
    /// Seconds.
    pub seconds: Option<f64>,
    /// Milliseconds.
    pub milliseconds: Option<f64>,
}

impl DelayValue {
    fn duration(&self) -> Result<Duration> {
        let seconds = match self {
            Self::Seconds(value) => *value,
            Self::Text(text) => parse_clock_duration(text)?,
            Self::Parts(parts) => {
                let values = [
                    parts.hours,
                    parts.minutes,
                    parts.seconds,
                    parts.milliseconds,
                ];
                if values.iter().all(Option::is_none) {
                    return Err(invalid("delay duration must include a time component"));
                }
                if values
                    .iter()
                    .flatten()
                    .any(|value| !value.is_finite() || *value < 0.0)
                {
                    return Err(invalid("delay components must be finite and nonnegative"));
                }
                parts.hours.unwrap_or_default() * 3600.0
                    + parts.minutes.unwrap_or_default() * 60.0
                    + parts.seconds.unwrap_or_default()
                    + parts.milliseconds.unwrap_or_default() / 1000.0
            }
        };
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(invalid("delay must be finite and nonnegative"));
        }
        if seconds > MAX_DELAY.as_secs_f64() {
            return Err(invalid(format!(
                "delay must not exceed {} seconds",
                MAX_DELAY.as_secs()
            )));
        }
        Ok(Duration::from_secs_f64(seconds))
    }
}

/// One scene entry using Home Assistant's `scene:` list syntax.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SceneConfig {
    /// Optional unique ID. The entity object ID is derived from `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Required scene name and source for the entity object ID.
    pub name: String,
    /// Optional icon attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional source metadata accepted by Home Assistant scene YAML.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    /// Entity states, either a state string or `{state, ...attributes}`.
    pub entities: BTreeMap<String, Value>,
}

/// Validate script and scene configuration without touching Runtime state.
pub fn validate_config(
    scripts: &BTreeMap<String, ScriptConfig>,
    scenes: &[SceneConfig],
) -> Result<()> {
    let mut actions = 0;
    for (id, script) in scripts {
        if !valid_object_id(id)
            || matches!(id.as_str(), "turn_on" | "turn_off" | "toggle" | "reload")
        {
            return Err(invalid(format!(
                "script object ID `{id}` is invalid or reserved"
            )));
        }
        if script
            .alias
            .as_ref()
            .is_some_and(|alias| alias.trim().is_empty())
        {
            return Err(invalid(format!("script `{id}` alias must not be empty")));
        }
        if script
            .description
            .as_ref()
            .is_some_and(|value| value.len() > 4096)
        {
            return Err(invalid(format!("script `{id}` description is too long")));
        }
        if script
            .icon
            .as_ref()
            .is_some_and(|icon| icon.trim().is_empty())
        {
            return Err(invalid(format!("script `{id}` icon must not be empty")));
        }
        if !(2..=MAX_SCRIPT_CONCURRENCY).contains(&script.max) {
            return Err(invalid(format!(
                "script `{id}` max must be between 2 and {MAX_SCRIPT_CONCURRENCY}"
            )));
        }
        if script.sequence.len() > MAX_SEQUENCE_ACTIONS {
            return Err(invalid(format!(
                "script `{id}` exceeds the {MAX_SEQUENCE_ACTIONS}-step sequence limit"
            )));
        }
        validate_actions(&script.sequence, 0, &mut actions)
            .map_err(|error| invalid(format!("script `{id}`: {error}")))?;
    }

    let mut scene_ids = std::collections::HashSet::new();
    let mut scene_unique_ids = std::collections::HashSet::new();
    if scenes.len() > MAX_SCENES {
        return Err(invalid(format!(
            "scene configuration exceeds {MAX_SCENES} scenes"
        )));
    }
    for scene in scenes {
        let object_id = scene_object_id(&scene.name)?;
        if !scene_ids.insert(object_id.clone()) {
            return Err(invalid(format!("duplicate scene object ID `{object_id}`")));
        }
        validate_scene_config(scene, false)?;
        if let Some(id) = &scene.id
            && (id.trim().is_empty() || id.len() > 256 || !scene_unique_ids.insert(id.as_str()))
        {
            return Err(invalid(format!(
                "scene unique ID `{id}` is invalid or duplicated"
            )));
        }
    }
    Ok(())
}

/// Cloneable runtime integration for scripts and scenes.
#[derive(Clone)]
pub struct ScriptEngine {
    owner: Arc<EngineOwner>,
}

struct EngineOwner {
    shutdown: watch::Sender<bool>,
    worker: Mutex<Option<thread::JoinHandle<Result<()>>>>,
    closed: AtomicBool,
}

impl Drop for EngineOwner {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.shutdown.send_replace(true);
        }
    }
}

impl ScriptEngine {
    /// Load durable dynamic scenes, start the isolated async actor, and register
    /// script/scene services on the supplied runtime.
    pub fn start(
        runtime: Runtime,
        scripts: BTreeMap<String, ScriptConfig>,
        scenes: Vec<SceneConfig>,
    ) -> Result<Self> {
        validate_config(&scripts, &scenes)?;
        let dynamic_scenes = load_dynamic_scenes(&runtime)?;
        let stored_scenes = build_scene_map(&scenes, &dynamic_scenes)?;
        let scene_store = Arc::new(SceneStore {
            scenes: RwLock::new(stored_scenes),
            dynamic: Mutex::new(dynamic_scenes),
        });

        for (id, config) in &scripts {
            initialize_script_state(&runtime, id, config)?;
        }
        for (entity_id, scene) in scene_store
            .scenes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            initialize_scene_state(&runtime, entity_id, &scene.config)?;
        }

        let (commands, command_rx) = async_mpsc::channel(MAX_COMMANDS);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let thread_runtime = runtime.clone();
        let thread_scripts = Arc::new(scripts.clone());
        let thread_scenes = Arc::clone(&scene_store);
        let worker = thread::Builder::new()
            .name("hass-script-actor".into())
            .spawn(move || {
                let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .max_blocking_threads(64)
                    .thread_name("hass-script-tokio")
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        Error::Service(format!("script runtime startup failed: {error}"))
                    })?;
                tokio_runtime.block_on(run_scheduler(
                    thread_runtime,
                    thread_scripts,
                    thread_scenes,
                    command_rx,
                    shutdown_rx,
                ))
            })
            .map_err(|error| Error::Service(format!("cannot start script actor: {error}")))?;
        let owner = Arc::new(EngineOwner {
            shutdown,
            worker: Mutex::new(Some(worker)),
            closed: AtomicBool::new(false),
        });

        register_services(&runtime, &commands, &scripts)?;
        Ok(Self { owner })
    }

    /// Cancel script delays, stop accepting new actor work, and join all runs.
    /// Already-entered synchronous Runtime service handlers are joined because
    /// the core API cannot forcibly cancel arbitrary handlers.
    pub async fn shutdown(self) -> Result<()> {
        self.owner.closed.store(true, Ordering::Release);
        self.owner.shutdown.send_replace(true);
        let worker = self
            .owner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| Error::Service(format!("script actor join task failed: {error}")))?
                .map_err(|_| Error::Service("script actor thread panicked".into()))??;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct StoredScene {
    config: SceneConfig,
    dynamic: bool,
}

struct SceneStore {
    scenes: RwLock<BTreeMap<String, StoredScene>>,
    dynamic: Mutex<BTreeMap<String, SceneConfig>>,
}

enum Command {
    Start(StartRequest),
    Control {
        operation: ScriptOperation,
        entity_ids: Vec<String>,
        variables: Map<String, Value>,
        ancestry: Vec<String>,
        context: Context,
        reply: SyncSender<Result<ServiceResult>>,
    },
    SceneTurnOn {
        entity_ids: Vec<String>,
        ancestry: Vec<String>,
        transition: Option<f64>,
        context: Context,
        reply: SyncSender<Result<ServiceResult>>,
    },
    SceneCreate {
        data: Map<String, Value>,
        context: Context,
        reply: SyncSender<Result<ServiceResult>>,
    },
}

struct StartRequest {
    id: String,
    variables: Map<String, Value>,
    ancestry: Vec<String>,
    context: Context,
    wait: bool,
    reply: SyncSender<Result<ServiceResult>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptOperation {
    TurnOn,
    TurnOff,
    Toggle,
}

struct PendingRun {
    id: String,
    run_id: String,
    variables: Map<String, Value>,
    ancestry: Vec<String>,
    context: Context,
    waiter: Option<SyncSender<Result<ServiceResult>>>,
    accepted_reply: Option<SyncSender<Result<ServiceResult>>>,
}

struct ActiveRun {
    run_id: String,
    cancel: watch::Sender<bool>,
}

struct Completion {
    id: String,
    run_id: String,
    result: Result<()>,
    context: Context,
    waiter: Option<SyncSender<Result<ServiceResult>>>,
}

async fn run_scheduler(
    runtime: Runtime,
    scripts: Arc<BTreeMap<String, ScriptConfig>>,
    scenes: Arc<SceneStore>,
    mut commands: async_mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let template = TemplateEngine::new(runtime.clone());
    let mut active: HashMap<String, Vec<ActiveRun>> = HashMap::new();
    let mut pending: HashMap<String, VecDeque<PendingRun>> = HashMap::new();
    let mut runs = JoinSet::<Completion>::new();
    let mut scene_runs = JoinSet::<()>::new();
    let mut last_triggered = HashMap::<String, DateTime<Utc>>::new();
    let mut shutting_down = false;

    loop {
        if *shutdown.borrow() {
            shutting_down = true;
        }
        if shutting_down {
            for runs_for_script in active.values() {
                for run in runs_for_script {
                    run.cancel.send_replace(true);
                }
            }
            for queue in pending.values_mut() {
                while let Some(run) = queue.pop_front() {
                    reply_error(
                        run.waiter,
                        Error::Service("script engine stopped before queued run".into()),
                    );
                }
            }
            pending.clear();
            if runs.is_empty() && scene_runs.is_empty() {
                return Ok(());
            }
            tokio::select! {
                joined = runs.join_next(), if !runs.is_empty() => {
                    if let Some(joined) = joined {
                        finish_run(joined, &runtime, &scripts, &mut active, &mut pending,
                            &mut runs, &mut last_triggered, &template).await;
                    }
                }
                joined = scene_runs.join_next(), if !scene_runs.is_empty() => {
                    if let Some(Err(error)) = joined {
                    tracing::error!(target: "hass_script::scene", %error, "scene apply task failed during shutdown");
                    }
                }
            }
            continue;
        }

        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    shutting_down = true;
                }
            }
            command = commands.recv() => {
                match command {
                    Some(Command::Start(request)) => {
                        handle_start(
                            request, &runtime, &scripts, &mut active, &mut pending,
                            &mut runs, &mut last_triggered, &template,
                        ).await;
                    }
                    Some(Command::Control { operation, entity_ids, variables, ancestry, context, reply }) => {
                        handle_control(
                            operation, entity_ids, variables, ancestry, context, reply,
                            &runtime, &scripts, &mut active, &mut pending,
                            &mut runs, &mut last_triggered, &template,
                        ).await;
                    }
                    Some(Command::SceneTurnOn { entity_ids, ancestry, transition, context, reply }) => {
                        if scene_runs.len() >= 16 {
                            let _ = reply.send(Err(Error::Service("scene apply concurrency limit reached".into())));
                            continue;
                        }
                        let runtime = runtime.clone();
                        let scenes = Arc::clone(&scenes);
                        scene_runs.spawn(async move {
                            let result = apply_scenes(runtime, scenes, entity_ids, ancestry, transition, context).await;
                            let _ = reply.send(result);
                        });
                    }
                    Some(Command::SceneCreate { data, context, reply }) => {
                        let result = create_scene(&runtime, &scenes, data, context);
                        let _ = reply.send(result);
                    }
                    None => shutting_down = true,
                }
            }
            joined = runs.join_next(), if !runs.is_empty() => {
                if let Some(joined) = joined {
                    finish_run(
                        joined, &runtime, &scripts, &mut active, &mut pending,
                        &mut runs, &mut last_triggered, &template,
                    ).await;
                }
            }
            joined = scene_runs.join_next(), if !scene_runs.is_empty() => {
                if let Some(Err(error)) = joined {
                tracing::error!(target: "hass_script::scene", %error, "scene apply task failed");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_start(
    request: StartRequest,
    runtime: &Runtime,
    scripts: &BTreeMap<String, ScriptConfig>,
    active: &mut HashMap<String, Vec<ActiveRun>>,
    pending: &mut HashMap<String, VecDeque<PendingRun>>,
    runs: &mut JoinSet<Completion>,
    last_triggered: &mut HashMap<String, DateTime<Utc>>,
    template: &TemplateEngine,
) {
    let Some(config) = scripts.get(&request.id) else {
        let _ = request
            .reply
            .send(Err(Error::NotFound(format!("script.{}", request.id))));
        return;
    };
    if request.ancestry.iter().any(|id| id == &request.id) || request.ancestry.len() >= MAX_NESTING
    {
        let _ = request.reply.send(Err(Error::Service(format!(
            "script recursion loop detected at `{}`",
            request.id
        ))));
        return;
    }
    let running = active.get(&request.id).map_or(0, Vec::len);
    let queued = pending.get(&request.id).map_or(0, VecDeque::len);
    let active_total: usize = active.values().map(Vec::len).sum();
    let pending_total: usize = pending.values().map(VecDeque::len).sum();
    match config.mode {
        ScriptMode::Single if running > 0 => {
            let _ = request.reply.send(Err(Error::Service(format!(
                "script `{}` is already running in single mode",
                request.id
            ))));
            return;
        }
        ScriptMode::Restart => {
            if let Some(current) = active.get(&request.id) {
                for run in current {
                    run.cancel.send_replace(true);
                }
            }
            if let Some(queue) = pending.remove(&request.id) {
                for queued_run in queue {
                    reply_error(
                        queued_run.waiter,
                        Error::Service("queued script run was superseded by restart".into()),
                    );
                }
            }
        }
        ScriptMode::Queued if running > 0 => {
            if running + queued >= config.max || pending_total >= MAX_PENDING_RUNS {
                let _ = request.reply.send(Err(Error::Service(format!(
                    "script `{}` queued run limit reached (per-script {}, global {})",
                    request.id, config.max, MAX_PENDING_RUNS
                ))));
                return;
            }
            let queued_run = pending_run(request);
            let accepted_reply = queued_run.accepted_reply.clone();
            let id = queued_run.id.clone();
            let context = queued_run.context.clone();
            pending
                .entry(queued_run.id.clone())
                .or_default()
                .push_back(queued_run);
            if let Some(reply) = accepted_reply {
                let _ = reply.send(Ok(service_result(
                    runtime,
                    &format!("script.{id}"),
                    Some(json!({"accepted": true, "queued": true, "script_id": id})),
                    context,
                )));
            }
            return;
        }
        ScriptMode::Parallel if running >= config.max => {
            let _ = request.reply.send(Err(Error::Service(format!(
                "script `{}` parallel run limit ({}) reached",
                request.id, config.max
            ))));
            return;
        }
        _ => {}
    }
    if active_total >= MAX_ACTIVE_RUNS {
        let _ = request.reply.send(Err(Error::Service(format!(
            "script actor active-run limit ({MAX_ACTIVE_RUNS}) reached"
        ))));
        return;
    }

    let run = pending_run(request);
    start_run(
        run,
        runtime,
        scripts,
        active,
        runs,
        last_triggered,
        template,
    );
}

// The scheduler accepts a queued request and replies immediately for
// fire-and-forget service calls. Waiting entity service calls keep their reply
// with the queued run until that run completes.
fn pending_run(request: StartRequest) -> PendingRun {
    let (waiter, accepted_reply) = if request.wait {
        (Some(request.reply), None)
    } else {
        (None, Some(request.reply))
    };
    PendingRun {
        id: request.id,
        run_id: Uuid::new_v4().to_string(),
        variables: request.variables,
        ancestry: request.ancestry,
        context: request.context,
        waiter,
        accepted_reply,
    }
}

fn start_run(
    mut run: PendingRun,
    runtime: &Runtime,
    scripts: &BTreeMap<String, ScriptConfig>,
    active: &mut HashMap<String, Vec<ActiveRun>>,
    runs: &mut JoinSet<Completion>,
    last_triggered: &mut HashMap<String, DateTime<Utc>>,
    template: &TemplateEngine,
) {
    let Some(config) = scripts.get(&run.id).cloned() else {
        reply_error(
            run.waiter.take(),
            Error::NotFound(format!("script.{}", run.id)),
        );
        reply_error(
            run.accepted_reply.take(),
            Error::NotFound(format!("script.{}", run.id)),
        );
        return;
    };
    let (cancel, cancel_rx) = watch::channel(false);
    let active_for_script = active.entry(run.id.clone()).or_default();
    active_for_script.push(ActiveRun {
        run_id: run.run_id.clone(),
        cancel: cancel.clone(),
    });
    let current = active_for_script.len();
    let now = Utc::now();
    last_triggered.insert(run.id.clone(), now);
    let state = match set_script_state(
        runtime,
        &run.id,
        &config,
        current,
        Some(now),
        run.context.clone(),
    ) {
        Ok(state) => state,
        Err(error) => {
            cancel.send_replace(true);
            active_for_script.retain(|active_run| active_run.run_id != run.run_id);
            reply_error(run.waiter.take(), error.clone());
            reply_error(run.accepted_reply.take(), error);
            return;
        }
    };
    let id = run.id.clone();
    let run_id = run.run_id.clone();
    let context = run.context.clone();
    let lineage = {
        let mut lineage = run.ancestry.clone();
        lineage.push(run.id.clone());
        lineage
    };
    let runtime = runtime.clone();
    let template = template.clone();
    let waiter = run.waiter.take();
    runs.spawn(async move {
        let execution_context = Context {
            id: Uuid::new_v4().to_string(),
            parent_id: Some(context.id.clone()),
            user_id: context.user_id.clone(),
        };
        let result = execute_script(
            runtime,
            config,
            run.variables,
            lineage,
            execution_context,
            cancel_rx,
            template,
        )
        .await;
        Completion {
            id,
            run_id,
            result,
            context,
            waiter,
        }
    });
    if let Some(reply) = run.accepted_reply.take() {
        let _ = reply.send(Ok(ServiceResult {
            changed_states: vec![state],
            response: Some(json!({"accepted": true, "script_id": run.id})),
            context: run.context,
        }));
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_control(
    operation: ScriptOperation,
    entity_ids: Vec<String>,
    variables: Map<String, Value>,
    ancestry: Vec<String>,
    context: Context,
    reply: SyncSender<Result<ServiceResult>>,
    runtime: &Runtime,
    scripts: &BTreeMap<String, ScriptConfig>,
    active: &mut HashMap<String, Vec<ActiveRun>>,
    pending: &mut HashMap<String, VecDeque<PendingRun>>,
    runs: &mut JoinSet<Completion>,
    last_triggered: &mut HashMap<String, DateTime<Utc>>,
    template: &TemplateEngine,
) {
    let mut changed_states = Vec::new();
    let mut results: Vec<Result<()>> = Vec::new();
    for entity_id in entity_ids {
        let Some(id) = entity_id.strip_prefix("script.") else {
            results.push(Err(Error::NotFound(format!("script entity `{entity_id}`"))));
            continue;
        };
        let Some(config) = scripts.get(id) else {
            results.push(Err(Error::NotFound(format!("script.{id}"))));
            continue;
        };
        let active_count = active.get(id).map_or(0, Vec::len);
        let has_queued = pending.get(id).is_some_and(|queue| !queue.is_empty());
        let should_stop = matches!(operation, ScriptOperation::TurnOff)
            || (matches!(operation, ScriptOperation::Toggle) && (active_count > 0 || has_queued));
        if should_stop {
            if let Some(current) = active.get(id) {
                for run in current {
                    run.cancel.send_replace(true);
                }
            }
            if let Some(queue) = pending.remove(id) {
                for queued_run in queue {
                    reply_error(
                        queued_run.waiter,
                        Error::Service("queued script run was cancelled".into()),
                    );
                }
            }
            if active_count == 0 {
                match set_script_state(
                    runtime,
                    id,
                    config,
                    0,
                    last_triggered.get(id).cloned(),
                    context.clone(),
                ) {
                    Ok(state) => changed_states.push(state),
                    Err(error) => results.push(Err(error)),
                }
            }
        } else {
            let request = StartRequest {
                id: id.to_owned(),
                variables: variables.clone(),
                ancestry: ancestry.clone(),
                context: context.clone(),
                wait: false,
                reply: reply.clone(),
            };
            // Reuse scheduling in place; a temporary channel prevents each
            // target from consuming the aggregate service response.
            let (target_tx, target_rx) = mpsc::sync_channel(1);
            let mut request = request;
            request.reply = target_tx;
            handle_start(
                request,
                runtime,
                scripts,
                active,
                pending,
                runs,
                last_triggered,
                template,
            )
            .await;
            match target_rx.recv_timeout(SERVICE_REPLY_TIMEOUT) {
                Ok(Ok(result)) => changed_states.extend(result.changed_states),
                Ok(Err(error)) => results.push(Err(error)),
                Err(error) => results.push(Err(timeout_error(error))),
            }
        }
    }
    if let Some(error) = results.into_iter().find_map(std::result::Result::err) {
        let _ = reply.send(Err(error));
    } else {
        let _ = reply.send(Ok(ServiceResult {
            changed_states,
            response: None,
            context,
        }));
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_run(
    joined: std::result::Result<Completion, tokio::task::JoinError>,
    runtime: &Runtime,
    scripts: &BTreeMap<String, ScriptConfig>,
    active: &mut HashMap<String, Vec<ActiveRun>>,
    pending: &mut HashMap<String, VecDeque<PendingRun>>,
    runs: &mut JoinSet<Completion>,
    last_triggered: &mut HashMap<String, DateTime<Utc>>,
    template: &TemplateEngine,
) {
    let completion = match joined {
        Ok(completion) => completion,
        Err(error) => {
            tracing::error!(%error, "script action task panicked");
            return;
        }
    };
    let Some(config) = scripts.get(&completion.id) else {
        reply_error(
            completion.waiter,
            Error::NotFound(format!("script.{}", completion.id)),
        );
        return;
    };
    if let Some(runs) = active.get_mut(&completion.id) {
        runs.retain(|run| run.run_id != completion.run_id);
        if runs.is_empty() {
            active.remove(&completion.id);
        }
    }
    let current = active.get(&completion.id).map_or(0, Vec::len);
    let state_result = set_script_state(
        runtime,
        &completion.id,
        config,
        current,
        last_triggered.get(&completion.id).cloned(),
        completion.context.clone(),
    );
    let completion_result = match (completion.result, state_result) {
        (Ok(()), Ok(state)) => Ok(state),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    };
    if let Some(waiter) = completion.waiter {
        let response = completion_result.map(|state| ServiceResult {
            changed_states: vec![state],
            response: Some(json!({"completed": true, "script_id": completion.id})),
            context: completion.context.clone(),
        });
        let _ = waiter.send(response);
    } else if let Err(error) = &completion_result {
        tracing::warn!(script_id = %completion.id, %error, "script run failed");
    }

    if current == 0 && config.mode == ScriptMode::Queued {
        let next = pending
            .get_mut(&completion.id)
            .and_then(VecDeque::pop_front);
        if pending.get(&completion.id).is_some_and(VecDeque::is_empty) {
            pending.remove(&completion.id);
        }
        if let Some(next) = next {
            start_run(
                next,
                runtime,
                scripts,
                active,
                runs,
                last_triggered,
                template,
            );
        }
    }
}

async fn execute_script(
    runtime: Runtime,
    config: ScriptConfig,
    supplied_variables: Map<String, Value>,
    lineage: Vec<String>,
    context: Context,
    mut cancel: watch::Receiver<bool>,
    template: TemplateEngine,
) -> Result<()> {
    let deadline = Instant::now() + MAX_RUN_TIME;
    let mut variables = config.variables;
    for (key, value) in supplied_variables {
        variables.insert(key, value);
    }
    variables = render_map(&template, &variables, &variables)?;
    let mut executed = 0;
    match execute_actions(
        &runtime,
        &template,
        &config.sequence,
        &mut variables,
        &lineage,
        &context,
        &mut cancel,
        deadline,
        0,
        &mut executed,
    )
    .await?
    {
        ExecutionControl::Continue | ExecutionControl::Stop | ExecutionControl::Cancelled => Ok(()),
    }
}

/// Validate a script sequence stored as a JSON value. This is used by other
/// native integrations that reuse the same action runner.
pub fn validate_sequence(sequence: &Value) -> Result<()> {
    let actions: Vec<Action> = serde_json::from_value(sequence.clone())
        .map_err(|error| invalid(format!("invalid script sequence: {error}")))?;
    let mut total = 0;
    validate_actions(&actions, 0, &mut total)
}

/// Run a validated sequence with the shared bounded action executor.
///
/// The caller-supplied context is passed unchanged to each invoked service.
/// Cancellation is observed between actions and during delays. A synchronous
/// service handler already entered through `Runtime::call_service` cannot be
/// interrupted and is joined before this future returns.
pub async fn run_sequence(
    runtime: Runtime,
    sequence: Value,
    mut variables: Map<String, Value>,
    context: Context,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let actions: Vec<Action> = serde_json::from_value(sequence)
        .map_err(|error| invalid(format!("invalid script sequence: {error}")))?;
    let mut total = 0;
    validate_actions(&actions, 0, &mut total)?;
    if *cancel.borrow() {
        return Ok(());
    }
    let template = TemplateEngine::new(runtime.clone());
    variables = render_map(&template, &variables, &variables)?;
    let deadline = Instant::now() + MAX_RUN_TIME;
    let mut executed = 0;
    let _ = execute_actions(
        &runtime,
        &template,
        &actions,
        &mut variables,
        &current_lineage(),
        &context,
        &mut cancel,
        deadline,
        0,
        &mut executed,
    )
    .await?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecutionControl {
    Continue,
    Stop,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn execute_actions(
    runtime: &Runtime,
    template: &TemplateEngine,
    actions: &[Action],
    variables: &mut Map<String, Value>,
    lineage: &[String],
    context: &Context,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
    depth: usize,
    executed: &mut usize,
) -> Result<ExecutionControl> {
    if depth > MAX_NESTING {
        return Err(Error::Service(
            "script nested sequence depth limit reached".into(),
        ));
    }
    for action in actions {
        if *cancel.borrow() {
            return Ok(ExecutionControl::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::Service(
                "script exceeded its 60-second run limit".into(),
            ));
        }
        *executed += 1;
        if *executed > MAX_ACTIONS {
            return Err(Error::Service(format!(
                "script exceeded the {MAX_ACTIONS}-action run limit"
            )));
        }
        let kind_count = action.kind_count();
        if kind_count != 1 {
            return Err(Error::InvalidInput(
                "script step must contain exactly one action kind".into(),
            ));
        }
        if let Some(service) = action.action.as_deref().or(action.service.as_deref()) {
            let (domain, service_name) = service
                .split_once('.')
                .ok_or_else(|| invalid(format!("invalid service name `{service}`")))?;
            let source_data = action.data.as_ref().or(action.service_data.as_ref());
            let empty = Map::new();
            let data = render_map(template, source_data.unwrap_or(&empty), variables)?;
            let target = render_map(
                template,
                action.target.as_ref().unwrap_or(&empty),
                variables,
            )?;
            let call = ServiceCall {
                domain: domain.to_owned(),
                service: service_name.to_owned(),
                data,
                target,
                context: context.clone(),
            };
            if call_service_cancellable(runtime.clone(), call, lineage.to_vec(), deadline, cancel)
                .await?
                .is_none()
            {
                return Ok(ExecutionControl::Cancelled);
            }
        } else if let Some(delay) = &action.delay {
            let duration = delay.duration()?;
            if !wait_delay(duration, deadline, cancel).await? {
                return Ok(ExecutionControl::Cancelled);
            }
        } else if action.condition.is_some() {
            let condition = condition_from_action(action)?;
            if !condition_matches(runtime, template, &condition, variables)? {
                continue;
            }
        } else if let Some(choose) = &action.choose {
            let mut selected = None;
            for branch in choose {
                if branch.conditions.is_empty() {
                    return Err(invalid("choose branches need at least one condition"));
                }
                let mut matches = true;
                for condition in &branch.conditions {
                    if !condition_matches(runtime, template, condition, variables)? {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    selected = Some(branch.sequence.as_slice());
                    break;
                }
            }
            let sequence = selected.or(action.default.as_deref()).unwrap_or_default();
            let control = Box::pin(execute_actions(
                runtime,
                template,
                sequence,
                variables,
                lineage,
                context,
                cancel,
                deadline,
                depth + 1,
                executed,
            ))
            .await?;
            if control != ExecutionControl::Continue {
                return Ok(control);
            }
        } else if let Some(repeat) = &action.repeat {
            let control = Box::pin(execute_repeat(
                runtime,
                template,
                repeat,
                variables,
                lineage,
                context,
                cancel,
                deadline,
                depth + 1,
                executed,
            ))
            .await?;
            if control != ExecutionControl::Continue {
                return Ok(control);
            }
        } else if let Some(values) = &action.variables {
            for (key, value) in render_map(template, values, variables)? {
                variables.insert(key, value);
            }
        } else if let Some(message) = &action.stop {
            if action.error.unwrap_or(false) {
                return Err(Error::Service(if message.is_empty() {
                    "script stopped with an error".into()
                } else {
                    message.clone()
                }));
            }
            return Ok(ExecutionControl::Stop);
        }
    }
    Ok(ExecutionControl::Continue)
}

#[allow(clippy::too_many_arguments)]
async fn execute_repeat(
    runtime: &Runtime,
    template: &TemplateEngine,
    repeat: &RepeatConfig,
    variables: &mut Map<String, Value>,
    lineage: &[String],
    context: &Context,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
    depth: usize,
    executed: &mut usize,
) -> Result<ExecutionControl> {
    let limit = repeat.count.unwrap_or(MAX_REPEAT).min(MAX_REPEAT);
    for index in 0..limit {
        if *cancel.borrow() {
            return Ok(ExecutionControl::Cancelled);
        }
        if let Some(conditions) = &repeat.while_conditions
            && !conditions_match(runtime, template, conditions, variables)?
        {
            break;
        }
        variables.insert(
            "repeat".into(),
            json!({"index": index + 1, "first": index == 0, "last": index + 1 == limit}),
        );
        let control = Box::pin(execute_actions(
            runtime,
            template,
            &repeat.sequence,
            variables,
            lineage,
            context,
            cancel,
            deadline,
            depth,
            executed,
        ))
        .await?;
        if control != ExecutionControl::Continue {
            return Ok(control);
        }
        if let Some(conditions) = &repeat.until
            && conditions_match(runtime, template, conditions, variables)?
        {
            break;
        }
    }
    Ok(ExecutionControl::Continue)
}

async fn wait_delay(
    duration: Duration,
    deadline: Instant,
    cancel: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let end = Instant::now() + duration;
    if end > deadline {
        return Err(Error::Service(
            "script delay exceeds its run deadline".into(),
        ));
    }
    tokio::select! {
        _ = sleep_until(end) => Ok(true),
        _ = sleep_until(deadline) => Err(Error::Service("script exceeded its 60-second run limit".into())),
        _ = wait_cancelled(cancel) => Ok(false),
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() || cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn call_service_cancellable(
    runtime: Runtime,
    call: ServiceCall,
    lineage: Vec<String>,
    deadline: Instant,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<ServiceResult>> {
    let service_name = format!("{}.{}", call.domain, call.service);
    let mut handler =
        tokio::task::spawn_blocking(move || with_lineage(lineage, || runtime.call_service(call)));
    tokio::select! {
        result = &mut handler => match result {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(Error::Service(format!("service `{service_name}` task failed: {error}"))),
        },
        _ = sleep_until(deadline) => {
            tracing::warn!(service = %service_name, "script action exceeded run deadline; joining synchronous handler");
            let _ = handler.await;
            Err(Error::Service("script exceeded its 60-second run limit".into()))
        }
        _ = wait_cancelled(cancel) => {
            // Arbitrary registered synchronous handlers cannot be interrupted.
            let _ = handler.await;
            Ok(None)
        }
    }
}

fn condition_from_action(action: &Action) -> Result<Condition> {
    let condition = action.condition.as_deref().unwrap_or_default();
    match condition {
        "state" => Ok(Condition::State {
            entity_id: action
                .entity_id
                .clone()
                .ok_or_else(|| invalid("state condition requires entity_id"))?,
            state: action
                .state
                .clone()
                .ok_or_else(|| invalid("state condition requires state"))?,
        }),
        "numeric_state" => Ok(Condition::NumericState {
            entity_id: action
                .entity_id
                .clone()
                .ok_or_else(|| invalid("numeric_state condition requires entity_id"))?,
            above: action.above,
            below: action.below,
        }),
        "template" => Ok(Condition::Template {
            value_template: action
                .value_template
                .as_deref()
                .or(action.template.as_deref())
                .ok_or_else(|| invalid("template condition requires value_template"))?
                .to_owned(),
        }),
        "and" => Ok(Condition::And {
            conditions: action
                .conditions
                .clone()
                .ok_or_else(|| invalid("and condition requires conditions"))?,
        }),
        "or" => Ok(Condition::Or {
            conditions: action
                .conditions
                .clone()
                .ok_or_else(|| invalid("or condition requires conditions"))?,
        }),
        "not" => Ok(Condition::Not {
            conditions: action
                .conditions
                .clone()
                .ok_or_else(|| invalid("not condition requires conditions"))?,
        }),
        other => Err(invalid(format!("unsupported script condition `{other}`"))),
    }
}

fn conditions_match(
    runtime: &Runtime,
    template: &TemplateEngine,
    conditions: &[Condition],
    variables: &Map<String, Value>,
) -> Result<bool> {
    for condition in conditions {
        if !condition_matches(runtime, template, condition, variables)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn condition_matches(
    runtime: &Runtime,
    template: &TemplateEngine,
    condition: &Condition,
    variables: &Map<String, Value>,
) -> Result<bool> {
    match condition {
        Condition::State { entity_id, state } => {
            let ids = entity_id.values();
            if ids.is_empty() {
                return Ok(false);
            }
            Ok(ids.iter().all(|id| {
                runtime
                    .get_state(id)
                    .is_some_and(|current| current.state == *state)
            }))
        }
        Condition::NumericState {
            entity_id,
            above,
            below,
        } => {
            let ids = entity_id.values();
            if ids.is_empty() || (above.is_none() && below.is_none()) {
                return Ok(false);
            }
            Ok(ids.iter().all(|id| {
                runtime
                    .get_state(id)
                    .and_then(|state| state.state.parse::<f64>().ok())
                    .is_some_and(|value| {
                        above.is_none_or(|bound| value > bound)
                            && below.is_none_or(|bound| value < bound)
                    })
            }))
        }
        Condition::Template { value_template } => {
            let value = template.render_value(value_template, variables)?;
            Ok(truthy(&value))
        }
        Condition::And { conditions } => conditions_match(runtime, template, conditions, variables),
        Condition::Or { conditions } => {
            for child in conditions {
                if condition_matches(runtime, template, child, variables)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Condition::Not { conditions } => {
            let child = conditions
                .first()
                .ok_or_else(|| invalid("not condition needs one child"))?;
            Ok(!condition_matches(runtime, template, child, variables)?)
        }
    }
}

fn render_map(
    template: &TemplateEngine,
    values: &Map<String, Value>,
    variables: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(key, value)| Ok((key.clone(), render_value(template, value, variables)?)))
        .collect()
}

fn render_value(
    template: &TemplateEngine,
    value: &Value,
    variables: &Map<String, Value>,
) -> Result<Value> {
    match value {
        Value::String(text) if text.contains("{{") || text.contains("{%") => {
            template.render_value(text, variables)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| render_value(template, value, variables))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(values) => render_map(template, values, variables).map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => {
            !value.is_empty() && !matches!(value.as_str(), "false" | "False" | "0")
        }
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn register_services(
    runtime: &Runtime,
    commands: &async_mpsc::Sender<Command>,
    scripts: &BTreeMap<String, ScriptConfig>,
) -> Result<()> {
    for id in scripts.keys() {
        let sender = commands.clone();
        let script_id = id.clone();
        runtime.register_service(
            "script",
            id,
            json!({"name": scripts[id].alias.as_deref().unwrap_or(id)}),
            Arc::new(move |_runtime, call| {
                let variables = service_variables(call)?;
                let (reply, receiver) = mpsc::sync_channel(1);
                let request = StartRequest {
                    id: script_id.clone(),
                    variables,
                    ancestry: current_lineage(),
                    context: call.context.clone(),
                    wait: true,
                    reply,
                };
                dispatch(&sender, Command::Start(request), receiver)
            }),
        )?;
    }

    for (service, operation) in [
        ("turn_on", ScriptOperation::TurnOn),
        ("turn_off", ScriptOperation::TurnOff),
        ("toggle", ScriptOperation::Toggle),
    ] {
        let sender = commands.clone();
        runtime.register_service(
            "script",
            service,
            json!({"name": format!("Script {service}")}),
            Arc::new(move |_runtime, call| {
                let entity_ids = target_entity_ids(call)?;
                let variables = service_variables(call)?;
                let (reply, receiver) = mpsc::sync_channel(1);
                let command = Command::Control {
                    operation,
                    entity_ids,
                    variables,
                    ancestry: current_lineage(),
                    context: call.context.clone(),
                    reply,
                };
                dispatch(&sender, command, receiver)
            }),
        )?;
    }

    let sender = commands.clone();
    runtime.register_service(
        "scene",
        "turn_on",
        json!({"name":"Activate scene"}),
        Arc::new(move |_runtime, call| {
            let entity_ids = target_entity_ids(call)?;
            let transition = match call.data.get("transition") {
                None => None,
                Some(value) => {
                    let value = value
                        .as_f64()
                        .ok_or_else(|| invalid("scene transition must be numeric"))?;
                    if !value.is_finite() || !(0.0..=6553.0).contains(&value) {
                        return Err(invalid(
                            "scene transition must be between 0 and 6553 seconds",
                        ));
                    }
                    Some(value)
                }
            };
            let (reply, receiver) = mpsc::sync_channel(1);
            dispatch(
                &sender,
                Command::SceneTurnOn {
                    entity_ids,
                    ancestry: current_lineage(),
                    transition,
                    context: call.context.clone(),
                    reply,
                },
                receiver,
            )
        }),
    )?;

    let sender = commands.clone();
    runtime.register_service(
        "scene",
        "create",
        json!({"name":"Create scene"}),
        Arc::new(move |_runtime, call| {
            let (reply, receiver) = mpsc::sync_channel(1);
            dispatch(
                &sender,
                Command::SceneCreate {
                    data: call.data.clone(),
                    context: call.context.clone(),
                    reply,
                },
                receiver,
            )
        }),
    )?;
    Ok(())
}

fn dispatch(
    sender: &async_mpsc::Sender<Command>,
    command: Command,
    receiver: mpsc::Receiver<Result<ServiceResult>>,
) -> Result<ServiceResult> {
    sender.try_send(command).map_err(|error| {
        Error::Service(format!(
            "script actor is unavailable or its bounded queue is full: {error}"
        ))
    })?;
    match receiver.recv_timeout(SERVICE_REPLY_TIMEOUT) {
        Ok(result) => result,
        Err(error) => Err(timeout_error(error)),
    }
}

fn timeout_error(error: impl std::fmt::Display) -> Error {
    Error::Service(format!(
        "script service handler timed out or stopped: {error}"
    ))
}

fn target_entity_ids(call: &ServiceCall) -> Result<Vec<String>> {
    let value = call
        .target
        .get("entity_id")
        .or_else(|| call.data.get("entity_id"))
        .ok_or_else(|| invalid("service call requires target.entity_id"))?;
    let ids = match value {
        Value::String(value) => vec![value.clone()],
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("entity_id list must contain strings"))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(invalid(
                "target.entity_id must be a string or list of strings",
            ));
        }
    };
    if ids.is_empty() {
        return Err(invalid("target.entity_id must not be empty"));
    }
    Ok(ids)
}

fn service_variables(call: &ServiceCall) -> Result<Map<String, Value>> {
    match call.data.get("variables") {
        None => Ok(Map::new()),
        Some(Value::Object(values)) => Ok(values.clone()),
        Some(_) => Err(invalid("service variables must be an object")),
    }
}

fn initialize_script_state(runtime: &Runtime, id: &str, config: &ScriptConfig) -> Result<()> {
    set_script_state(runtime, id, config, 0, None, Context::default()).map(|_| ())
}

fn set_script_state(
    runtime: &Runtime,
    id: &str,
    config: &ScriptConfig,
    current: usize,
    last_triggered: Option<DateTime<Utc>>,
    context: Context,
) -> Result<State> {
    let mut attributes = Map::new();
    attributes.insert(
        "friendly_name".into(),
        Value::String(config.alias.clone().unwrap_or_else(|| id.replace('_', " "))),
    );
    attributes.insert("mode".into(), Value::String(mode_name(config.mode).into()));
    attributes.insert("current".into(), Value::from(current as u64));
    attributes.insert("max".into(), Value::from(config.max as u64));
    if let Some(description) = &config.description {
        attributes.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(icon) = &config.icon {
        attributes.insert("icon".into(), Value::String(icon.clone()));
    }
    if let Some(last_triggered) = last_triggered {
        attributes.insert(
            "last_triggered".into(),
            Value::String(last_triggered.to_rfc3339()),
        );
    }
    let state = if current > 0 { "on" } else { "off" };
    runtime
        .set_state(&format!("script.{id}"), state, attributes, context, false)
        .map(|change| change.state)
}

fn mode_name(mode: ScriptMode) -> &'static str {
    match mode {
        ScriptMode::Single => "single",
        ScriptMode::Restart => "restart",
        ScriptMode::Queued => "queued",
        ScriptMode::Parallel => "parallel",
    }
}

fn service_result(
    runtime: &Runtime,
    entity_id: &str,
    response: Option<Value>,
    context: Context,
) -> ServiceResult {
    ServiceResult {
        changed_states: runtime.get_state(entity_id).into_iter().collect(),
        response,
        context,
    }
}

fn initialize_scene_state(runtime: &Runtime, entity_id: &str, scene: &SceneConfig) -> Result<()> {
    let previous = runtime.get_state(entity_id);
    let mut attributes = scene_attributes(scene);
    if let Some(previous) = &previous
        && let Some(last_triggered) = previous.attributes.get("last_triggered")
    {
        attributes.insert("last_triggered".into(), last_triggered.clone());
    }
    runtime
        .set_state(
            entity_id,
            previous
                .as_ref()
                .map_or("unknown", |state| state.state.as_str()),
            attributes,
            Context::default(),
            false,
        )
        .map(|_| ())
}

fn scene_attributes(scene: &SceneConfig) -> Map<String, Value> {
    let mut attributes = Map::new();
    attributes.insert("friendly_name".into(), Value::String(scene.name.clone()));
    attributes.insert(
        "entity_id".into(),
        Value::Array(scene.entities.keys().cloned().map(Value::String).collect()),
    );
    if let Some(id) = &scene.id {
        attributes.insert("id".into(), Value::String(id.clone()));
    }
    if let Some(icon) = &scene.icon {
        attributes.insert("icon".into(), Value::String(icon.clone()));
    }
    attributes
}

fn load_dynamic_scenes(runtime: &Runtime) -> Result<BTreeMap<String, SceneConfig>> {
    let Some(value) = runtime.get_data(SCENE_METADATA_KEY)? else {
        return Ok(BTreeMap::new());
    };
    let scenes: BTreeMap<String, SceneConfig> = serde_json::from_value(value)
        .map_err(|error| Error::Storage(format!("invalid persisted dynamic scenes: {error}")))?;
    if scenes.len() > MAX_SCENES {
        return Err(Error::Storage(format!(
            "persisted dynamic scenes exceed {MAX_SCENES}"
        )));
    }
    for (object_id, scene) in &scenes {
        if !valid_object_id(object_id) || scene_object_id(&scene.name)? != *object_id {
            return Err(Error::Storage(format!(
                "persisted dynamic scene key `{object_id}` does not match its name"
            )));
        }
        validate_scene_config(scene, false)
            .map_err(|error| Error::Storage(format!("invalid persisted dynamic scene: {error}")))?;
    }
    Ok(scenes)
}

fn build_scene_map(
    static_scenes: &[SceneConfig],
    dynamic_scenes: &BTreeMap<String, SceneConfig>,
) -> Result<BTreeMap<String, StoredScene>> {
    let mut result = BTreeMap::new();
    for scene in static_scenes {
        let entity_id = format!("scene.{}", scene_object_id(&scene.name)?);
        result.insert(
            entity_id,
            StoredScene {
                config: scene.clone(),
                dynamic: false,
            },
        );
    }
    for (object_id, scene) in dynamic_scenes {
        let entity_id = format!("scene.{object_id}");
        if result.contains_key(&entity_id) {
            return Err(invalid(format!(
                "dynamic scene `{entity_id}` collides with configured scene"
            )));
        }
        result.insert(
            entity_id,
            StoredScene {
                config: scene.clone(),
                dynamic: true,
            },
        );
    }
    Ok(result)
}

async fn apply_scenes(
    runtime: Runtime,
    scenes: Arc<SceneStore>,
    entity_ids: Vec<String>,
    ancestry: Vec<String>,
    transition: Option<f64>,
    context: Context,
) -> Result<ServiceResult> {
    let mut changed_states = Vec::new();
    for entity_id in entity_ids {
        let stored = scenes
            .scenes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&entity_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("scene `{entity_id}`")))?;
        let now = Utc::now();
        let mut attributes = scene_attributes(&stored.config);
        attributes.insert("last_triggered".into(), Value::String(now.to_rfc3339()));
        let activation_time = now.to_rfc3339();
        let scene_state = runtime
            .set_state(
                &entity_id,
                &activation_time,
                attributes,
                context.clone(),
                true,
            )?
            .state;
        changed_states.push(scene_state);

        for (target_id, definition) in &stored.config.entities {
            let (state, attributes) = decode_scene_entity(definition)?;
            let call = scene_service_call(
                target_id,
                &state,
                attributes,
                transition,
                context.clone(),
                &runtime,
            )?;
            let runtime_for_call = runtime.clone();
            let service_lineage = ancestry.clone();
            let applied = tokio::task::spawn_blocking(move || {
                with_lineage(service_lineage, || runtime_for_call.call_service(call))
            })
            .await
            .map_err(|error| {
                Error::Service(format!("scene entity service task failed: {error}"))
            })??;
            changed_states.extend(applied.changed_states);
        }
    }
    Ok(ServiceResult {
        changed_states,
        response: None,
        context,
    })
}

fn scene_service_call(
    entity_id: &str,
    desired_state: &str,
    mut attributes: Map<String, Value>,
    transition: Option<f64>,
    context: Context,
    runtime: &Runtime,
) -> Result<ServiceCall> {
    let (domain, _) = entity_id
        .split_once('.')
        .ok_or_else(|| invalid(format!("invalid scene entity ID `{entity_id}`")))?;
    if domain == "script" {
        return Err(unsupported_scene_entity(entity_id, desired_state));
    }
    let available = runtime.services();
    let has_service = |service: &str| {
        available
            .get(domain)
            .and_then(Value::as_object)
            .is_some_and(|services| services.contains_key(service))
    };
    let (service, value_field, value) = match domain {
        "input_boolean" | "switch" | "light" | "fan" | "climate" | "media_player" => {
            if desired_state == "on" {
                ("turn_on", None, None)
            } else if desired_state == "off" {
                ("turn_off", None, None)
            } else {
                return Err(unsupported_scene_entity(entity_id, desired_state));
            }
        }
        "cover" if desired_state == "open" || desired_state == "opening" => {
            ("open_cover", None, None)
        }
        "cover" if desired_state == "closed" || desired_state == "closing" => {
            ("close_cover", None, None)
        }
        "lock" if desired_state == "locked" => ("lock", None, None),
        "lock" if desired_state == "unlocked" => ("unlock", None, None),
        "input_number" | "counter" => (
            "set_value",
            Some("value"),
            Some(parse_number(entity_id, desired_state)?),
        ),
        "input_text" => (
            "set_value",
            Some("value"),
            Some(Value::String(desired_state.to_owned())),
        ),
        "input_select" => (
            "select_option",
            Some("option"),
            Some(Value::String(desired_state.to_owned())),
        ),
        _ if desired_state == "on" && has_service("turn_on") => ("turn_on", None, None),
        _ if desired_state == "off" && has_service("turn_off") => ("turn_off", None, None),
        _ if has_service("set_value") => (
            "set_value",
            Some("value"),
            Some(Value::String(desired_state.to_owned())),
        ),
        _ => return Err(unsupported_scene_entity(entity_id, desired_state)),
    };
    if !has_service(service) {
        return Err(Error::Service(format!(
            "scene cannot restore `{entity_id}`: required service `{domain}.{service}` is unavailable"
        )));
    }
    attributes.remove("state");
    for key in [
        "friendly_name",
        "icon",
        "supported_features",
        "entity_id",
        "device_class",
    ] {
        attributes.remove(key);
    }
    let allowed_attributes: &[&str] = match domain {
        "light" => &[
            "brightness",
            "color_temp",
            "color_temp_kelvin",
            "hs_color",
            "xy_color",
            "rgb_color",
            "rgbw_color",
            "rgbww_color",
            "white",
            "effect",
        ],
        "fan" => &[
            "percentage",
            "preset_mode",
            "oscillating",
            "direction",
            "percentage_step",
        ],
        "climate" => &[
            "temperature",
            "target_temp_high",
            "target_temp_low",
            "hvac_mode",
            "fan_mode",
            "swing_mode",
            "preset_mode",
            "auxiliary_heat",
        ],
        "media_player" => &[
            "volume_level",
            "is_volume_muted",
            "media_content_id",
            "media_content_type",
            "source",
            "shuffle",
            "repeat",
        ],
        "cover" => &[
            "current_position",
            "position",
            "current_tilt_position",
            "tilt_position",
        ],
        _ => &[],
    };
    attributes.retain(|key, _| allowed_attributes.contains(&key.as_str()));
    let mut data = attributes;
    if domain == "light"
        && let Some(transition) = transition
    {
        data.insert("transition".into(), Value::from(transition));
    }
    if let (Some(field), Some(value)) = (value_field, value) {
        data.insert(field.into(), value);
    }
    Ok(ServiceCall {
        domain: domain.to_owned(),
        service: service.to_owned(),
        data,
        target: Map::from_iter([("entity_id".into(), Value::String(entity_id.to_owned()))]),
        context,
    })
}

fn unsupported_scene_entity(entity_id: &str, state: &str) -> Error {
    Error::Service(format!(
        "scene cannot restore `{entity_id}` to `{state}`: no supported entity service is available"
    ))
}

fn parse_number(entity_id: &str, state: &str) -> Result<Value> {
    let value = state.parse::<f64>().map_err(|_| {
        Error::Service(format!(
            "scene value `{state}` for `{entity_id}` is not numeric"
        ))
    })?;
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| Error::Service(format!("scene value for `{entity_id}` is not finite")))
}

fn create_scene(
    runtime: &Runtime,
    store: &SceneStore,
    data: Map<String, Value>,
    context: Context,
) -> Result<ServiceResult> {
    if let Some(key) = data
        .keys()
        .find(|key| !matches!(key.as_str(), "scene_id" | "entities" | "snapshot_entities"))
    {
        return Err(invalid(format!("unsupported scene.create field `{key}`")));
    }
    let scene_id = data
        .get("scene_id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("scene.create requires scene_id"))?;
    if !valid_object_id(scene_id) {
        return Err(invalid(format!("scene_id `{scene_id}` is invalid")));
    }
    let mut entities = match data.get("entities") {
        None => BTreeMap::new(),
        Some(Value::Object(values)) => values
            .iter()
            .map(|(id, value)| (id.clone(), value.clone()))
            .collect(),
        Some(_) => return Err(invalid("scene.create entities must be an object")),
    };
    let snapshots = match data.get("snapshot_entities") {
        None => Vec::new(),
        Some(Value::String(id)) => vec![id.clone()],
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("snapshot_entities must contain strings"))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(invalid(
                "snapshot_entities must be a string or list of strings",
            ));
        }
    };
    if entities.is_empty() && snapshots.is_empty() {
        return Err(invalid(
            "scene.create requires entities or snapshot_entities",
        ));
    }
    for entity_id in snapshots {
        if entities.contains_key(&entity_id) {
            return Err(invalid(format!(
                "scene entity `{entity_id}` appears in entities and snapshot_entities"
            )));
        }
        if let Some(state) = runtime.get_state(&entity_id) {
            let mut definition = state.attributes.clone();
            definition.insert("state".into(), Value::String(state.state));
            entities.insert(entity_id, Value::Object(definition));
        }
    }
    let scene = SceneConfig {
        id: None,
        name: scene_id.to_owned(),
        icon: None,
        metadata: Map::new(),
        entities,
    };
    validate_scene_config(&scene, false)?;
    let object_id = scene_object_id(&scene.name)?;
    let entity_id = format!("scene.{object_id}");

    let current_scene = store
        .scenes
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&entity_id)
        .cloned();
    if current_scene
        .as_ref()
        .is_some_and(|existing| !existing.dynamic)
    {
        return Err(Error::Service(format!(
            "configured scene `{entity_id}` cannot be replaced"
        )));
    }
    let mut dynamic = store
        .dynamic
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut updated = dynamic.clone();
    updated.insert(object_id.clone(), scene.clone());
    if updated.len() > MAX_SCENES {
        return Err(Error::Service(format!(
            "dynamic scene limit ({MAX_SCENES}) reached"
        )));
    }
    persist_dynamic_scenes(runtime, &updated)?;
    *dynamic = updated;
    store
        .scenes
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            entity_id.clone(),
            StoredScene {
                config: scene.clone(),
                dynamic: true,
            },
        );

    initialize_scene_state(runtime, &entity_id, &scene)?;
    Ok(ServiceResult {
        changed_states: runtime.get_state(&entity_id).into_iter().collect(),
        response: Some(json!({"entity_id": entity_id})),
        context,
    })
}

fn persist_dynamic_scenes(runtime: &Runtime, scenes: &BTreeMap<String, SceneConfig>) -> Result<()> {
    let value = serde_json::to_value(scenes)
        .map_err(|error| Error::Storage(format!("cannot encode dynamic scenes: {error}")))?;
    runtime.set_data(SCENE_METADATA_KEY, &value)
}

fn decode_scene_entity(value: &Value) -> Result<(String, Map<String, Value>)> {
    match value {
        Value::String(state) => Ok((state.clone(), Map::new())),
        Value::Bool(state) => Ok((if *state { "on" } else { "off" }.into(), Map::new())),
        Value::Object(attributes) => {
            let state = attributes
                .get("state")
                .ok_or_else(|| invalid("scene entity mapping must include state"))?;
            let state = match state {
                Value::String(value) => value.clone(),
                Value::Bool(value) => if *value { "on" } else { "off" }.into(),
                _ => return Err(invalid("scene state must be a string or boolean")),
            };
            let mut attributes = attributes.clone();
            attributes.remove("state");
            Ok((state, attributes))
        }
        _ => Err(invalid(
            "scene entity value must be a state string or object",
        )),
    }
}

fn validate_scene_config(scene: &SceneConfig, allow_empty: bool) -> Result<()> {
    if scene.name.trim().is_empty() || scene.name.len() > 128 {
        return Err(invalid("scene name must contain 1 to 128 characters"));
    }
    if scene
        .icon
        .as_ref()
        .is_some_and(|icon| icon.trim().is_empty())
    {
        return Err(invalid("scene icon must not be empty"));
    }
    if !allow_empty && scene.entities.is_empty() {
        return Err(invalid(format!(
            "scene `{}` needs at least one entity",
            scene.name
        )));
    }
    if scene.entities.len() > MAX_SCENE_ENTITIES {
        return Err(invalid(format!(
            "scene `{}` exceeds the {MAX_SCENE_ENTITIES}-entity limit",
            scene.name
        )));
    }
    for (entity_id, definition) in &scene.entities {
        if !valid_entity_id(entity_id) {
            return Err(invalid(format!(
                "scene `{}` has invalid entity ID `{entity_id}`",
                scene.name
            )));
        }
        let (state, _) = decode_scene_entity(definition)?;
        if state.trim().is_empty() || state.len() > 256 {
            return Err(invalid(format!(
                "scene state for `{entity_id}` is empty or too long"
            )));
        }
    }
    Ok(())
}

fn validate_actions(actions: &[Action], depth: usize, total: &mut usize) -> Result<()> {
    if depth > MAX_NESTING {
        return Err(invalid(format!(
            "nested action depth exceeds {MAX_NESTING}"
        )));
    }
    if actions.len() > MAX_SEQUENCE_ACTIONS {
        return Err(invalid(format!(
            "sequence exceeds {MAX_SEQUENCE_ACTIONS} actions"
        )));
    }
    for action in actions {
        *total += 1;
        if *total > MAX_ACTIONS {
            return Err(invalid(format!(
                "configuration exceeds {MAX_ACTIONS} total actions"
            )));
        }
        let count = action.kind_count();
        if count != 1 {
            return Err(invalid(
                "each script step must specify exactly one action kind",
            ));
        }
        if action
            .alias
            .as_ref()
            .is_some_and(|alias| alias.trim().is_empty())
        {
            return Err(invalid("action alias must not be empty"));
        }
        if let Some(service) = action.action.as_deref().or(action.service.as_deref()) {
            if !valid_service_name(service) {
                return Err(invalid(format!("invalid service name `{service}`")));
            }
            if action.data.is_some() && action.service_data.is_some() {
                return Err(invalid(
                    "service step cannot use both data and service_data",
                ));
            }
            reject_unused_action_fields(
                action,
                &["action", "service", "data", "service_data", "target"],
            )?;
        } else if let Some(delay) = &action.delay {
            delay.duration()?;
            reject_unused_action_fields(action, &["delay"])?;
        } else if action.condition.is_some() {
            let condition = condition_from_action(action)?;
            validate_condition_value(&condition)?;
            let mut allowed = vec!["condition"];
            match action.condition.as_deref().unwrap_or_default() {
                "state" => allowed.extend(["entity_id", "state"]),
                "numeric_state" => allowed.extend(["entity_id", "above", "below"]),
                "template" => allowed.extend(["value_template", "template"]),
                "and" | "or" | "not" => allowed.push("conditions"),
                _ => {}
            }
            reject_unused_action_fields(action, &allowed)?;
        } else if let Some(choose) = &action.choose {
            if choose.is_empty() || choose.len() > 32 {
                return Err(invalid("choose must have 1 to 32 branches"));
            }
            reject_unused_action_fields(action, &["choose", "default"])?;
            for branch in choose {
                if branch.conditions.is_empty() || branch.conditions.len() > 32 {
                    return Err(invalid("choose branches need 1 to 32 conditions"));
                }
                for condition in &branch.conditions {
                    validate_condition_value(condition)?;
                }
                validate_actions(&branch.sequence, depth + 1, total)?;
            }
            if let Some(default) = &action.default {
                validate_actions(default, depth + 1, total)?;
            }
        } else if let Some(repeat) = &action.repeat {
            reject_unused_action_fields(action, &["repeat"])?;
            let guards = usize::from(repeat.count.is_some())
                + usize::from(repeat.while_conditions.is_some())
                + usize::from(repeat.until.is_some());
            if guards != 1 {
                return Err(invalid(
                    "repeat must specify exactly one of count, while, or until",
                ));
            }
            if repeat
                .count
                .is_some_and(|count| count == 0 || count > MAX_REPEAT)
            {
                return Err(invalid(format!(
                    "repeat count must be between 1 and {MAX_REPEAT}"
                )));
            }
            if repeat.sequence.is_empty() {
                return Err(invalid("repeat sequence must not be empty"));
            }
            for conditions in [&repeat.while_conditions, &repeat.until]
                .into_iter()
                .flatten()
            {
                if conditions.is_empty() || conditions.len() > 32 {
                    return Err(invalid("repeat guards need 1 to 32 conditions"));
                }
                for condition in conditions {
                    validate_condition_value(condition)?;
                }
            }
            validate_actions(&repeat.sequence, depth + 1, total)?;
        } else if let Some(_variables) = &action.variables {
            reject_unused_action_fields(action, &["variables"])?;
        } else if action.stop.is_some() {
            reject_unused_action_fields(action, &["stop", "error"])?;
            if action
                .stop
                .as_ref()
                .is_some_and(|message| message.len() > 1024)
            {
                return Err(invalid("stop message is too long"));
            }
        }
    }
    Ok(())
}

impl Action {
    fn kind_count(&self) -> usize {
        usize::from(self.action.is_some())
            + usize::from(self.service.is_some())
            + usize::from(self.delay.is_some())
            + usize::from(self.condition.is_some())
            + usize::from(self.choose.is_some())
            + usize::from(self.repeat.is_some())
            + usize::from(self.variables.is_some())
            + usize::from(self.stop.is_some())
    }
}

fn reject_unused_action_fields(action: &Action, allowed: &[&str]) -> Result<()> {
    let fields = [
        ("action", action.action.is_some()),
        ("service", action.service.is_some()),
        ("data", action.data.is_some()),
        ("service_data", action.service_data.is_some()),
        ("target", action.target.is_some()),
        ("delay", action.delay.is_some()),
        ("condition", action.condition.is_some()),
        ("entity_id", action.entity_id.is_some()),
        ("state", action.state.is_some()),
        ("above", action.above.is_some()),
        ("below", action.below.is_some()),
        ("value_template", action.value_template.is_some()),
        ("template", action.template.is_some()),
        ("conditions", action.conditions.is_some()),
        ("choose", action.choose.is_some()),
        ("default", action.default.is_some()),
        ("repeat", action.repeat.is_some()),
        ("variables", action.variables.is_some()),
        ("stop", action.stop.is_some()),
        ("error", action.error.is_some()),
    ];
    if let Some((field, _)) = fields
        .iter()
        .find(|(field, present)| *present && !allowed.contains(field))
    {
        return Err(invalid(format!(
            "field `{field}` is not supported for this script step"
        )));
    }
    Ok(())
}

fn validate_condition_value(condition: &Condition) -> Result<()> {
    let mut total = 0;
    validate_condition(condition, 0, &mut total)
}

fn validate_condition(condition: &Condition, depth: usize, total: &mut usize) -> Result<()> {
    if depth > MAX_NESTING {
        return Err(invalid("condition nesting limit reached"));
    }
    *total += 1;
    if *total > MAX_CONDITION_NODES {
        return Err(invalid(format!(
            "condition count exceeds {MAX_CONDITION_NODES}"
        )));
    }
    match condition {
        Condition::State { entity_id, state } => {
            validate_entity_ids(entity_id)?;
            if state.trim().is_empty() {
                return Err(invalid("state condition state must not be empty"));
            }
        }
        Condition::NumericState {
            entity_id,
            above,
            below,
        } => {
            validate_entity_ids(entity_id)?;
            if above.is_none() && below.is_none() {
                return Err(invalid("numeric_state needs above or below"));
            }
            if above.is_some_and(|value| !value.is_finite())
                || below.is_some_and(|value| !value.is_finite())
            {
                return Err(invalid("numeric_state bounds must be finite"));
            }
        }
        Condition::Template { value_template } if value_template.trim().is_empty() => {
            return Err(invalid("template condition must not be empty"));
        }
        Condition::And { conditions } | Condition::Or { conditions } => {
            if conditions.is_empty() || conditions.len() > 32 {
                return Err(invalid("and/or condition needs 1 to 32 children"));
            }
            for child in conditions {
                validate_condition(child, depth + 1, total)?;
            }
        }
        Condition::Not { conditions } => {
            if conditions.len() != 1 {
                return Err(invalid("not condition needs exactly one child"));
            }
            validate_condition(&conditions[0], depth + 1, total)?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_entity_ids(ids: &EntityIds) -> Result<()> {
    let values = ids.values();
    if values.is_empty() || values.iter().any(|id| !valid_entity_id(id)) {
        return Err(invalid("entity_id must contain valid entity IDs"));
    }
    Ok(())
}

fn scene_object_id(name: &str) -> Result<String> {
    let slug = slugify(name);
    if !valid_object_id(&slug) {
        return Err(invalid(format!(
            "scene name `{name}` cannot produce a valid object ID"
        )));
    }
    Ok(slug)
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.trim().to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('_');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    output
}

fn valid_object_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_entity_id(value: &str) -> bool {
    let Some((domain, object_id)) = value.split_once('.') else {
        return false;
    };
    valid_object_id(domain) && valid_object_id(object_id) && !object_id.contains('.')
}

fn valid_service_name(value: &str) -> bool {
    let Some((domain, service)) = value.split_once('.') else {
        return false;
    };
    valid_object_id(domain) && valid_object_id(service) && !service.contains('.')
}

fn parse_clock_duration(text: &str) -> Result<f64> {
    let fields: Vec<_> = text.split(':').collect();
    if fields.len() != 3 {
        return Err(invalid("text delay must use HH:MM:SS"));
    }
    let hours = fields[0]
        .parse::<u64>()
        .map_err(|_| invalid("delay hours must be a nonnegative integer"))?;
    let minutes = fields[1]
        .parse::<u64>()
        .map_err(|_| invalid("delay minutes must be a nonnegative integer"))?;
    let seconds = fields[2]
        .parse::<f64>()
        .map_err(|_| invalid("delay seconds must be a nonnegative number"))?;
    if minutes >= 60 || seconds >= 60.0 || !seconds.is_finite() || seconds < 0.0 {
        return Err(invalid("delay minutes and seconds must be below 60"));
    }
    Ok(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
}

fn current_lineage() -> Vec<String> {
    SCRIPT_LINEAGE.with(|lineage| lineage.borrow().clone())
}

fn with_lineage<T>(lineage: Vec<String>, action: impl FnOnce() -> T) -> T {
    SCRIPT_LINEAGE.with(|current| {
        let previous = std::mem::replace(&mut *current.borrow_mut(), lineage);
        let result = action();
        *current.borrow_mut() = previous;
        result
    })
}

fn reply_error(waiter: Option<SyncSender<Result<ServiceResult>>>, error: Error) {
    if let Some(waiter) = waiter {
        let _ = waiter.send(Err(error));
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;
    use std::sync::atomic::AtomicUsize;

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn script(mode: ScriptMode, sequence: Value) -> ScriptConfig {
        serde_json::from_value(json!({
            "mode": mode_name(mode),
            "max": 4,
            "sequence": sequence,
        }))
        .unwrap()
    }

    fn call(
        runtime: &Runtime,
        domain: &str,
        service: &str,
        data: Value,
        target: Value,
    ) -> Result<ServiceResult> {
        runtime.call_service(ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: target.as_object().cloned().unwrap_or_default(),
            context: Context::default(),
        })
    }

    fn target(entity_id: &str) -> Value {
        json!({"entity_id": entity_id})
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..300 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition did not become true before timeout");
    }

    #[tokio::test]
    async fn strict_schema_and_pure_validation_reject_unsupported_constructs() {
        let unknown: std::result::Result<ScriptConfig, _> = serde_json::from_value(json!({
            "sequence": [{"for_each": ["a", "b"]}]
        }));
        assert!(unknown.is_err());

        let config = script(ScriptMode::Single, json!([{"delay": 31}]));
        let scripts = BTreeMap::from([("bad_delay".into(), config)]);
        assert!(validate_config(&scripts, &[]).is_err());

        let invalid_scene = SceneConfig {
            id: None,
            name: "Empty scene".into(),
            icon: None,
            metadata: Map::new(),
            entities: BTreeMap::new(),
        };
        assert!(validate_config(&BTreeMap::new(), &[invalid_scene]).is_err());
    }

    #[tokio::test]
    async fn scheduling_modes_enforce_single_restart_queue_parallel_and_cancel() {
        let runtime = runtime();
        let recorded = Arc::new(AtomicUsize::new(0));
        let active_calls = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let record_target = Arc::clone(&recorded);
        runtime
            .register_service(
                "test",
                "record",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    record_target.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: call.data.get("value").cloned(),
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let active_target = Arc::clone(&active_calls);
        let max_target = Arc::clone(&max_active);
        runtime
            .register_service(
                "test",
                "block",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    let current = active_target.fetch_add(1, Ordering::SeqCst) + 1;
                    max_target.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    active_target.fetch_sub(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();

        let delayed_record = json!([
            {"delay": "00:00:00.04"},
            {"action": "test.record"}
        ]);
        let mut scripts = BTreeMap::new();
        scripts.insert(
            "single_run".into(),
            script(ScriptMode::Single, delayed_record.clone()),
        );
        scripts.insert(
            "restart_run".into(),
            script(ScriptMode::Restart, delayed_record.clone()),
        );
        scripts.insert(
            "queued_run".into(),
            script(ScriptMode::Queued, delayed_record.clone()),
        );
        scripts.insert(
            "parallel_run".into(),
            script(ScriptMode::Parallel, json!([{"action":"test.block"}])),
        );
        scripts.insert(
            "cancel_run".into(),
            script(ScriptMode::Single, json!([{"delay":"00:00:10"}])),
        );
        let engine = ScriptEngine::start(runtime.clone(), scripts, Vec::new()).unwrap();

        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.single_run"),
        )
        .unwrap();
        let second_single = call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.single_run"),
        );
        assert!(
            matches!(second_single, Err(Error::Service(message)) if message.contains("single mode"))
        );
        wait_until(|| {
            runtime
                .get_state("script.single_run")
                .is_some_and(|state| state.state == "off")
        })
        .await;

        let before_restart = recorded.load(Ordering::SeqCst);
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.restart_run"),
        )
        .unwrap();
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.restart_run"),
        )
        .unwrap();
        wait_until(|| {
            runtime
                .get_state("script.restart_run")
                .is_some_and(|state| state.state == "off")
                && recorded.load(Ordering::SeqCst) > before_restart
        })
        .await;
        assert_eq!(recorded.load(Ordering::SeqCst), before_restart + 1);

        let before_queue = recorded.load(Ordering::SeqCst);
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.queued_run"),
        )
        .unwrap();
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.queued_run"),
        )
        .unwrap();
        wait_until(|| recorded.load(Ordering::SeqCst) >= before_queue + 2).await;

        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.parallel_run"),
        )
        .unwrap();
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.parallel_run"),
        )
        .unwrap();
        wait_until(|| max_active.load(Ordering::SeqCst) >= 2).await;

        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.cancel_run"),
        )
        .unwrap();
        call(
            &runtime,
            "script",
            "turn_off",
            Value::Null,
            target("script.cancel_run"),
        )
        .unwrap();
        wait_until(|| {
            runtime
                .get_state("script.cancel_run")
                .is_some_and(|state| state.state == "off")
        })
        .await;
        call(
            &runtime,
            "script",
            "turn_on",
            Value::Null,
            target("script.cancel_run"),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), engine.shutdown())
            .await
            .expect("shutdown should cancel the active delay")
            .unwrap();
        assert_eq!(runtime.get_state("script.cancel_run").unwrap().state, "off");
    }

    #[tokio::test]
    async fn actions_cover_conditions_choose_repeat_variables_stop_and_service_errors() {
        let runtime = runtime();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let capture_target = Arc::clone(&captured);
        runtime
            .register_service(
                "test",
                "capture",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    capture_target
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(call.data.get("value").cloned().unwrap_or(Value::Null));
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        runtime
            .set_state(
                "binary_sensor.ready",
                "on",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();

        let action_script: ScriptConfig = serde_json::from_value(json!({
            "variables": {"enabled": true, "label": "welcome"},
            "sequence": [
                {"condition":"state", "entity_id":"binary_sensor.ready", "state":"on"},
                {"choose":[{"conditions":[{"condition":"template", "value_template":"{{ enabled }}"}], "sequence":[{"action":"test.capture", "data":{"value":"{{ label }}"}}]}]},
                {"repeat":{"count":2, "sequence":[{"action":"test.capture", "data":{"value":"{{ repeat.index }}"}}]}},
                {"stop":"finish"},
                {"action":"test.capture", "data":{"value":"unreachable"}}
            ]
        }))
        .unwrap();
        let bad_service = script(ScriptMode::Single, json!([{"action":"missing.nope"}]));
        let scripts = BTreeMap::from([
            ("actions".into(), action_script),
            ("bad_service".into(), bad_service),
        ]);
        let engine = ScriptEngine::start(runtime.clone(), scripts, Vec::new()).unwrap();
        call(&runtime, "script", "actions", Value::Null, Value::Null).unwrap();
        let values = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(values, vec![json!("welcome"), json!(1), json!(2)]);
        let error = call(&runtime, "script", "bad_service", Value::Null, Value::Null).unwrap_err();
        assert!(
            matches!(error, Error::Service(message) if message.contains("unknown service missing.nope"))
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recursion_is_rejected_without_deadlocking_actor() {
        let runtime = runtime();
        let loop_script = script(ScriptMode::Single, json!([{"action":"script.loop_self"}]));
        let scripts = BTreeMap::from([("loop_self".into(), loop_script)]);
        let engine = ScriptEngine::start(runtime.clone(), scripts, Vec::new()).unwrap();
        let error = call(&runtime, "script", "loop_self", Value::Null, Value::Null).unwrap_err();
        assert!(
            matches!(error, Error::Service(message) if message.contains("recursion loop detected"))
        );
        engine.shutdown().await.unwrap();
    }

    fn register_input_boolean(runtime: &Runtime) {
        runtime
            .set_state(
                "input_boolean.lamp",
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        for (service, state) in [("turn_on", "on"), ("turn_off", "off")] {
            runtime
                .register_service(
                    "input_boolean",
                    service,
                    Value::Null,
                    Arc::new(move |runtime, call| {
                        let ids = target_entity_ids(call)?;
                        let mut changed_states = Vec::new();
                        for entity_id in ids {
                            let attributes = runtime
                                .get_state(&entity_id)
                                .map_or_else(Map::new, |state| state.attributes);
                            changed_states.push(
                                runtime
                                    .set_state(
                                        &entity_id,
                                        state,
                                        attributes,
                                        call.context.clone(),
                                        false,
                                    )?
                                    .state,
                            );
                        }
                        Ok(ServiceResult {
                            changed_states,
                            response: None,
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();
        }
    }

    #[tokio::test]
    async fn scenes_use_services_snapshot_metadata_and_restore_after_restart() {
        let runtime = runtime();
        register_input_boolean(&runtime);
        let configured = SceneConfig {
            id: Some("reading_corner_unique".into()),
            name: "Reading corner".into(),
            icon: Some("mdi:book-open".into()),
            metadata: Map::new(),
            entities: BTreeMap::from([("input_boolean.lamp".into(), json!("on"))]),
        };
        let first =
            ScriptEngine::start(runtime.clone(), BTreeMap::new(), vec![configured]).unwrap();
        call(
            &runtime,
            "scene",
            "turn_on",
            Value::Null,
            target("scene.reading_corner"),
        )
        .unwrap();
        assert_eq!(runtime.get_state("input_boolean.lamp").unwrap().state, "on");

        call(
            &runtime,
            "input_boolean",
            "turn_on",
            Value::Null,
            target("input_boolean.lamp"),
        )
        .unwrap();
        let created = call(
            &runtime,
            "scene",
            "create",
            json!({"scene_id":"snapshot","snapshot_entities":["input_boolean.lamp"]}),
            Value::Null,
        )
        .unwrap();
        assert_eq!(
            created.response,
            Some(json!({"entity_id":"scene.snapshot"}))
        );
        assert!(runtime.get_data(SCENE_METADATA_KEY).unwrap().is_some());
        call(
            &runtime,
            "input_boolean",
            "turn_off",
            Value::Null,
            target("input_boolean.lamp"),
        )
        .unwrap();
        call(
            &runtime,
            "scene",
            "turn_on",
            Value::Null,
            target("scene.snapshot"),
        )
        .unwrap();
        assert_eq!(runtime.get_state("input_boolean.lamp").unwrap().state, "on");
        first.shutdown().await.unwrap();

        call(
            &runtime,
            "input_boolean",
            "turn_off",
            Value::Null,
            target("input_boolean.lamp"),
        )
        .unwrap();
        let second = ScriptEngine::start(runtime.clone(), BTreeMap::new(), Vec::new()).unwrap();
        call(
            &runtime,
            "scene",
            "turn_on",
            Value::Null,
            target("scene.snapshot"),
        )
        .unwrap();
        assert_eq!(runtime.get_state("input_boolean.lamp").unwrap().state, "on");
        second.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scene_without_a_real_entity_service_fails_instead_of_injecting_state() {
        let runtime = runtime();
        let scene = SceneConfig {
            id: None,
            name: "Unsupported light".into(),
            icon: None,
            metadata: Map::new(),
            entities: BTreeMap::from([("light.missing".into(), json!("on"))]),
        };
        let engine = ScriptEngine::start(runtime.clone(), BTreeMap::new(), vec![scene]).unwrap();
        let error = call(
            &runtime,
            "scene",
            "turn_on",
            Value::Null,
            target("scene.unsupported_light"),
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::Service(message) if message.contains("cannot restore `light.missing`"))
        );
        assert!(runtime.get_state("light.missing").is_none());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shared_sequence_api_preserves_context_and_validates_values() {
        let runtime = runtime();
        let seen_context = Arc::new(Mutex::new(None::<Context>));
        let target_context = Arc::clone(&seen_context);
        runtime
            .register_service(
                "test",
                "context",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    *target_context
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(call.context.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let sequence = json!([{"action":"test.context"}]);
        validate_sequence(&sequence).unwrap();
        let context = Context {
            id: "cause-id".into(),
            parent_id: Some("parent-id".into()),
            user_id: Some("user-id".into()),
        };
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        run_sequence(runtime, sequence, Map::new(), context.clone(), cancel_rx)
            .await
            .unwrap();
        assert_eq!(
            *seen_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(context)
        );
    }
}
