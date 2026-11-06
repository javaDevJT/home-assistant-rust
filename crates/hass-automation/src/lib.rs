//! A strict, bounded subset of Home Assistant automations.
//!
//! The crate accepts YAML deserialized as `Vec<Automation>`, then validates
//! and runs it against [`hass_core::Runtime`]. Supported triggers are `state`
//! and `event`; supported conditions are `state` and `numeric_state`; actions
//! are service calls and delays of at most 30 seconds. Unknown options are
//! rejected. See the crate README for complete YAML examples and limits.
//!
//! ```yaml
//! - id: toggle_demo
//!   triggers:
//!     - trigger: event
//!       event_type: demo_toggle
//!   actions:
//!     - action: input_boolean.toggle
//!       target:
//!         entity_id: input_boolean.demo
//! ```

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hass_core::{Context, Error, Event, Result, Runtime, ServiceCall, State};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::{
    task::{JoinHandle, JoinSet},
    time::Instant as TokioInstant,
};
use uuid::Uuid;

const MAX_TRIGGERS: usize = 16;
const MAX_CONDITIONS: usize = 16;
const MAX_ACTIONS: usize = 32;
const MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_RUN_TIME: Duration = Duration::from_secs(60);
const MAX_ACTIVE_RUNS: usize = 16;
const MAX_PENDING_RUNS: usize = 128;
const MAX_LINEAGE_DEPTH: usize = 32;
const MAX_LINEAGE_ENTRIES: usize = 4096;
const LINEAGE_TTL: Duration = Duration::from_secs(5 * 60);

/// A configured automation. All fields are strict: unrecognized YAML keys
/// produce a deserialization error instead of being ignored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Automation {
    /// Stable identifier used for loop prevention.
    pub id: String,
    /// Optional display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// One or more trigger definitions. Any trigger may start this automation.
    pub triggers: Vec<Trigger>,
    /// Conditions are ANDed. An empty list passes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Actions execute sequentially.
    pub actions: Vec<Action>,
}

/// Supported event or state trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// Match a change in the state's value for any configured entity.
    State {
        entity_id: EntityIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// Match an event type and a top-level subset of its data.
    Event {
        event_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_data: Option<Map<String, Value>>,
    },
}

/// One entity ID or a list of entity IDs accepted by a state trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntityIds {
    One(String),
    Many(Vec<String>),
}

impl EntityIds {
    fn as_slice(&self) -> Vec<&str> {
        match self {
            Self::One(id) => vec![id],
            Self::Many(ids) => ids.iter().map(String::as_str).collect(),
        }
    }
}

/// Supported state or numeric-state condition.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    /// Require the current state string to equal `state`.
    State { entity_id: String, state: String },
    /// Require a numeric state to be strictly above and/or below its bounds.
    NumericState {
        entity_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
    },
}

/// A service call or bounded delay.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    Service(ServiceAction),
    Delay(DelayAction),
}

/// Service action in modern `action: domain.service` form.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAction {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Map<String, Value>>,
}

/// Delay action in the familiar `delay: ...` form.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayAction {
    pub delay: DelayValue,
}

/// Delay duration as seconds, `HH:MM:SS` text, or a duration object.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DelayValue {
    Seconds(f64),
    Text(String),
    Parts(DelayParts),
}

/// Components accepted in a delay duration object.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayParts {
    #[serde(default)]
    pub hours: Option<f64>,
    #[serde(default)]
    pub minutes: Option<f64>,
    #[serde(default)]
    pub seconds: Option<f64>,
    #[serde(default)]
    pub milliseconds: Option<f64>,
}

impl DelayValue {
    fn duration(&self) -> std::result::Result<Duration, String> {
        let seconds = match self {
            Self::Seconds(seconds) => *seconds,
            Self::Text(text) => parse_clock_duration(text)?,
            Self::Parts(parts) => {
                let values = [
                    parts.hours,
                    parts.minutes,
                    parts.seconds,
                    parts.milliseconds,
                ];
                if values.iter().all(Option::is_none) {
                    return Err("delay duration object must contain a time component".into());
                }
                validate_components(&values)?;
                parts.hours.unwrap_or_default() * 3600.0
                    + parts.minutes.unwrap_or_default() * 60.0
                    + parts.seconds.unwrap_or_default()
                    + parts.milliseconds.unwrap_or_default() / 1000.0
            }
        };
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("delay must be a finite, nonnegative duration".into());
        }
        if seconds > MAX_DELAY.as_secs_f64() {
            return Err(format!(
                "delay must not exceed {} seconds",
                MAX_DELAY.as_secs()
            ));
        }
        Ok(Duration::from_secs_f64(seconds))
    }
}

fn validate_components(values: &[Option<f64>]) -> std::result::Result<(), String> {
    if values
        .iter()
        .flatten()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err("delay components must be finite and nonnegative".into());
    }
    Ok(())
}

fn parse_clock_duration(text: &str) -> std::result::Result<f64, String> {
    let fields: Vec<_> = text.split(':').collect();
    if fields.len() != 3 {
        return Err("text delay must use HH:MM:SS format".into());
    }
    let hours = fields[0]
        .parse::<u64>()
        .map_err(|_| "delay hours must be a nonnegative integer".to_string())?;
    let minutes = fields[1]
        .parse::<u64>()
        .map_err(|_| "delay minutes must be a nonnegative integer".to_string())?;
    let seconds = fields[2]
        .parse::<f64>()
        .map_err(|_| "delay seconds must be a nonnegative number".to_string())?;
    if minutes >= 60 || seconds >= 60.0 || !seconds.is_finite() || seconds < 0.0 {
        return Err("text delay minutes and seconds must be below 60".into());
    }
    Ok(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
}

/// Validate IDs, supported limits, trigger and condition options, and actions.
pub fn validate(automations: &[Automation]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for automation in automations {
        if !valid_identifier(&automation.id) || !ids.insert(automation.id.as_str()) {
            return Err(Error::InvalidInput(format!(
                "automation id `{}` is invalid or duplicated",
                automation.id
            )));
        }
        if automation
            .alias
            .as_ref()
            .is_some_and(|alias| alias.trim().is_empty())
        {
            return Err(Error::InvalidInput(format!(
                "automation `{}` alias must not be empty",
                automation.id
            )));
        }
        if automation.triggers.is_empty() || automation.triggers.len() > MAX_TRIGGERS {
            return Err(Error::InvalidInput(format!(
                "automation `{}` must have 1 to {MAX_TRIGGERS} triggers",
                automation.id
            )));
        }
        if automation.conditions.len() > MAX_CONDITIONS {
            return Err(Error::InvalidInput(format!(
                "automation `{}` exceeds the {MAX_CONDITIONS}-condition limit",
                automation.id
            )));
        }
        if automation.actions.is_empty() || automation.actions.len() > MAX_ACTIONS {
            return Err(Error::InvalidInput(format!(
                "automation `{}` must have 1 to {MAX_ACTIONS} actions",
                automation.id
            )));
        }

        for trigger in &automation.triggers {
            match trigger {
                Trigger::State { entity_id, .. } => {
                    let entities = entity_id.as_slice();
                    if entities.is_empty() || entities.iter().any(|id| !valid_entity_id(id)) {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid state trigger entity_id",
                            automation.id
                        )));
                    }
                }
                Trigger::Event { event_type, .. } if !valid_event_type(event_type) => {
                    return Err(Error::InvalidInput(format!(
                        "automation `{}` has an invalid event_type",
                        automation.id
                    )));
                }
                Trigger::Event { .. } => {}
            }
        }
        for condition in &automation.conditions {
            match condition {
                Condition::State { entity_id, .. } if !valid_entity_id(entity_id) => {
                    return Err(Error::InvalidInput(format!(
                        "automation `{}` has an invalid state condition entity_id",
                        automation.id
                    )));
                }
                Condition::NumericState {
                    entity_id,
                    above,
                    below,
                } => {
                    if !valid_entity_id(entity_id)
                        || (above.is_none() && below.is_none())
                        || above.is_some_and(|n| !n.is_finite())
                        || below.is_some_and(|n| !n.is_finite())
                        || matches!((above, below), (Some(a), Some(b)) if a >= b)
                    {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid numeric_state condition",
                            automation.id
                        )));
                    }
                }
                Condition::State { .. } => {}
            }
        }
        for action in &automation.actions {
            match action {
                Action::Service(service) if !valid_service_name(&service.action) => {
                    return Err(Error::InvalidInput(format!(
                        "automation `{}` has invalid service action `{}`",
                        automation.id, service.action
                    )));
                }
                Action::Service(_) => {}
                Action::Delay(delay) => {
                    delay.delay.duration().map_err(Error::InvalidInput)?;
                }
            }
        }
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn valid_entity_id(value: &str) -> bool {
    let Some((domain, object_id)) = value.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !object_id.is_empty()
        && value.matches('.').count() == 1
        && [domain, object_id].iter().all(|part| {
            part.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn valid_event_type(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

fn valid_service_name(value: &str) -> bool {
    let Some((domain, service)) = value.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !service.is_empty()
        && !service.contains('.')
        && [domain, service].iter().all(|part| {
            part.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Start the automation engine. It subscribes to runtime events before
/// returning. Broadcast lag and an overflowing pending queue terminate the
/// listener and are returned by [`AutomationEngine::shutdown`].
pub fn start(runtime: Runtime, automations: Vec<Automation>) -> Result<AutomationEngine> {
    validate(&automations)?;
    let receiver = runtime.subscribe();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let listener_cancel = cancel_tx.clone();
    let task = tokio::spawn(async move {
        run_engine(runtime, automations, receiver, cancel_rx, listener_cancel).await
    });
    Ok(AutomationEngine {
        cancel: cancel_tx,
        task: Some(task),
    })
}

/// Running automation engine. Call [`shutdown`](Self::shutdown) to cancel
/// delays and join all work.
pub struct AutomationEngine {
    cancel: tokio::sync::watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

impl AutomationEngine {
    /// Cancel event handling and active delays, then join every active run.
    /// Synchronous service handlers already in progress are joined and must
    /// return before shutdown completes.
    pub async fn shutdown(mut self) -> Result<()> {
        self.cancel.send_replace(true);
        let task = self
            .task
            .take()
            .expect("automation listener handle is present until shutdown");
        task.await
            .map_err(|error| Error::Service(format!("automation listener task failed: {error}")))?
    }
}

impl Drop for AutomationEngine {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

#[derive(Clone)]
struct PendingRun {
    automation: Automation,
    trigger_context: Context,
    lineage: Vec<String>,
}

#[derive(Clone)]
struct LineageEntry {
    automation_ids: Vec<String>,
    expires_at: Instant,
}

#[derive(Default)]
struct LineageRegistry {
    by_context_id: Mutex<HashMap<String, LineageEntry>>,
}

impl LineageRegistry {
    fn lookup(&self, context_id: &str) -> Vec<String> {
        let now = Instant::now();
        let mut entries = self
            .by_context_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, entry| entry.expires_at > now);
        entries
            .get(context_id)
            .map(|entry| entry.automation_ids.clone())
            .unwrap_or_default()
    }

    fn insert(&self, context_id: String, mut automation_ids: Vec<String>) -> Result<()> {
        let now = Instant::now();
        let mut entries = self
            .by_context_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= MAX_LINEAGE_ENTRIES {
            return Err(Error::Service(format!(
                "automation lineage registry reached its limit of {MAX_LINEAGE_ENTRIES}"
            )));
        }
        if automation_ids.len() > MAX_LINEAGE_DEPTH {
            automation_ids.truncate(MAX_LINEAGE_DEPTH);
        }
        entries.insert(
            context_id,
            LineageEntry {
                automation_ids,
                expires_at: now + LINEAGE_TTL,
            },
        );
        Ok(())
    }
}

async fn run_engine(
    runtime: Runtime,
    automations: Vec<Automation>,
    mut receiver: tokio::sync::broadcast::Receiver<Event>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    cancel_tx: tokio::sync::watch::Sender<bool>,
) -> Result<()> {
    let lineage = Arc::new(LineageRegistry::default());
    let mut active = JoinSet::<()>::new();
    let mut pending = VecDeque::<PendingRun>::new();
    let mut failure = None;

    loop {
        if *cancel_rx.borrow() {
            break;
        }
        while active.len() < MAX_ACTIVE_RUNS {
            let Some(run) = pending.pop_front() else {
                break;
            };
            let run_runtime = runtime.clone();
            let run_lineage = Arc::clone(&lineage);
            let run_cancel = cancel_rx.clone();
            active.spawn(async move {
                if let Err(error) =
                    execute_automation(run_runtime, run, run_lineage, run_cancel).await
                {
                    tracing::warn!(%error, "automation run failed");
                }
            });
        }

        tokio::select! {
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            joined = active.join_next(), if !active.is_empty() => {
                if let Some(Err(error)) = joined {
                    tracing::error!(%error, "automation run task panicked");
                }
            }
            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        let inherited_lineage = lineage.lookup(&event.context.id);
                        for automation in &automations {
                            if inherited_lineage.iter().any(|id| id == &automation.id) {
                                continue;
                            }
                            if inherited_lineage.len() >= MAX_LINEAGE_DEPTH {
                                tracing::warn!(
                                    automation_id = %automation.id,
                                    "automation chain depth limit reached"
                                );
                                continue;
                            }
                            if !automation.triggers.iter().any(|trigger| trigger_matches(trigger, &event)) {
                                continue;
                            }
                            if !conditions_match(&runtime, &automation.conditions) {
                                continue;
                            }
                            if pending.len() >= MAX_PENDING_RUNS {
                                failure = Some(Error::Service(format!(
                                    "automation pending run queue reached its limit of {MAX_PENDING_RUNS}"
                                )));
                                break;
                            }
                            pending.push_back(PendingRun {
                                automation: automation.clone(),
                                trigger_context: event.context.clone(),
                                lineage: inherited_lineage.clone(),
                            });
                        }
                        if failure.is_some() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        failure = Some(Error::Service(format!(
                            "automation listener lagged and missed {skipped} runtime events"
                        )));
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        failure = Some(Error::Service("runtime event stream closed".into()));
                        break;
                    }
                }
            }
        }
    }

    // Cancel child runs and wait for all of them. In-flight synchronous service
    // calls are awaited by their run tasks because the core API cannot cancel
    // arbitrary handlers safely.
    cancel_tx.send_replace(true);
    pending.clear();
    while let Some(joined) = active.join_next().await {
        if let Err(error) = joined {
            tracing::error!(%error, "automation run task failed during shutdown");
        }
    }
    if let Some(error) = failure {
        tracing::error!(%error, "automation engine stopped after event processing failure");
        Err(error)
    } else {
        Ok(())
    }
}

async fn execute_automation(
    runtime: Runtime,
    run: PendingRun,
    lineage_registry: Arc<LineageRegistry>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if *cancel_rx.borrow() {
        return Ok(());
    }
    let mut ancestry = run.lineage;
    ancestry.push(run.automation.id.clone());
    if ancestry.len() > MAX_LINEAGE_DEPTH {
        return Ok(());
    }
    let service_context = Context {
        id: Uuid::new_v4().to_string(),
        parent_id: Some(run.trigger_context.id.clone()),
        user_id: run.trigger_context.user_id.clone(),
    };
    lineage_registry.insert(service_context.id.clone(), ancestry)?;
    let deadline = TokioInstant::now() + MAX_RUN_TIME;

    for action in run.automation.actions {
        if *cancel_rx.borrow() {
            return Ok(());
        }
        match action {
            Action::Delay(delay) => {
                let duration = delay.delay.duration().map_err(Error::InvalidInput)?;
                match wait_for_delay(duration, deadline, &mut cancel_rx).await? {
                    true => {}
                    false => return Ok(()),
                }
            }
            Action::Service(service) => {
                let (domain, service_name) = service
                    .action
                    .split_once('.')
                    .expect("service names are validated before engine start");
                let call = ServiceCall {
                    domain: domain.to_owned(),
                    service: service_name.to_owned(),
                    data: service.data.unwrap_or_default(),
                    target: service.target.unwrap_or_default(),
                    context: service_context.clone(),
                };
                run_service_call(&runtime, call, deadline, &mut cancel_rx).await?;
            }
        }
    }
    Ok(())
}

/// Returns true if the delay completed, false if shutdown canceled it.
async fn wait_for_delay(
    duration: Duration,
    deadline: TokioInstant,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool> {
    let delay_end = TokioInstant::now() + duration;
    if delay_end > deadline {
        return Err(Error::Service(
            "automation run exceeded its 60-second limit".into(),
        ));
    }
    tokio::select! {
        _ = tokio::time::sleep_until(delay_end) => Ok(true),
        _ = tokio::time::sleep_until(deadline) => Err(Error::Service("automation run exceeded its 60-second limit".into())),
        _ = wait_for_cancellation(cancel_rx) => Ok(false),
    }
}

async fn wait_for_cancellation(cancel_rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *cancel_rx.borrow() || cancel_rx.changed().await.is_err() {
            return;
        }
    }
}

async fn run_service_call(
    runtime: &Runtime,
    call: ServiceCall,
    deadline: TokioInstant,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if TokioInstant::now() >= deadline {
        return Err(Error::Service(
            "automation run exceeded its 60-second limit".into(),
        ));
    }
    let runtime = runtime.clone();
    let service_name = format!("{}.{}", call.domain, call.service);
    let mut handler = tokio::task::spawn_blocking(move || runtime.call_service(call));
    tokio::select! {
        result = &mut handler => map_service_result(result, &service_name),
        _ = tokio::time::sleep_until(deadline) => {
            tracing::warn!(service = %service_name, "automation action exceeded its run deadline; joining synchronous handler");
            let _ = handler.await;
            Err(Error::Service("automation run exceeded its 60-second limit".into()))
        }
        _ = wait_for_cancellation(cancel_rx) => {
            // Do not detach an already-started synchronous handler on shutdown.
            let _ = handler.await;
            Ok(())
        }
    }
}

fn map_service_result(
    result: std::result::Result<Result<hass_core::ServiceResult>, tokio::task::JoinError>,
    service_name: &str,
) -> Result<()> {
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(Error::Service(format!(
            "service `{service_name}` task failed: {error}"
        ))),
    }
}

fn trigger_matches(trigger: &Trigger, event: &Event) -> bool {
    match trigger {
        Trigger::State {
            entity_id,
            from,
            to,
        } => state_trigger_matches(entity_id, from.as_deref(), to.as_deref(), event),
        Trigger::Event {
            event_type,
            event_data,
        } => {
            event.event_type == *event_type
                && event_data.as_ref().is_none_or(|required| {
                    required
                        .iter()
                        .all(|(key, value)| event.data.get(key) == Some(value))
                })
        }
    }
}

fn state_trigger_matches(
    entity_ids: &EntityIds,
    from: Option<&str>,
    to: Option<&str>,
    event: &Event,
) -> bool {
    if event.event_type != "state_changed" {
        return false;
    }
    let Some(entity_id) = event.data.get("entity_id").and_then(Value::as_str) else {
        return false;
    };
    if !entity_ids.as_slice().contains(&entity_id) {
        return false;
    }
    let old_state = match event.data.get("old_state") {
        Some(Value::Null) | None => None,
        Some(value) => match serde_json::from_value::<State>(value.clone()) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(%error, "ignoring malformed old_state in state_changed event");
                return false;
            }
        },
    };
    let new_state = match event.data.get("new_state") {
        Some(Value::Null) | None => None,
        Some(value) => match serde_json::from_value::<State>(value.clone()) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(%error, "ignoring malformed new_state in state_changed event");
                return false;
            }
        },
    };
    let (Some(new_state), entity) = (new_state, entity_id) else {
        // Deletion transitions are outside this crate's documented state subset.
        return false;
    };
    if new_state.entity_id != entity {
        return false;
    }
    if let Some(old_state) = &old_state
        && (old_state.entity_id != entity || old_state.state == new_state.state)
    {
        // Attribute-only reports are not state transitions.
        return false;
    }
    from.is_none_or(|expected| {
        old_state
            .as_ref()
            .is_some_and(|state| state.state == expected)
    }) && to.is_none_or(|expected| new_state.state == expected)
}

fn conditions_match(runtime: &Runtime, conditions: &[Condition]) -> bool {
    conditions.iter().all(|condition| match condition {
        Condition::State { entity_id, state } => runtime
            .get_state(entity_id)
            .is_some_and(|current| current.state == *state),
        Condition::NumericState {
            entity_id,
            above,
            below,
        } => runtime.get_state(entity_id).is_some_and(|current| {
            let Ok(value) = current.state.parse::<f64>() else {
                return false;
            };
            value.is_finite()
                && above.is_none_or(|bound| value > bound)
                && below.is_none_or(|bound| value < bound)
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{MemoryStore, ServiceResult};
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration as StdDuration,
    };
    use tokio::sync::mpsc;

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn config(yaml: &str) -> Vec<Automation> {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn register_notifier(runtime: &Runtime, sender: mpsc::UnboundedSender<ServiceCall>) {
        runtime
            .register_service(
                "test",
                "notify",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    let _ = sender.send(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn state_trigger_condition_and_service_preserve_context() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: arrived
  triggers:
    - trigger: state
      entity_id: person.josh
      from: away
      to: home
  conditions:
    - condition: state
      entity_id: input_boolean.allowed
      state: "on"
    - condition: numeric_state
      entity_id: sensor.score
      above: 10
  actions:
    - action: test.notify
      data:
        message: arrived
      target:
        entity_id: notify.phone
"#,
        );
        runtime
            .set_state(
                "input_boolean.allowed",
                "on",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .set_state("sensor.score", "12", Map::new(), Context::default(), false)
            .unwrap();
        runtime
            .set_state("person.josh", "away", Map::new(), Context::default(), false)
            .unwrap();
        let engine = start(runtime.clone(), automations).unwrap();
        let trigger_context = Context {
            id: Uuid::new_v4().to_string(),
            parent_id: None,
            user_id: Some("user-7".into()),
        };
        runtime
            .set_state(
                "person.josh",
                "home",
                Map::new(),
                trigger_context.clone(),
                false,
            )
            .unwrap();
        let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call.domain, "test");
        assert_eq!(call.service, "notify");
        assert_eq!(
            call.context.parent_id.as_deref(),
            Some(trigger_context.id.as_str())
        );
        assert_eq!(call.context.user_id.as_deref(), Some("user-7"));
        assert_eq!(call.data["message"], "arrived");
        assert_eq!(call.target["entity_id"], "notify.phone");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn event_trigger_matches_data_subset() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: front_door
  triggers:
    - trigger: event
      event_type: door_opened
      event_data:
        source: front
  actions:
    - action: test.notify
"#,
            ),
        )
        .unwrap();
        let mut data = Map::new();
        data.insert("source".into(), Value::String("front".into()));
        data.insert("battery".into(), Value::from(81));
        runtime
            .fire_event("door_opened", data, Context::default())
            .unwrap();
        let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call.service, "notify");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn state_trigger_requires_the_configured_state_transition() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        runtime
            .set_state("sensor.value", "off", Map::new(), Context::default(), false)
            .unwrap();
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: exact_transition
  triggers:
    - trigger: state
      entity_id: sensor.value
      from: off
      to: on
  actions:
    - action: test.notify
"#,
            ),
        )
        .unwrap();

        let mut attributes = Map::new();
        attributes.insert("reported".into(), Value::Bool(true));
        runtime
            .set_state("sensor.value", "off", attributes, Context::default(), true)
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(receiver.try_recv().is_err());

        runtime
            .set_state("sensor.value", "on", Map::new(), Context::default(), false)
            .unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_some()
        );
        engine.shutdown().await.unwrap();
    }

    #[test]
    fn rejects_unknown_options_and_bad_bounds() {
        let unknown: std::result::Result<Vec<Automation>, _> = serde_yaml::from_str(
            r#"
- id: invalid
  triggers:
    - trigger: event
      event_type: demo
      platform: event
  actions:
    - action: test.notify
"#,
        );
        assert!(unknown.is_err());

        let invalid = config(
            r#"
- id: invalid
  triggers:
    - trigger: event
      event_type: demo
  conditions:
    - condition: numeric_state
      entity_id: sensor.value
      above: 10
      below: 5
  actions:
    - action: test.notify
"#,
        );
        assert!(validate(&invalid).is_err());

        let bad_delay = config(
            r#"
- id: invalid_delay
  triggers:
    - trigger: event
      event_type: demo
  actions:
    - delay:
        seconds: 31
"#,
        );
        assert!(validate(&bad_delay).is_err());
    }

    #[tokio::test]
    async fn shutdown_cancels_active_delay_and_joins_run() {
        let runtime = runtime();
        let (started_sender, mut started_receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, started_sender);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_handler = Arc::clone(&calls);
        runtime
            .register_service(
                "test",
                "count",
                Value::Null,
                Arc::new(move |_runtime, call| {
                    calls_in_handler.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: delayed
  triggers:
    - trigger: event
      event_type: begin
  actions:
    - action: test.notify
    - delay: 20
    - action: test.count
"#,
            ),
        )
        .unwrap();
        runtime
            .fire_event("begin", Map::new(), Context::default())
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(2), started_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        tokio::time::timeout(StdDuration::from_secs(2), engine.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn automation_chains_do_not_reenter_the_first_automation() {
        let runtime = runtime();
        let count = Arc::new(AtomicUsize::new(0));
        let count_a = Arc::clone(&count);
        runtime
            .register_service(
                "test",
                "a",
                Value::Null,
                Arc::new(move |runtime, call| {
                    count_a.fetch_add(1, Ordering::SeqCst);
                    runtime.set_state(
                        "sensor.b",
                        "off",
                        Map::new(),
                        call.context.clone(),
                        false,
                    )?;
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let count_b = Arc::clone(&count);
        runtime
            .register_service(
                "test",
                "b",
                Value::Null,
                Arc::new(move |runtime, call| {
                    count_b.fetch_add(1, Ordering::SeqCst);
                    runtime.set_state(
                        "sensor.a",
                        "off",
                        Map::new(),
                        call.context.clone(),
                        false,
                    )?;
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        runtime
            .set_state("sensor.a", "off", Map::new(), Context::default(), false)
            .unwrap();
        runtime
            .set_state("sensor.b", "on", Map::new(), Context::default(), false)
            .unwrap();
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: automation_a
  triggers:
    - trigger: state
      entity_id: sensor.a
  actions:
    - action: test.a
- id: automation_b
  triggers:
    - trigger: state
      entity_id: sensor.b
  actions:
    - action: test.b
"#,
            ),
        )
        .unwrap();
        runtime
            .set_state("sensor.a", "on", Map::new(), Context::default(), false)
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while count.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(StdDuration::from_millis(80)).await;
        engine.shutdown().await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
