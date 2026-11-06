//! A strict, bounded native Home Assistant automation engine.
//!
//! YAML may be deserialized as `Vec<Automation>`, validated, and run against
//! [`hass_core::Runtime`]. The engine supports state, numeric-state, template,
//! time, time-pattern, lifecycle, event, and native MQTT signal triggers;
//! state, numeric-state, template, and nested conditions; and action sequences
//! through the shared `hass-script` executor. Unknown or unsupported options
//! are rejected. See the crate README for examples and limits.
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
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{Datelike, Timelike};
use chrono_tz::Tz;
use hass_core::{Context, Error, Event, Result, Runtime, ServiceCall, ServiceResult, State};
use hass_script::{run_sequence, validate_sequence};
use hass_template::TemplateEngine;
use hass_webhook::{
    WebhookConfig, WebhookHandlerError, WebhookManager, WebhookRegistration, WebhookRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::Instant as TokioInstant,
};
use uuid::Uuid;

const MAX_TRIGGERS: usize = 16;
const MAX_CONDITIONS: usize = 16;
const MAX_ACTIONS: usize = 32;
const MAX_ACTIVE_RUNS: usize = 16;
const MAX_PENDING_RUNS: usize = 128;
const MAX_WEBHOOK_DELIVERIES: usize = 128;
const MAX_SCHEDULED_RUNS: usize = 128;
const MAX_TRIGGER_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LINEAGE_DEPTH: usize = 32;
const MAX_LINEAGE_ENTRIES: usize = 4096;
const LINEAGE_TTL: Duration = Duration::from_secs(5 * 60);
const AUTOMATION_ENABLED_PREFIX: &str = "automation.enabled.";
const PERSISTENT_NOTIFICATION_SIGNAL: &str = "persistent_notifications_updated";

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
    #[serde(alias = "trigger")]
    pub triggers: Vec<Trigger>,
    /// Conditions are ANDed. An empty list passes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Values available to action and condition templates.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub variables: Map<String, Value>,
    /// Values available while evaluating trigger templates.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub trigger_variables: Map<String, Value>,
    /// Run scheduling behavior.
    #[serde(default)]
    pub mode: AutomationMode,
    /// Maximum active or queued runs for `queued` and `parallel` modes.
    #[serde(default = "default_max_runs")]
    pub max: usize,
    /// Initial state when no persisted enable state exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_state: Option<bool>,
    /// Script-compatible sequence. Both `action` and `actions` are accepted.
    #[serde(alias = "action")]
    pub actions: Vec<Action>,
}

fn default_max_runs() -> usize {
    10
}

/// Scheduling behavior for automation runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationMode {
    /// Ignore new triggers while one run is active.
    #[default]
    Single,
    /// Cancel active and queued runs, then start the newest trigger.
    Restart,
    /// Run one at a time in trigger order.
    Queued,
    /// Run up to `max` actions concurrently.
    Parallel,
}

/// Supported Home Assistant automation trigger definitions.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "trigger", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// Match a change in the state's value for any configured entity.
    State {
        entity_id: EntityIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#for: Option<DelayValue>,
    },
    /// Match a threshold crossing into a configured numeric range.
    NumericState {
        entity_id: EntityIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_template: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#for: Option<DelayValue>,
    },
    /// Match a false-to-true template transition.
    Template {
        value_template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#for: Option<DelayValue>,
    },
    /// Match one or more local wall-clock times each day.
    Time { at: TimeAt },
    /// Match a recurring local wall-clock time pattern.
    TimePattern {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hours: Option<TimePatternValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<TimePatternValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<TimePatternValue>,
    },
    /// Match engine startup or shutdown lifecycle events.
    #[serde(rename = "homeassistant")]
    HomeAssistant { event: String },
    /// Match native MQTT signals emitted by the real MQTT integration.
    Mqtt {
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_template: Option<String>,
    },
    /// Match an added, updated, or removed persistent notification.
    PersistentNotification {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notification_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update_type: Option<NotificationUpdateTypes>,
    },
    /// Match an event type and a top-level subset of its data.
    Event {
        event_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_data: Option<Map<String, Value>>,
    },
    /// Match an inbound Home Assistant webhook.
    Webhook {
        webhook_id: String,
        #[serde(default = "default_webhook_local_only")]
        local_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_methods: Option<WebhookAllowedMethods>,
        #[serde(default = "default_webhook_enabled")]
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
}

fn default_webhook_local_only() -> bool {
    true
}

fn default_webhook_enabled() -> bool {
    true
}

/// A single method or list of methods accepted by a webhook trigger.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WebhookAllowedMethods {
    /// One allowed method, as in `allowed_methods: PUT`.
    One(String),
    /// Several allowed methods, as in `allowed_methods: [GET, POST]`.
    Many(Vec<String>),
}

impl WebhookAllowedMethods {
    fn into_vec(self) -> Vec<String> {
        let methods = match self {
            Self::One(method) => vec![method],
            Self::Many(methods) => methods,
        };
        methods
            .into_iter()
            .map(|method| method.to_ascii_uppercase())
            .collect()
    }
}

fn webhook_method_supported(method: &str) -> bool {
    let method = method.to_ascii_uppercase();
    hass_webhook::SUPPORTED_METHODS.contains(&method.as_str())
}

/// Update types emitted by Home Assistant's persistent notification trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationUpdateType {
    /// A current notification snapshot. The native feed does not emit snapshots.
    Current,
    /// A new notification was created.
    Added,
    /// An existing notification was replaced.
    Updated,
    /// One or more notifications were dismissed.
    Removed,
}

impl NotificationUpdateType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Removed => "removed",
        }
    }
}

/// One update type or a list of update types accepted by a notification trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NotificationUpdateTypes {
    One(NotificationUpdateType),
    Many(Vec<NotificationUpdateType>),
}

impl NotificationUpdateTypes {
    fn allows(&self, update_type: NotificationUpdateType) -> bool {
        match self {
            Self::One(expected) => *expected == update_type,
            Self::Many(expected) => expected.is_empty() || expected.contains(&update_type),
        }
    }
}

/// One clock time or a list of clock times in `HH:MM[:SS]` form.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TimeAt {
    One(String),
    Many(Vec<String>),
}

/// A time-pattern component written as an integer, `*`, or `/step`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TimePatternValue {
    Number(u32),
    Text(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case", deny_unknown_fields)]
enum TriggerRepr {
    State {
        entity_id: EntityIds,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        not_from: Option<String>,
        #[serde(default)]
        not_to: Option<String>,
        #[serde(default)]
        attribute: Option<String>,
        #[serde(default)]
        r#for: Option<DelayValue>,
    },
    NumericState {
        entity_id: EntityIds,
        #[serde(default)]
        above: Option<f64>,
        #[serde(default)]
        below: Option<f64>,
        #[serde(default)]
        attribute: Option<String>,
        #[serde(default)]
        value_template: Option<String>,
        #[serde(default)]
        r#for: Option<DelayValue>,
    },
    Template {
        value_template: String,
        #[serde(default)]
        r#for: Option<DelayValue>,
    },
    Time {
        at: TimeAt,
    },
    TimePattern {
        #[serde(default)]
        hours: Option<TimePatternValue>,
        #[serde(default)]
        minutes: Option<TimePatternValue>,
        #[serde(default)]
        seconds: Option<TimePatternValue>,
    },
    #[serde(rename = "homeassistant")]
    HomeAssistant {
        event: String,
    },
    Mqtt {
        topic: String,
        #[serde(default)]
        payload: Option<String>,
        #[serde(default)]
        value_template: Option<String>,
    },
    PersistentNotification {
        #[serde(default)]
        notification_id: Option<String>,
        #[serde(default)]
        update_type: Option<NotificationUpdateTypes>,
    },
    Event {
        event_type: String,
        #[serde(default)]
        event_data: Option<Map<String, Value>>,
    },
    Webhook {
        webhook_id: String,
        #[serde(default = "default_webhook_local_only")]
        local_only: bool,
        #[serde(default)]
        allowed_methods: Option<WebhookAllowedMethods>,
        #[serde(default = "default_webhook_enabled")]
        enabled: bool,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        alias: Option<String>,
    },
}

impl From<TriggerRepr> for Trigger {
    fn from(trigger: TriggerRepr) -> Self {
        match trigger {
            TriggerRepr::State {
                entity_id,
                from,
                to,
                not_from,
                not_to,
                attribute,
                r#for,
            } => Self::State {
                entity_id,
                from,
                to,
                not_from,
                not_to,
                attribute,
                r#for,
            },
            TriggerRepr::NumericState {
                entity_id,
                above,
                below,
                attribute,
                value_template,
                r#for,
            } => Self::NumericState {
                entity_id,
                above,
                below,
                attribute,
                value_template,
                r#for,
            },
            TriggerRepr::Template {
                value_template,
                r#for,
            } => Self::Template {
                value_template,
                r#for,
            },
            TriggerRepr::Time { at } => Self::Time { at },
            TriggerRepr::TimePattern {
                hours,
                minutes,
                seconds,
            } => Self::TimePattern {
                hours,
                minutes,
                seconds,
            },
            TriggerRepr::HomeAssistant { event } => Self::HomeAssistant { event },
            TriggerRepr::Mqtt {
                topic,
                payload,
                value_template,
            } => Self::Mqtt {
                topic,
                payload,
                value_template,
            },
            TriggerRepr::PersistentNotification {
                notification_id,
                update_type,
            } => Self::PersistentNotification {
                notification_id,
                update_type,
            },
            TriggerRepr::Event {
                event_type,
                event_data,
            } => Self::Event {
                event_type,
                event_data,
            },
            TriggerRepr::Webhook {
                webhook_id,
                local_only,
                allowed_methods,
                enabled,
                id,
                alias,
            } => Self::Webhook {
                webhook_id,
                local_only,
                allowed_methods,
                enabled,
                id,
                alias,
            },
        }
    }
}

impl<'de> Deserialize<'de> for Trigger {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object_mut() else {
            return Err(serde::de::Error::custom("trigger must be a mapping"));
        };
        if let Some(platform) = object.remove("platform") {
            if object.contains_key("trigger") {
                return Err(serde::de::Error::custom(
                    "trigger must use either `trigger` or legacy `platform`, not both",
                ));
            }
            object.insert("trigger".into(), platform);
        }
        serde_json::from_value::<TriggerRepr>(value)
            .map(Into::into)
            .map_err(serde::de::Error::custom)
    }
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

/// Supported state, numeric-state, template, and boolean conditions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    /// Require the current state string to equal `state`.
    State {
        entity_id: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
    },
    /// Require a numeric state to be strictly above and/or below its bounds.
    NumericState {
        entity_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_template: Option<String>,
    },
    /// Evaluate a template as a boolean condition.
    Template { value_template: String },
    /// Evaluate every nested condition.
    And { conditions: Vec<Condition> },
    /// Evaluate at least one nested condition.
    Or { conditions: Vec<Condition> },
    /// Negate one nested condition.
    Not { conditions: Vec<Condition> },
}

/// A service call or bounded delay.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    Service(ServiceAction),
    Delay(DelayAction),
    /// Any additional action supported by the shared script executor.
    Script(Value),
}

/// Service action in modern `action: domain.service` form.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAction {
    #[serde(alias = "service")]
    pub action: String,
    #[serde(
        default,
        alias = "service_data",
        skip_serializing_if = "Option::is_none"
    )]
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
    fn duration_with_limit(&self, limit: Duration) -> std::result::Result<Duration, String> {
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
        if seconds > limit.as_secs_f64() {
            return Err(format!(
                "duration must not exceed {} seconds",
                limit.as_secs()
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
    let mut entity_ids = std::collections::HashSet::new();
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
        if automation.max == 0 || automation.max > MAX_PENDING_RUNS {
            return Err(Error::InvalidInput(format!(
                "automation `{}` max must be between 1 and {MAX_PENDING_RUNS}",
                automation.id
            )));
        }
        if !entity_ids.insert(automation_entity_id(&automation.id)) {
            return Err(Error::InvalidInput(format!(
                "automation `{}` conflicts with another automation entity_id",
                automation.id
            )));
        }

        for trigger in &automation.triggers {
            match trigger {
                Trigger::State {
                    entity_id, r#for, ..
                } => {
                    let entities = entity_id.as_slice();
                    if entities.is_empty() || entities.iter().any(|id| !valid_entity_id(id)) {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid state trigger entity_id",
                            automation.id
                        )));
                    }
                    if let Some(duration) = r#for {
                        duration
                            .duration_with_limit(MAX_TRIGGER_FOR)
                            .map_err(Error::InvalidInput)?;
                    }
                }
                Trigger::NumericState {
                    entity_id,
                    above,
                    below,
                    r#for,
                    value_template,
                    ..
                } => {
                    if entity_id.as_slice().is_empty()
                        || entity_id.as_slice().iter().any(|id| !valid_entity_id(id))
                        || (above.is_none() && below.is_none())
                        || above.is_some_and(|n| !n.is_finite())
                        || below.is_some_and(|n| !n.is_finite())
                        || matches!((above, below), (Some(a), Some(b)) if a >= b)
                        || value_template.as_ref().is_some_and(|s| s.trim().is_empty())
                    {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid numeric_state trigger",
                            automation.id
                        )));
                    }
                    if let Some(duration) = r#for {
                        duration
                            .duration_with_limit(MAX_TRIGGER_FOR)
                            .map_err(Error::InvalidInput)?;
                    }
                }
                Trigger::Template {
                    value_template,
                    r#for,
                } => {
                    if value_template.trim().is_empty() {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an empty template trigger",
                            automation.id
                        )));
                    }
                    if let Some(duration) = r#for {
                        duration
                            .duration_with_limit(MAX_TRIGGER_FOR)
                            .map_err(Error::InvalidInput)?;
                    }
                }
                Trigger::Time { at } => {
                    let times = match at {
                        TimeAt::One(time) => vec![time.as_str()],
                        TimeAt::Many(times) => times.iter().map(String::as_str).collect(),
                    };
                    if times.is_empty() || times.iter().any(|time| parse_clock_time(time).is_none())
                    {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid time trigger",
                            automation.id
                        )));
                    }
                }
                Trigger::TimePattern {
                    hours,
                    minutes,
                    seconds,
                } => {
                    if [hours.as_ref(), minutes.as_ref(), seconds.as_ref()]
                        .into_iter()
                        .all(|component| component.is_none())
                        || !valid_time_pattern_component(hours.as_ref(), 23)
                        || !valid_time_pattern_component(minutes.as_ref(), 59)
                        || !valid_time_pattern_component(seconds.as_ref(), 59)
                    {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid time_pattern trigger",
                            automation.id
                        )));
                    }
                }
                Trigger::HomeAssistant { event }
                    if !matches!(event.as_str(), "start" | "shutdown") =>
                {
                    return Err(Error::InvalidInput(format!(
                        "automation `{}` has an unsupported homeassistant event `{event}`",
                        automation.id
                    )));
                }
                Trigger::HomeAssistant { .. } => {}
                Trigger::Mqtt {
                    topic,
                    value_template,
                    ..
                } => {
                    if !valid_mqtt_topic_filter(topic)
                        || value_template.as_ref().is_some_and(|s| s.trim().is_empty())
                    {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid mqtt trigger topic or value_template",
                            automation.id
                        )));
                    }
                }
                Trigger::PersistentNotification { .. } => {}
                Trigger::Event { event_type, .. } if !valid_event_type(event_type) => {
                    return Err(Error::InvalidInput(format!(
                        "automation `{}` has an invalid event_type",
                        automation.id
                    )));
                }
                Trigger::Event { .. } => {}
                Trigger::Webhook {
                    allowed_methods, ..
                } => {
                    let supported = match allowed_methods {
                        None => true,
                        Some(WebhookAllowedMethods::One(method)) => {
                            webhook_method_supported(method)
                        }
                        Some(WebhookAllowedMethods::Many(methods)) => methods
                            .iter()
                            .all(|method| webhook_method_supported(method)),
                    };
                    if !supported {
                        return Err(Error::InvalidInput(format!(
                            "automation `{}` has an invalid webhook allowed_methods value",
                            automation.id
                        )));
                    }
                }
            }
        }
        for condition in &automation.conditions {
            validate_condition(&automation.id, condition)?;
        }
        for action in &automation.actions {
            if let Action::Service(service) = action
                && !valid_service_name(&service.action)
            {
                return Err(Error::InvalidInput(format!(
                    "automation `{}` has invalid service action `{}`",
                    automation.id, service.action
                )));
            }
        }
        let sequence = automation_sequence(&automation.actions)?;
        validate_sequence(&sequence)?;
    }
    Ok(())
}

fn validate_condition(automation_id: &str, condition: &Condition) -> Result<()> {
    match condition {
        Condition::State { entity_id, .. } if !valid_entity_id(entity_id) => {
            Err(Error::InvalidInput(format!(
                "automation `{automation_id}` has an invalid state condition entity_id"
            )))
        }
        Condition::NumericState {
            entity_id,
            above,
            below,
            value_template,
            ..
        } => {
            if !valid_entity_id(entity_id)
                || (above.is_none() && below.is_none())
                || above.is_some_and(|n| !n.is_finite())
                || below.is_some_and(|n| !n.is_finite())
                || matches!((above, below), (Some(a), Some(b)) if a >= b)
                || value_template.as_ref().is_some_and(|s| s.trim().is_empty())
            {
                return Err(Error::InvalidInput(format!(
                    "automation `{automation_id}` has an invalid numeric_state condition"
                )));
            }
            Ok(())
        }
        Condition::Template { value_template } if value_template.trim().is_empty() => {
            Err(Error::InvalidInput(format!(
                "automation `{automation_id}` has an empty template condition"
            )))
        }
        Condition::And { conditions } | Condition::Or { conditions } => {
            if conditions.is_empty() || conditions.len() > MAX_CONDITIONS {
                return Err(Error::InvalidInput(format!(
                    "automation `{automation_id}` has an invalid nested condition list"
                )));
            }
            for condition in conditions {
                validate_condition(automation_id, condition)?;
            }
            Ok(())
        }
        Condition::Not { conditions } => {
            if conditions.len() != 1 {
                return Err(Error::InvalidInput(format!(
                    "automation `{automation_id}` not condition requires exactly one child"
                )));
            }
            validate_condition(automation_id, &conditions[0])
        }
        _ => Ok(()),
    }
}

fn automation_sequence(actions: &[Action]) -> Result<Value> {
    let mut sequence = Vec::with_capacity(actions.len());
    for action in actions {
        let value = match action {
            Action::Service(service) => serde_json::to_value(service),
            Action::Delay(delay) => serde_json::to_value(delay),
            Action::Script(value) => Ok(value.clone()),
        }
        .map_err(|error| Error::InvalidInput(format!("invalid automation action: {error}")))?;
        sequence.push(value);
    }
    Ok(Value::Array(sequence))
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

fn automation_entity_id(id: &str) -> String {
    format!("automation.{}", id.replace('-', "_"))
}

fn parse_clock_time(value: &str) -> Option<(u32, u32, u32)> {
    let fields: Vec<_> = value.split(':').collect();
    if !matches!(fields.len(), 2 | 3) {
        return None;
    }
    let hour = fields[0].parse::<u32>().ok()?;
    let minute = fields[1].parse::<u32>().ok()?;
    let second = if fields.len() == 3 {
        fields[2].parse::<u32>().ok()?
    } else {
        0
    };
    (hour < 24 && minute < 60 && second < 60).then_some((hour, minute, second))
}

fn valid_time_pattern_component(value: Option<&TimePatternValue>, max: u32) -> bool {
    match value {
        None => true,
        Some(TimePatternValue::Number(number)) => *number <= max,
        Some(TimePatternValue::Text(value)) if value == "*" => true,
        Some(TimePatternValue::Text(value)) if value.starts_with('/') => value[1..]
            .parse::<u32>()
            .is_ok_and(|step| step > 0 && step <= max + 1),
        Some(TimePatternValue::Text(value)) => {
            value.parse::<u32>().is_ok_and(|number| number <= max)
        }
    }
}

fn matches_time_pattern(value: Option<&TimePatternValue>, actual: u32) -> bool {
    match value {
        None => true,
        Some(TimePatternValue::Number(number)) => *number == actual,
        Some(TimePatternValue::Text(value)) if value == "*" => true,
        Some(TimePatternValue::Text(value)) if value.starts_with('/') => value[1..]
            .parse::<u32>()
            .is_ok_and(|step| step != 0 && actual.is_multiple_of(step)),
        Some(TimePatternValue::Text(value)) => {
            value.parse::<u32>().is_ok_and(|number| number == actual)
        }
    }
}

fn valid_mqtt_topic_filter(topic: &str) -> bool {
    if topic.is_empty()
        || topic.len() > 65535
        || topic.as_bytes().contains(&0)
        || topic.contains("{{")
        || topic.contains("}}")
    {
        return false;
    }
    let levels: Vec<_> = topic.split('/').collect();
    levels.iter().enumerate().all(|(index, level)| {
        if level.contains('#') {
            *level == "#" && index + 1 == levels.len()
        } else if level.contains('+') {
            *level == "+"
        } else {
            true
        }
    })
}

fn mqtt_topic_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && !filter.starts_with('$') {
        return false;
    }
    let filter_levels: Vec<_> = filter.split('/').collect();
    let topic_levels: Vec<_> = topic.split('/').collect();
    let mut topic_index = 0;
    for (filter_index, filter_level) in filter_levels.iter().enumerate() {
        if *filter_level == "#" {
            return filter_index + 1 == filter_levels.len();
        }
        let Some(topic_level) = topic_levels.get(topic_index) else {
            return false;
        };
        if *filter_level != "+" && *filter_level != *topic_level {
            return false;
        }
        topic_index += 1;
    }
    topic_index == topic_levels.len()
}

/// Return the distinct MQTT topic filters required by configured automations.
///
/// The root integration subscribes these filters on the real broker and
/// dispatches actual `mqtt_message` signals to this engine.
pub fn mqtt_topics(automations: &[Automation]) -> Vec<String> {
    let mut topics = std::collections::BTreeSet::new();
    for automation in automations {
        for trigger in &automation.triggers {
            if let Trigger::Mqtt { topic, .. } = trigger {
                topics.insert(topic.clone());
            }
        }
    }
    topics.into_iter().collect()
}

fn automation_enabled_key(id: &str) -> String {
    format!("{AUTOMATION_ENABLED_PREFIX}{id}")
}

fn automation_is_enabled(control: &AutomationControl, id: &str) -> bool {
    control
        .enabled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(id)
        .copied()
        .unwrap_or(false)
}

fn publish_automation_state(
    runtime: &Runtime,
    automation: &Automation,
    enabled: bool,
    context: Context,
) -> Result<State> {
    let mut attributes = Map::new();
    attributes.insert("id".into(), Value::String(automation.id.clone()));
    attributes.insert(
        "friendly_name".into(),
        Value::String(
            automation
                .alias
                .clone()
                .unwrap_or_else(|| automation.id.clone()),
        ),
    );
    attributes.insert(
        "mode".into(),
        Value::String(
            match automation.mode {
                AutomationMode::Single => "single",
                AutomationMode::Restart => "restart",
                AutomationMode::Queued => "queued",
                AutomationMode::Parallel => "parallel",
            }
            .into(),
        ),
    );
    Ok(runtime
        .set_state(
            &automation_entity_id(&automation.id),
            if enabled { "on" } else { "off" },
            attributes,
            context,
            false,
        )?
        .state)
}

fn register_automation_services(runtime: &Runtime, control: AutomationControl) -> Result<()> {
    for (service, description) in [
        ("turn_on", "Enable one or more automations"),
        ("turn_off", "Disable one or more automations"),
        ("toggle", "Toggle one or more automations"),
        ("trigger", "Run one or more automations"),
    ] {
        let control = control.clone();
        let operation = service.to_owned();
        runtime.register_service(
            "automation",
            service,
            serde_json::json!({ "name": description }),
            Arc::new(move |runtime, call| {
                handle_automation_service(runtime, call, &operation, &control)
            }),
        )?;
    }
    Ok(())
}

fn handle_automation_service(
    runtime: &Runtime,
    call: &ServiceCall,
    operation: &str,
    control: &AutomationControl,
) -> Result<ServiceResult> {
    let ids = requested_automation_ids(call, control, operation != "trigger")?;
    let mut changed_states = Vec::new();
    for id in ids {
        let automation = control
            .automations
            .get(&id)
            .expect("resolved automation ID belongs to the configured set");
        if operation == "trigger" {
            let mut data = Map::new();
            data.insert("automation_id".into(), Value::String(id));
            data.insert(
                "skip_condition".into(),
                call.data
                    .get("skip_condition")
                    .cloned()
                    .unwrap_or(Value::Bool(false)),
            );
            if let Some(Value::Object(variables)) = call.data.get("variables") {
                data.insert("variables".into(), Value::Object(variables.clone()));
            }
            runtime.fire_event("automation_trigger", data, call.context.clone())?;
            continue;
        }

        let old_enabled = automation_is_enabled(control, &id);
        let new_enabled = match operation {
            "turn_on" => true,
            "turn_off" => false,
            "toggle" => !old_enabled,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "unsupported automation service `{operation}`"
                )));
            }
        };
        runtime.set_data(&automation_enabled_key(&id), &Value::Bool(new_enabled))?;
        control
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, new_enabled);
        if let Some(webhooks) = control.webhooks.as_ref().and_then(Weak::upgrade) {
            webhooks.reconcile()?;
        }
        changed_states.push(publish_automation_state(
            runtime,
            automation,
            new_enabled,
            call.context.clone(),
        )?);
    }
    Ok(ServiceResult {
        changed_states,
        response: None,
        context: call.context.clone(),
    })
}

fn requested_automation_ids(
    call: &ServiceCall,
    control: &AutomationControl,
    allow_all: bool,
) -> Result<Vec<String>> {
    let target = call
        .target
        .get("entity_id")
        .or_else(|| call.data.get("entity_id"));
    let mut requested = Vec::<String>::new();
    match target {
        Some(Value::String(id)) => requested.push(id.clone()),
        Some(Value::Array(ids)) => {
            for id in ids {
                let Some(id) = id.as_str() else {
                    return Err(Error::InvalidInput(
                        "automation entity_id targets must be strings".into(),
                    ));
                };
                requested.push(id.to_owned());
            }
        }
        Some(_) => {
            return Err(Error::InvalidInput(
                "automation entity_id target must be a string or list".into(),
            ));
        }
        None if allow_all => return Ok(control.automations.keys().cloned().collect()),
        None => {
            return Err(Error::InvalidInput(
                "automation.trigger requires an entity_id target".into(),
            ));
        }
    }
    if requested.is_empty() {
        return Err(Error::InvalidInput(
            "automation service requires at least one target".into(),
        ));
    }
    let mut resolved = std::collections::BTreeSet::new();
    for target in requested {
        let id = control
            .automations
            .keys()
            .find(|id| **id == target || automation_entity_id(id) == target)
            .ok_or_else(|| Error::InvalidInput(format!("unknown automation target `{target}`")))?;
        resolved.insert(id.clone());
    }
    Ok(resolved.into_iter().collect())
}

fn shutdown_runs(runtime: &Runtime, automations: &[Automation]) -> Vec<PendingRun> {
    let shutdown_event = synthetic_event(
        "homeassistant",
        serde_json::json!({ "event": "shutdown" }),
        Context::default(),
    );
    automations
        .iter()
        .filter(|automation| {
            automation.triggers.iter().any(|trigger| {
                matches!(trigger, Trigger::HomeAssistant { event } if event == "shutdown")
            })
        })
        .map(|automation| PendingRun {
            automation: automation.clone(),
            trigger_context: shutdown_event.context.clone(),
            lineage: Vec::new(),
            variables: homeassistant_trigger_variables(
                runtime,
                automation,
                &shutdown_event,
                "shutdown",
                "Home Assistant stopping",
            ),
        })
        .collect()
}

fn homeassistant_trigger_variables(
    runtime: &Runtime,
    automation: &Automation,
    event: &Event,
    event_name: &str,
    description: &str,
) -> Map<String, Value> {
    let mut trigger_fields = Map::new();
    trigger_fields.insert("event".into(), Value::String(event_name.into()));
    trigger_fields.insert("description".into(), Value::String(description.into()));
    variables_for_trigger_with_fields(
        runtime,
        automation,
        Some(event),
        "homeassistant",
        None,
        Some(trigger_fields),
    )
}

fn webhook_setup_variables(
    template: &TemplateEngine,
    runtime: &Runtime,
    automation: &Automation,
) -> Option<Map<String, Value>> {
    let entity_id = automation_entity_id(&automation.id);
    let this = runtime
        .get_state(&entity_id)
        .and_then(|state| serde_json::to_value(state).ok())
        .unwrap_or(Value::Null);
    let mut variables = Map::new();
    variables.insert("this".into(), this);
    for (name, value) in &automation.trigger_variables {
        variables
            .entry(name.clone())
            .or_insert_with(|| value.clone());
    }
    let raw_variables = variables.clone();
    for (name, value) in raw_variables {
        if name == "this" {
            continue;
        }
        match render_config_value(template, &value, &variables) {
            Ok(value) => {
                variables.insert(name, value);
            }
            Err(_) => {
                tracing::warn!(
                    automation_id = %automation.id,
                    "webhook trigger variables could not be rendered"
                );
                return None;
            }
        }
    }
    Some(variables)
}

fn webhook_trigger_fields(request: &WebhookRequest, target: &WebhookTarget) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "webhook_id".into(),
        Value::String(request.webhook_id().to_owned()),
    );
    fields.insert("description".into(), Value::String("webhook".into()));
    fields.insert("id".into(), Value::String(target.id.clone()));
    fields.insert("idx".into(), Value::String(target.idx.clone()));
    fields.insert(
        "alias".into(),
        target.alias.clone().map_or(Value::Null, Value::String),
    );
    if let Some(json) = &request.json {
        fields.insert("json".into(), json.clone());
    } else {
        fields.insert(
            "data".into(),
            Value::Object(
                request
                    .form
                    .iter()
                    .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        );
    }
    fields.insert(
        "query".into(),
        Value::Object(
            request
                .query
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect(),
        ),
    );
    fields
}

fn initial_template_states(
    runtime: &Runtime,
    automations: &[Automation],
) -> Result<HashMap<(String, usize), bool>> {
    let mut values = HashMap::new();
    for automation in automations {
        for (index, trigger) in automation.triggers.iter().enumerate() {
            if let Trigger::Template { value_template, .. } = trigger {
                let variables = variables_for_trigger(runtime, automation, None, "template", None);
                values.insert(
                    (automation.id.clone(), index),
                    template_truthy(runtime, value_template, &variables)?,
                );
            }
        }
    }
    Ok(values)
}

/// Start the automation engine. It subscribes to runtime events before
/// returning. Broadcast lag and an overflowing pending queue terminate the
/// listener and are returned by [`AutomationEngine::shutdown`].
pub fn start(runtime: Runtime, automations: Vec<Automation>) -> Result<AutomationEngine> {
    let timezone = runtime.time_zone();
    start_with_timezone(runtime, automations, timezone)
}

/// Start the automation engine using a configured IANA timezone.
///
/// The engine subscribes to runtime events and native integration signals
/// before returning. Broadcast lag and an overflowing pending queue terminate
/// the listener and are reported by [`AutomationEngine::shutdown`].
pub fn start_with_timezone(
    runtime: Runtime,
    automations: Vec<Automation>,
    timezone: impl AsRef<str>,
) -> Result<AutomationEngine> {
    start_with_timezone_and_manager(runtime, automations, timezone, None)
}

/// Start the automation engine with a process-local webhook callback manager.
///
/// Webhook requests are delivered to the automation listener over a private,
/// bounded channel. They are not published as runtime events.
pub fn start_with_webhooks(
    runtime: Runtime,
    automations: Vec<Automation>,
    webhooks: WebhookManager,
) -> Result<AutomationEngine> {
    let timezone = runtime.time_zone();
    start_with_timezone_and_webhooks(runtime, automations, timezone, webhooks)
}

/// Start the automation engine with a configured timezone and webhook manager.
pub fn start_with_timezone_and_webhooks(
    runtime: Runtime,
    automations: Vec<Automation>,
    timezone: impl AsRef<str>,
    webhooks: WebhookManager,
) -> Result<AutomationEngine> {
    start_with_timezone_and_manager(runtime, automations, timezone, Some(webhooks))
}

fn start_with_timezone_and_manager(
    runtime: Runtime,
    automations: Vec<Automation>,
    timezone: impl AsRef<str>,
    webhook_manager: Option<WebhookManager>,
) -> Result<AutomationEngine> {
    validate(&automations)?;
    let timezone = timezone
        .as_ref()
        .parse::<Tz>()
        .map_err(|_| Error::InvalidInput("unknown automation time zone".into()))?;
    let template_previous = initial_template_states(&runtime, &automations)?;
    let receiver = runtime.subscribe();
    let signal_receiver = automations
        .iter()
        .flat_map(|automation| &automation.triggers)
        .any(|trigger| {
            matches!(
                trigger,
                Trigger::Mqtt { .. } | Trigger::PersistentNotification { .. }
            )
        })
        .then(|| runtime.subscribe_signals());

    let mut enabled = HashMap::with_capacity(automations.len());
    for automation in &automations {
        let key = automation_enabled_key(&automation.id);
        let current = match runtime.get_data(&key)? {
            Some(value) => value.as_bool().ok_or_else(|| {
                Error::InvalidInput(format!(
                    "persisted enable state for automation `{}` must be a boolean",
                    automation.id
                ))
            })?,
            None => {
                let initial = automation.initial_state.unwrap_or(true);
                runtime.set_data(&key, &Value::Bool(initial))?;
                initial
            }
        };
        enabled.insert(automation.id.clone(), current);
        publish_automation_state(&runtime, automation, current, Context::default())?;
    }
    let enabled = Arc::new(Mutex::new(enabled));
    let (webhook_control, webhook_receiver) = if let Some(manager) = webhook_manager {
        let (sender, receiver) = mpsc::channel(MAX_WEBHOOK_DELIVERIES);
        let control = Arc::new(WebhookControl {
            manager,
            runtime: runtime.clone(),
            automations: automations.clone(),
            delivery_tx: sender,
            registrations: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        });
        if let Err(error) = control.reconcile() {
            control.deactivate();
            return Err(error);
        }
        (Some(control), Some(receiver))
    } else {
        (None, None)
    };
    let controls = AutomationControl {
        enabled,
        automations: Arc::new(
            automations
                .iter()
                .cloned()
                .map(|automation| (automation.id.clone(), automation))
                .collect(),
        ),
        webhooks: webhook_control.as_ref().map(Arc::downgrade),
    };
    if let Err(error) = register_automation_services(&runtime, controls.clone()) {
        if let Some(webhooks) = &webhook_control {
            webhooks.deactivate();
        }
        return Err(error);
    }

    let engine_automations = automations.clone();
    let engine_controls = controls.clone();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let listener_cancel = cancel_tx.clone();
    let run_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        run_engine(EngineInput {
            runtime: run_runtime,
            automations,
            timezone,
            controls,
            template_previous,
            receiver,
            signal_receiver,
            webhook_receiver,
            cancel_rx,
            cancel_tx: listener_cancel,
        })
        .await
    });
    Ok(AutomationEngine {
        cancel: cancel_tx,
        task: Some(task),
        runtime,
        automations: engine_automations,
        controls: engine_controls,
        webhooks: webhook_control,
    })
}

/// Running automation engine. Call [`shutdown`](Self::shutdown) to cancel
/// delays and join all work.
pub struct AutomationEngine {
    cancel: tokio::sync::watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
    runtime: Runtime,
    automations: Vec<Automation>,
    controls: AutomationControl,
    webhooks: Option<Arc<WebhookControl>>,
}

impl AutomationEngine {
    /// Cancel event handling and active delays, then join every active run.
    /// Synchronous service handlers already in progress are joined and must
    /// return before shutdown completes.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(webhooks) = &self.webhooks {
            webhooks.deactivate();
        }
        self.cancel.send_replace(true);
        let task = self
            .task
            .take()
            .expect("automation listener handle is present until shutdown");
        let mut result = match task.await {
            Ok(result) => result,
            Err(error) => Err(Error::Service(format!(
                "automation listener task failed: {error}"
            ))),
        };
        let lineage = Arc::new(LineageRegistry::default());
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        for run in shutdown_runs(&self.runtime, &self.automations) {
            if !automation_is_enabled(&self.controls, &run.automation.id) {
                continue;
            }
            match conditions_match(&self.runtime, &run.automation.conditions, &run.variables) {
                Ok(true) => {
                    if let Err(error) = execute_automation(
                        self.runtime.clone(),
                        run,
                        Arc::clone(&lineage),
                        cancel_rx.clone(),
                    )
                    .await
                    {
                        if result.is_ok() {
                            result = Err(error);
                        } else {
                            tracing::warn!(%error, "shutdown automation failed after listener failure");
                        }
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    } else {
                        tracing::warn!(%error, "shutdown automation condition failed after listener failure");
                    }
                }
            }
        }
        result
    }
}

impl Drop for AutomationEngine {
    fn drop(&mut self) {
        if let Some(webhooks) = &self.webhooks {
            webhooks.deactivate();
        }
        self.cancel.send_replace(true);
    }
}

#[derive(Clone)]
struct PendingRun {
    automation: Automation,
    trigger_context: Context,
    lineage: Vec<String>,
    variables: Map<String, Value>,
}

#[derive(Clone)]
struct AutomationControl {
    enabled: Arc<Mutex<HashMap<String, bool>>>,
    automations: Arc<HashMap<String, Automation>>,
    // Runtime service callbacks retain AutomationControl, so this must stay
    // weak: WebhookControl owns a Runtime used to render configured IDs.
    webhooks: Option<Weak<WebhookControl>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WebhookTarget {
    automation_id: String,
    trigger_index: usize,
    id: String,
    idx: String,
    alias: Option<String>,
}

struct WebhookPlan {
    config: WebhookConfig,
    targets: Vec<WebhookTarget>,
}

struct RegisteredWebhook {
    config: WebhookConfig,
    targets: Vec<WebhookTarget>,
    _registration: WebhookRegistration,
}

struct WebhookDelivery {
    request: WebhookRequest,
    targets: Vec<WebhookTarget>,
}

struct WebhookControl {
    manager: WebhookManager,
    runtime: Runtime,
    automations: Vec<Automation>,
    delivery_tx: mpsc::Sender<WebhookDelivery>,
    registrations: Mutex<HashMap<String, RegisteredWebhook>>,
    stopped: AtomicBool,
}

impl WebhookControl {
    fn reconcile(&self) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let template = TemplateEngine::new(self.runtime.clone());
        let mut desired = Vec::<(String, WebhookPlan)>::new();
        let mut desired_index = HashMap::<String, usize>::new();

        for automation in &self.automations {
            let Some(variables) = webhook_setup_variables(&template, &self.runtime, automation)
            else {
                continue;
            };
            for (trigger_index, trigger) in automation.triggers.iter().enumerate() {
                let Trigger::Webhook {
                    webhook_id,
                    local_only,
                    allowed_methods,
                    enabled: trigger_enabled,
                    id,
                    alias,
                } = trigger
                else {
                    continue;
                };
                if !trigger_enabled {
                    continue;
                }
                let rendered_id = match template.render(webhook_id, &variables) {
                    Ok(webhook_id) => webhook_id,
                    Err(_) => {
                        tracing::warn!(
                            automation_id = %automation.id,
                            "webhook trigger ID template could not be rendered"
                        );
                        continue;
                    }
                };
                let target = WebhookTarget {
                    automation_id: automation.id.clone(),
                    trigger_index,
                    id: id.clone().unwrap_or_else(|| trigger_index.to_string()),
                    idx: trigger_index.to_string(),
                    alias: alias.clone(),
                };
                if let Some(index) = desired_index.get(&rendered_id).copied() {
                    desired[index].1.targets.push(target);
                    continue;
                }
                let config = WebhookConfig {
                    domain: "automation".into(),
                    name: automation
                        .alias
                        .clone()
                        .unwrap_or_else(|| automation.id.clone()),
                    local_only: *local_only,
                    allowed_methods: allowed_methods.clone().map(WebhookAllowedMethods::into_vec),
                };
                desired_index.insert(rendered_id.clone(), desired.len());
                desired.push((
                    rendered_id,
                    WebhookPlan {
                        config,
                        targets: vec![target],
                    },
                ));
            }
        }

        let mut registrations = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let remove = registrations
            .iter()
            .filter_map(|(webhook_id, current)| {
                let wanted = desired
                    .iter()
                    .find(|(desired_id, _)| desired_id == webhook_id)
                    .map(|(_, plan)| plan);
                (!wanted.is_some_and(|plan| {
                    plan.config == current.config && plan.targets == current.targets
                }))
                .then(|| webhook_id.clone())
            })
            .collect::<Vec<_>>();
        for webhook_id in remove {
            registrations.remove(&webhook_id);
        }

        let mut additions = Vec::new();
        for (webhook_id, plan) in desired {
            if registrations.contains_key(&webhook_id) {
                continue;
            }
            let sender = self.delivery_tx.clone();
            let targets = plan.targets.clone();
            let registration = self
                .manager
                .register(webhook_id.clone(), plan.config.clone(), move |request| {
                    let sender = sender.clone();
                    let targets = targets.clone();
                    async move {
                        sender
                            .try_send(WebhookDelivery { request, targets })
                            .map_err(|_| WebhookHandlerError)?;
                        Ok(None)
                    }
                })
                .map_err(|error| {
                    Error::Service(format!("webhook callback registration failed: {error}"))
                })?;
            additions.push((
                webhook_id,
                RegisteredWebhook {
                    config: plan.config,
                    targets: plan.targets,
                    _registration: registration,
                },
            ));
        }
        for (webhook_id, registration) in additions {
            registrations.insert(webhook_id, registration);
        }
        Ok(())
    }

    fn deactivate(&self) {
        self.stopped.store(true, Ordering::Release);
        self.registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
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

struct EngineInput {
    runtime: Runtime,
    automations: Vec<Automation>,
    timezone: Tz,
    controls: AutomationControl,
    template_previous: HashMap<(String, usize), bool>,
    receiver: broadcast::Receiver<Event>,
    signal_receiver: Option<broadcast::Receiver<Event>>,
    webhook_receiver: Option<mpsc::Receiver<WebhookDelivery>>,
    cancel_rx: watch::Receiver<bool>,
    cancel_tx: watch::Sender<bool>,
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

async fn run_engine(input: EngineInput) -> Result<()> {
    let EngineInput {
        runtime,
        automations,
        timezone,
        controls,
        mut template_previous,
        mut receiver,
        mut signal_receiver,
        mut webhook_receiver,
        mut cancel_rx,
        cancel_tx,
    } = input;
    let lineage = Arc::new(LineageRegistry::default());
    let mut active = JoinSet::<(u64, String, Result<()>)>::new();
    let mut scheduled = JoinSet::<(ScheduleIdentity, u64, Result<Option<PendingRun>>)>::new();
    let mut schedule_cancels = HashMap::<ScheduleIdentity, (u64, watch::Sender<bool>)>::new();
    let mut active_by_automation = HashMap::<String, HashMap<u64, watch::Sender<bool>>>::new();
    let mut pending = VecDeque::<PendingRun>::new();
    let mut failure = None;
    let mut next_run_id = 0u64;
    let mut next_schedule_id = 0u64;
    let mut ticker = tokio::time::interval_at(
        TokioInstant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consumed_start_triggers = HashSet::<(String, usize)>::new();

    loop {
        if failure.is_some() {
            break;
        }
        if *cancel_rx.borrow() {
            break;
        }
        while active.len() < MAX_ACTIVE_RUNS {
            let Some(index) = pending.iter().position(|run| {
                let active_count = active_by_automation
                    .get(&run.automation.id)
                    .map_or(0, HashMap::len);
                match run.automation.mode {
                    AutomationMode::Parallel => active_count < run.automation.max,
                    AutomationMode::Single | AutomationMode::Restart | AutomationMode::Queued => {
                        active_count == 0
                    }
                }
            }) else {
                break;
            };
            let run = pending
                .remove(index)
                .expect("eligible pending automation run exists");
            let run_id = next_run_id;
            next_run_id = next_run_id.wrapping_add(1);
            let automation_id = run.automation.id.clone();
            let (run_cancel_tx, run_cancel_rx) = watch::channel(false);
            active_by_automation
                .entry(automation_id.clone())
                .or_default()
                .insert(run_id, run_cancel_tx);
            let run_runtime = runtime.clone();
            let run_lineage = Arc::clone(&lineage);
            active.spawn(async move {
                let result = execute_automation(run_runtime, run, run_lineage, run_cancel_rx).await;
                (run_id, automation_id, result)
            });
        }

        tokio::select! {
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            joined = active.join_next(), if !active.is_empty() => {
                match joined {
                    Some(Ok((run_id, automation_id, result))) => {
                        if let Some(runs) = active_by_automation.get_mut(&automation_id) {
                            runs.remove(&run_id);
                            if runs.is_empty() {
                                active_by_automation.remove(&automation_id);
                            }
                        }
                        if let Err(error) = result {
                            tracing::warn!(automation_id = %automation_id, %error, "automation run failed");
                        }
                    }
                    Some(Err(error)) => tracing::error!(%error, "automation run task panicked"),
                    None => {}
                }
            }
            joined = scheduled.join_next(), if !scheduled.is_empty() => {
                match joined {
                    Some(Ok((identity, schedule_id, result))) => {
                        if schedule_cancels.get(&identity).is_some_and(|(current, _)| *current == schedule_id) {
                            schedule_cancels.remove(&identity);
                        }
                        match result {
                            Ok(Some(run)) if automation_is_enabled(&controls, &run.automation.id) => {
                                match conditions_match(&runtime, &run.automation.conditions, &run.variables) {
                                    Ok(true) => {
                                        if run.automation.mode == AutomationMode::Restart {
                                            cancel_automation_runs(&run.automation.id, &mut pending, &mut active_by_automation, &mut schedule_cancels);
                                        }
                                        if let Err(error) = enqueue_run(&mut pending, &active_by_automation, run) {
                                            failure = Some(error);
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => tracing::warn!(%error, "automation condition evaluation failed"),
                                }
                            }
                            Ok(Some(_)) | Ok(None) => {}
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    Some(Err(error)) => tracing::error!(%error, "automation for-duration task panicked"),
                    None => {}
                }
            }
            _ = ticker.tick() => {
                let now = chrono::Utc::now().with_timezone(&timezone);
                let time_event = synthetic_event(
                    "automation_time",
                    serde_json::json!({
                        "hour": now.hour(),
                        "minute": now.minute(),
                        "second": now.second(),
                        "weekday": now.weekday().number_from_monday()
                    }),
                    Context::default(),
                );
                for automation in &automations {
                    if !automation_is_enabled(&controls, &automation.id) {
                        continue;
                    }
                    for trigger in &automation.triggers {
                        let matched = match trigger {
                            Trigger::Time { at } => {
                                let times: Vec<&str> = match at {
                                    TimeAt::One(time) => vec![time],
                                    TimeAt::Many(times) => times.iter().map(String::as_str).collect(),
                                };
                                times.iter().any(|time| parse_clock_time(time) == Some((now.hour(), now.minute(), now.second())))
                            }
                            Trigger::TimePattern { hours, minutes, seconds } => {
                                matches_time_pattern(hours.as_ref(), now.hour())
                                    && matches_time_pattern(minutes.as_ref(), now.minute())
                                    && matches_time_pattern(seconds.as_ref(), now.second())
                            }
                            _ => false,
                        };
                        if !matched {
                            continue;
                        }
                        let variables = variables_for_trigger(&runtime, automation, Some(&time_event), "time", None);
                        match conditions_match(&runtime, &automation.conditions, &variables) {
                            Ok(true) => {
                                let run = PendingRun {
                                    automation: automation.clone(),
                                    trigger_context: time_event.context.clone(),
                                    lineage: Vec::new(),
                                    variables,
                                };
                                if run.automation.mode == AutomationMode::Restart {
                                    cancel_automation_runs(&run.automation.id, &mut pending, &mut active_by_automation, &mut schedule_cancels);
                                }
                                if let Err(error) = enqueue_run(&mut pending, &active_by_automation, run) {
                                    failure = Some(error);
                                    break;
                                }
                            }
                            Ok(false) => {}
                            Err(error) => tracing::warn!(%error, "automation condition evaluation failed"),
                        }
                    }
                    if failure.is_some() { break; }
                }
                if failure.is_some() { break; }
            }
            webhook = async {
                match webhook_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending::<Option<WebhookDelivery>>().await,
                }
            } => {
                match webhook {
                    Some(delivery) => {
                        if let Err(error) = enqueue_webhook_delivery(
                            &runtime,
                            &controls,
                            delivery,
                            &mut pending,
                            &mut active_by_automation,
                            &mut schedule_cancels,
                        ) {
                            failure = Some(error);
                            break;
                        }
                    }
                    None => webhook_receiver = None,
                }
            }
            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        let inherited_lineage = lineage.lookup(&event.context.id);
                        if event.event_type == "automation_trigger" {
                            if let Some(id) = event.data.get("automation_id").and_then(Value::as_str)
                                && let Some(automation) = automations.iter().find(|automation| automation.id == id)
                            {
                                if inherited_lineage.iter().any(|ancestor| ancestor == id) {
                                    continue;
                                }
                                if inherited_lineage.len() >= MAX_LINEAGE_DEPTH {
                                    tracing::warn!(automation_id = %id, "automation chain depth limit reached");
                                    continue;
                                }
                                let caller_variables = event.data.get("variables").and_then(Value::as_object).cloned();
                                let variables = variables_for_trigger(&runtime, automation, Some(&event), "service", caller_variables);
                                let skip_condition = event.data.get("skip_condition").and_then(Value::as_bool).unwrap_or(false);
                                let should_run = if skip_condition {
                                    true
                                } else {
                                    match conditions_match(&runtime, &automation.conditions, &variables) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            tracing::warn!(automation_id = %id, %error, "manual automation condition failed");
                                            false
                                        }
                                    }
                                };
                                if should_run {
                                    let run = PendingRun {
                                        automation: automation.clone(),
                                        trigger_context: event.context.clone(),
                                        lineage: inherited_lineage,
                                        variables,
                                    };
                                    if run.automation.mode == AutomationMode::Restart {
                                        cancel_automation_runs(&run.automation.id, &mut pending, &mut active_by_automation, &mut schedule_cancels);
                                    }
                                    if let Err(error) = enqueue_run(&mut pending, &active_by_automation, run) {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            continue;
                        }
                        for automation in &automations {
                            let mut new_start_receipts = HashSet::new();
                            if event.event_type == "homeassistant_started" {
                                for (trigger_index, trigger) in
                                    automation.triggers.iter().enumerate()
                                {
                                    if matches!(trigger, Trigger::HomeAssistant { event } if event == "start")
                                        && consumed_start_triggers
                                            .insert((automation.id.clone(), trigger_index))
                                    {
                                        new_start_receipts.insert(trigger_index);
                                    }
                                }
                            }
                            if !automation_is_enabled(&controls, &automation.id) {
                                continue;
                            }
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
                            for (trigger_index, trigger) in automation.triggers.iter().enumerate() {
                                if matches!(trigger, Trigger::HomeAssistant { event } if event == "start")
                                    && !new_start_receipts.contains(&trigger_index)
                                {
                                    continue;
                                }
                                let matched = if let Trigger::Template { value_template, r#for } = trigger {
                                    if event.event_type != "state_changed" { continue; }
                                    let variables = variables_for_trigger(&runtime, automation, Some(&event), "template", None);
                                    let value = match template_truthy(&runtime, value_template, &variables) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            tracing::warn!(automation_id = %automation.id, %error, "template trigger evaluation failed");
                                            false
                                        }
                                    };
                                    let was_true = template_previous.insert((automation.id.clone(), trigger_index), value).unwrap_or(false);
                                    (value && !was_true).then(|| TriggerMatch {
                                        variables,
                                        duration: r#for.as_ref().and_then(|duration| duration.duration_with_limit(MAX_TRIGGER_FOR).ok()),
                                        entity_id: None,
                                    })
                                } else {
                                    match match_event_trigger(&runtime, automation, trigger, &event, false) {
                                        Ok(matched) => {
                                            matched
                                        },
                                        Err(error) => {
                                            tracing::warn!(automation_id = %automation.id, %error, "automation trigger evaluation failed");
                                            None
                                        }
                                    }
                                };
                                let Some(matched) = matched else { continue; };
                                let run = PendingRun {
                                    automation: automation.clone(),
                                    trigger_context: event.context.clone(),
                                    lineage: inherited_lineage.clone(),
                                    variables: matched.variables,
                                };
                                if let Some(duration) = matched.duration.filter(|duration| !duration.is_zero()) {
                                    if scheduled.len() >= MAX_SCHEDULED_RUNS {
                                        failure = Some(Error::Service(format!(
                                            "automation scheduled trigger queue reached its limit of {MAX_SCHEDULED_RUNS}"
                                        )));
                                        break;
                                    }
                                    let identity = ScheduleIdentity {
                                        automation_id: automation.id.clone(),
                                        trigger_index,
                                        entity_id: matched.entity_id,
                                    };
                                    if let Some((_, existing)) = schedule_cancels.remove(&identity) {
                                        existing.send_replace(true);
                                    }
                                    let schedule_id = next_schedule_id;
                                    next_schedule_id = next_schedule_id.wrapping_add(1);
                                    let (schedule_cancel_tx, schedule_cancel_rx) = watch::channel(false);
                                    schedule_cancels.insert(identity.clone(), (schedule_id, schedule_cancel_tx));
                                    let schedule_runtime = runtime.clone();
                                    let schedule_trigger = trigger.clone();
                                    scheduled.spawn(async move {
                                        let result = wait_for_trigger_for(
                                            schedule_runtime,
                                            &schedule_trigger,
                                            &run.variables,
                                            duration,
                                            schedule_cancel_rx,
                                        ).await.map(|active| active.then_some(run));
                                        (identity, schedule_id, result)
                                    });
                                    continue;
                                }
                                match conditions_match(&runtime, &run.automation.conditions, &run.variables) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(error) => {
                                        tracing::warn!(automation_id = %run.automation.id, %error, "automation condition evaluation failed");
                                        continue;
                                    }
                                }
                                if run.automation.mode == AutomationMode::Restart {
                                    cancel_automation_runs(&run.automation.id, &mut pending, &mut active_by_automation, &mut schedule_cancels);
                                }
                                if let Err(error) = enqueue_run(&mut pending, &active_by_automation, run) {
                                    failure = Some(error);
                                    break;
                                }
                            }
                            if failure.is_some() { break; }
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
            signal = async {
                match signal_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match signal {
                    Ok(event) => {
                        let inherited_lineage = lineage.lookup(&event.context.id);
                        for automation in &automations {
                            if !automation_is_enabled(&controls, &automation.id)
                                || inherited_lineage.iter().any(|id| id == &automation.id)
                            {
                                continue;
                            }
                            for trigger in &automation.triggers {
                                let matched = if matches!(trigger, Trigger::PersistentNotification { .. }) {
                                    match persistent_notification_trigger_matches(
                                        &runtime,
                                        automation,
                                        trigger,
                                        &event,
                                    ) {
                                        Ok(matches) => matches,
                                        Err(error) => {
                                            tracing::warn!(automation_id = %automation.id, %error, "persistent notification trigger evaluation failed");
                                            Vec::new()
                                        }
                                    }
                                } else {
                                    match match_event_trigger(&runtime, automation, trigger, &event, true) {
                                        Ok(Some(matched)) => vec![matched],
                                        Ok(None) => Vec::new(),
                                        Err(error) => {
                                            tracing::warn!(automation_id = %automation.id, %error, "native signal trigger evaluation failed");
                                            Vec::new()
                                        }
                                    }
                                };
                                for matched in matched {
                                    let run = PendingRun {
                                        automation: automation.clone(),
                                        trigger_context: event.context.clone(),
                                        lineage: inherited_lineage.clone(),
                                        variables: matched.variables,
                                    };
                                    match conditions_match(&runtime, &run.automation.conditions, &run.variables) {
                                        Ok(true) => {}
                                        Ok(false) => continue,
                                        Err(error) => {
                                            tracing::warn!(automation_id = %run.automation.id, %error, "native signal automation condition failed");
                                            continue;
                                        }
                                    }
                                    if run.automation.mode == AutomationMode::Restart {
                                        cancel_automation_runs(&run.automation.id, &mut pending, &mut active_by_automation, &mut schedule_cancels);
                                    }
                                    if let Err(error) = enqueue_run(&mut pending, &active_by_automation, run) {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                                if failure.is_some() { break; }
                            }
                            if failure.is_some() { break; }
                        }
                        if failure.is_some() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        failure = Some(Error::Service(format!(
                            "automation signal listener lagged and missed {skipped} native signals"
                        )));
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        failure = Some(Error::Service("runtime signal stream closed".into()));
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
    for runs in active_by_automation.values() {
        for cancel in runs.values() {
            cancel.send_replace(true);
        }
    }
    for (_, cancel) in schedule_cancels.values() {
        cancel.send_replace(true);
    }
    while let Some(joined) = scheduled.join_next().await {
        if let Err(error) = joined {
            tracing::error!(%error, "automation for-duration task failed during shutdown");
        }
    }
    while let Some(joined) = active.join_next().await {
        match joined {
            Ok((_, automation_id, Err(error))) => {
                tracing::warn!(automation_id = %automation_id, %error, "automation run failed during shutdown");
            }
            Ok(_) => {}
            Err(error) => tracing::error!(%error, "automation run task failed during shutdown"),
        }
    }
    if let Some(error) = failure {
        tracing::error!(%error, "automation engine stopped after event processing failure");
        Err(error)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ScheduleIdentity {
    automation_id: String,
    trigger_index: usize,
    entity_id: Option<String>,
}

struct TriggerMatch {
    variables: Map<String, Value>,
    duration: Option<Duration>,
    entity_id: Option<String>,
}

fn enqueue_run(
    pending: &mut VecDeque<PendingRun>,
    active: &HashMap<String, HashMap<u64, watch::Sender<bool>>>,
    run: PendingRun,
) -> Result<bool> {
    let active_count = active.get(&run.automation.id).map_or(0, HashMap::len);
    let pending_count = pending
        .iter()
        .filter(|queued| queued.automation.id == run.automation.id)
        .count();
    match run.automation.mode {
        AutomationMode::Single if active_count + pending_count > 0 => {
            tracing::debug!(
                automation_id = %run.automation.id,
                "ignoring trigger while single-mode automation is active"
            );
            return Ok(false);
        }
        AutomationMode::Parallel | AutomationMode::Queued
            if active_count + pending_count >= run.automation.max =>
        {
            tracing::warn!(
                automation_id = %run.automation.id,
                mode = match run.automation.mode { AutomationMode::Queued => "queued", _ => "parallel" },
                max = run.automation.max,
                "automation mode run limit reached; skipping trigger"
            );
            return Ok(false);
        }
        _ => {}
    }
    if pending.len() >= MAX_PENDING_RUNS {
        return Err(Error::Service(format!(
            "automation pending run queue reached its limit of {MAX_PENDING_RUNS}"
        )));
    }
    pending.push_back(run);
    Ok(true)
}

fn enqueue_webhook_delivery(
    runtime: &Runtime,
    controls: &AutomationControl,
    delivery: WebhookDelivery,
    pending: &mut VecDeque<PendingRun>,
    active: &mut HashMap<String, HashMap<u64, watch::Sender<bool>>>,
    scheduled: &mut HashMap<ScheduleIdentity, (u64, watch::Sender<bool>)>,
) -> Result<()> {
    for target in delivery.targets {
        if !automation_is_enabled(controls, &target.automation_id) {
            continue;
        }
        let Some(automation) = controls.automations.get(&target.automation_id) else {
            continue;
        };
        if !matches!(
            automation.triggers.get(target.trigger_index),
            Some(Trigger::Webhook { enabled: true, .. })
        ) {
            continue;
        }
        let variables = variables_for_trigger_with_fields(
            runtime,
            automation,
            None,
            "webhook",
            None,
            Some(webhook_trigger_fields(&delivery.request, &target)),
        );
        match conditions_match(runtime, &automation.conditions, &variables) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                tracing::warn!(
                    automation_id = %automation.id,
                    "webhook automation condition evaluation failed"
                );
                continue;
            }
        }
        let run = PendingRun {
            automation: automation.clone(),
            trigger_context: Context::default(),
            lineage: Vec::new(),
            variables,
        };
        if run.automation.mode == AutomationMode::Restart {
            cancel_automation_runs(&run.automation.id, pending, active, scheduled);
        }
        enqueue_run(pending, active, run)?;
    }
    Ok(())
}

fn cancel_automation_runs(
    automation_id: &str,
    pending: &mut VecDeque<PendingRun>,
    active: &mut HashMap<String, HashMap<u64, watch::Sender<bool>>>,
    scheduled: &mut HashMap<ScheduleIdentity, (u64, watch::Sender<bool>)>,
) {
    if let Some(runs) = active.get(automation_id) {
        for cancel in runs.values() {
            cancel.send_replace(true);
        }
    }
    pending.retain(|run| run.automation.id != automation_id);
    scheduled.retain(|identity, (_, cancel)| {
        if identity.automation_id == automation_id {
            cancel.send_replace(true);
            false
        } else {
            true
        }
    });
}

async fn execute_automation(
    runtime: Runtime,
    run: PendingRun,
    lineage_registry: Arc<LineageRegistry>,
    cancel_rx: watch::Receiver<bool>,
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
    let sequence = automation_sequence(&run.automation.actions)?;
    run_sequence(runtime, sequence, run.variables, service_context, cancel_rx).await
}

async fn wait_for_trigger_for(
    runtime: Runtime,
    trigger: &Trigger,
    variables: &Map<String, Value>,
    duration: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<bool> {
    let mut events = runtime.subscribe();
    let deadline = TokioInstant::now() + duration;
    let initially_active = trigger_for_still_matches(&runtime, trigger, variables)?;
    if !initially_active {
        return Ok(false);
    }
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                let active = trigger_for_still_matches(&runtime, trigger, variables)?;
                return Ok(active);
            }
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() { return Ok(false); }
            }
            event = events.recv() => match event {
                Ok(_) => {
                    let active = trigger_for_still_matches(&runtime, trigger, variables)?;
                    if !active {
                        return Ok(false);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(Error::Service(format!(
                        "automation for-duration listener lagged and missed {skipped} runtime events"
                    )));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::Service("runtime event stream closed during trigger for".into()));
                }
            }
        }
    }
}

fn match_event_trigger(
    runtime: &Runtime,
    automation: &Automation,
    trigger: &Trigger,
    event: &Event,
    native_signal: bool,
) -> Result<Option<TriggerMatch>> {
    let platform = trigger_platform_name(trigger);
    let variables = match trigger {
        Trigger::HomeAssistant { event: expected }
            if expected == "start" && event.event_type == "homeassistant_started" =>
        {
            homeassistant_trigger_variables(
                runtime,
                automation,
                event,
                "start",
                "Home Assistant starting",
            )
        }
        _ => variables_for_trigger(runtime, automation, Some(event), platform, None),
    };
    let (matches, duration, entity_id) = match trigger {
        Trigger::State { r#for, .. } => {
            let entity = state_trigger_matches(trigger, event)?;
            (entity.is_some(), r#for, entity)
        }
        Trigger::NumericState { r#for, .. } => {
            let matched = numeric_state_trigger_matches(runtime, trigger, &variables, event)?;
            let current_entity = event.data.get("entity_id").and_then(Value::as_str);
            (matched, r#for, current_entity.map(str::to_owned))
        }
        Trigger::Template { .. } => (false, &None, None),
        Trigger::Event {
            event_type,
            event_data,
        } => (
            !native_signal
                && event.event_type == *event_type
                && event_data.as_ref().is_none_or(|required| {
                    required
                        .iter()
                        .all(|(key, value)| event.data.get(key) == Some(value))
                }),
            &None,
            None,
        ),
        Trigger::HomeAssistant { event: expected } => (
            !native_signal && expected == "start" && event.event_type == "homeassistant_started",
            &None,
            None,
        ),
        Trigger::Mqtt {
            topic,
            payload,
            value_template,
        } => {
            if !native_signal || event.event_type != "mqtt_message" {
                (false, &None, None)
            } else {
                let actual_topic = event.data.get("topic").and_then(Value::as_str);
                let actual_payload = event.data.get("payload").and_then(Value::as_str);
                let rendered = if let Some(template) = value_template {
                    let mut mqtt_variables = variables.clone();
                    mqtt_variables.insert(
                        "value".into(),
                        actual_payload.map_or(Value::Null, |value| Value::String(value.into())),
                    );
                    mqtt_variables.insert(
                        "payload".into(),
                        actual_payload.map_or(Value::Null, |value| Value::String(value.into())),
                    );
                    render_template_value(runtime, template, &mqtt_variables)?
                        .as_str()
                        .map(str::to_owned)
                } else {
                    actual_payload.map(str::to_owned)
                };
                (
                    actual_topic.is_some_and(|actual| mqtt_topic_matches(topic, actual))
                        && payload
                            .as_ref()
                            .is_none_or(|expected| rendered.as_deref() == Some(expected)),
                    &None,
                    None,
                )
            }
        }
        Trigger::PersistentNotification { .. } => (false, &None, None),
        Trigger::Time { .. } | Trigger::TimePattern { .. } => (false, &None, None),
        Trigger::Webhook { .. } => (false, &None, None),
    };
    if !matches {
        return Ok(None);
    }
    let duration = duration
        .as_ref()
        .map(|duration| {
            duration
                .duration_with_limit(MAX_TRIGGER_FOR)
                .map_err(Error::InvalidInput)
        })
        .transpose()?;
    Ok(Some(TriggerMatch {
        variables,
        duration,
        entity_id,
    }))
}

fn trigger_platform_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::State { .. } => "state",
        Trigger::NumericState { .. } => "numeric_state",
        Trigger::Template { .. } => "template",
        Trigger::Time { .. } => "time",
        Trigger::TimePattern { .. } => "time_pattern",
        Trigger::HomeAssistant { .. } => "homeassistant",
        Trigger::Mqtt { .. } => "mqtt",
        Trigger::PersistentNotification { .. } => "persistent_notification",
        Trigger::Event { .. } => "event",
        Trigger::Webhook { .. } => "webhook",
    }
}

fn persistent_notification_trigger_matches(
    runtime: &Runtime,
    automation: &Automation,
    trigger: &Trigger,
    event: &Event,
) -> Result<Vec<TriggerMatch>> {
    let Trigger::PersistentNotification {
        notification_id,
        update_type,
    } = trigger
    else {
        return Ok(Vec::new());
    };
    if event.event_type != PERSISTENT_NOTIFICATION_SIGNAL {
        return Ok(Vec::new());
    }
    let Some(actual_type) = event.data.get("type").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    let Some(actual_type) = parse_notification_update_type(actual_type) else {
        return Ok(Vec::new());
    };
    if update_type
        .as_ref()
        .is_some_and(|expected| !expected.allows(actual_type))
    {
        return Ok(Vec::new());
    }
    let Some(notifications) = event.data.get("notifications").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let notification_id = notification_id.as_deref().filter(|id| !id.is_empty());
    let mut matches = Vec::with_capacity(notifications.len());
    for notification in notifications.values() {
        if notification_id.is_some_and(|expected| {
            notification.get("notification_id").and_then(Value::as_str) != Some(expected)
        }) {
            continue;
        }
        let mut trigger_fields = Map::new();
        trigger_fields.insert(
            "update_type".into(),
            Value::String(actual_type.as_str().into()),
        );
        trigger_fields.insert("notification".into(), notification.clone());
        matches.push(TriggerMatch {
            variables: variables_for_trigger_with_fields(
                runtime,
                automation,
                None,
                "persistent_notification",
                None,
                Some(trigger_fields),
            ),
            duration: None,
            entity_id: None,
        });
    }
    Ok(matches)
}

fn parse_notification_update_type(value: &str) -> Option<NotificationUpdateType> {
    match value {
        "current" => Some(NotificationUpdateType::Current),
        "added" => Some(NotificationUpdateType::Added),
        "updated" => Some(NotificationUpdateType::Updated),
        "removed" => Some(NotificationUpdateType::Removed),
        _ => None,
    }
}

fn variables_for_trigger(
    runtime: &Runtime,
    automation: &Automation,
    event: Option<&Event>,
    platform: &str,
    additional: Option<Map<String, Value>>,
) -> Map<String, Value> {
    variables_for_trigger_with_fields(runtime, automation, event, platform, additional, None)
}

fn variables_for_trigger_with_fields(
    runtime: &Runtime,
    automation: &Automation,
    event: Option<&Event>,
    platform: &str,
    additional: Option<Map<String, Value>>,
    trigger_fields: Option<Map<String, Value>>,
) -> Map<String, Value> {
    let mut variables = automation.variables.clone();
    for (name, value) in &automation.trigger_variables {
        variables
            .entry(name.clone())
            .or_insert_with(|| value.clone());
    }
    if let Some(additional) = additional {
        variables.extend(additional);
    }
    let mut trigger = Map::new();
    trigger.insert("platform".into(), Value::String(platform.into()));
    if let Some(event) = event {
        trigger.insert("event_type".into(), Value::String(event.event_type.clone()));
        for (name, value) in &event.data {
            trigger.insert(name.clone(), value.clone());
        }
        if let Some(value) = event.data.get("old_state") {
            trigger.insert("from_state".into(), value.clone());
        }
        if let Some(value) = event.data.get("new_state") {
            trigger.insert("to_state".into(), value.clone());
        }
    }
    if let Some(trigger_fields) = trigger_fields {
        trigger.extend(trigger_fields);
    }
    variables.insert("trigger".into(), Value::Object(trigger));
    let template = TemplateEngine::new(runtime.clone());
    let raw_variables = variables.clone();
    for (name, value) in raw_variables {
        if name == "trigger" {
            continue;
        }
        match render_config_value(&template, &value, &variables) {
            Ok(value) => {
                variables.insert(name, value);
            }
            Err(error) => {
                tracing::warn!(automation_id = %automation.id, variable = %name, %error, "automation variable template failed");
            }
        }
    }
    variables
}

fn render_config_value(
    template: &TemplateEngine,
    value: &Value,
    variables: &Map<String, Value>,
) -> Result<Value> {
    match value {
        Value::String(text) if text.contains("{{") => template.render_value(text, variables),
        Value::String(text) if text.contains("{%") => {
            template.render(text, variables).map(Value::String)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| render_config_value(template, value, variables))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.clone(),
                    render_config_value(template, value, variables)?,
                ))
            })
            .collect::<Result<Map<String, Value>>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn synthetic_event(event_type: &str, data: Value, context: Context) -> Event {
    Event {
        event_type: event_type.into(),
        data: data.as_object().cloned().unwrap_or_default(),
        origin: "LOCAL".into(),
        time_fired: chrono::Utc::now(),
        context,
    }
}

fn trigger_state_values(event: &Event) -> Result<(Option<State>, Option<State>, Option<String>)> {
    if event.event_type != "state_changed" {
        return Ok((None, None, None));
    }
    let entity = event
        .data
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let parse = |name: &str| -> Result<Option<State>> {
        match event.data.get(name) {
            Some(Value::Null) | None => Ok(None),
            Some(value) => serde_json::from_value::<State>(value.clone())
                .map(Some)
                .map_err(|error| {
                    Error::InvalidInput(format!("malformed {name} in state_changed: {error}"))
                }),
        }
    };
    Ok((parse("old_state")?, parse("new_state")?, entity))
}

fn state_value(state: &State, attribute: Option<&str>) -> Option<String> {
    if let Some(attribute) = attribute {
        state.attributes.get(attribute).map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
    } else {
        Some(state.state.clone())
    }
}

fn state_trigger_matches(trigger: &Trigger, event: &Event) -> Result<Option<String>> {
    let Trigger::State {
        entity_id,
        from,
        to,
        not_from,
        not_to,
        attribute,
        ..
    } = trigger
    else {
        return Ok(None);
    };
    let (old_state, new_state, entity) = trigger_state_values(event)?;
    let Some(entity) = entity else {
        return Ok(None);
    };
    if !entity_id.as_slice().contains(&entity.as_str()) {
        return Ok(None);
    }
    let Some(new_state) = new_state else {
        return Ok(None);
    };
    if new_state.entity_id != entity
        || old_state
            .as_ref()
            .is_some_and(|state| state.entity_id != entity)
    {
        return Ok(None);
    }
    let old_value = old_state
        .as_ref()
        .and_then(|state| state_value(state, attribute.as_deref()));
    let new_value = state_value(&new_state, attribute.as_deref());
    if new_value.is_none() || old_value == new_value {
        return Ok(None);
    }
    if attribute.is_none()
        && old_state
            .as_ref()
            .is_some_and(|old| old.state == new_state.state)
    {
        // Preserve the prior state-trigger contract: reports that only update
        // unrelated attributes are not state transitions.
        return Ok(None);
    }
    let matches = from
        .as_ref()
        .is_none_or(|expected| old_value.as_ref() == Some(expected))
        && to
            .as_ref()
            .is_none_or(|expected| new_value.as_ref() == Some(expected))
        && not_from
            .as_ref()
            .is_none_or(|forbidden| old_value.as_ref() != Some(forbidden))
        && not_to
            .as_ref()
            .is_none_or(|forbidden| new_value.as_ref() != Some(forbidden));
    Ok(matches.then_some(entity))
}

fn numeric_state_trigger_matches(
    runtime: &Runtime,
    trigger: &Trigger,
    variables: &Map<String, Value>,
    event: &Event,
) -> Result<bool> {
    let Trigger::NumericState {
        entity_id,
        above,
        below,
        attribute,
        value_template,
        ..
    } = trigger
    else {
        return Ok(false);
    };
    let (old_state, new_state, entity) = trigger_state_values(event)?;
    let Some(entity) = entity else {
        return Ok(false);
    };
    if !entity_id.as_slice().contains(&entity.as_str()) {
        return Ok(false);
    }
    let (Some(old_state), Some(new_state)) = (old_state, new_state) else {
        return Ok(false);
    };
    if old_state.entity_id != entity || new_state.entity_id != entity {
        return Ok(false);
    }
    let old = numeric_value(
        runtime,
        &old_state,
        attribute.as_deref(),
        value_template.as_deref(),
        variables,
    )?;
    let new = numeric_value(
        runtime,
        &new_state,
        attribute.as_deref(),
        value_template.as_deref(),
        variables,
    )?;
    Ok(
        old.is_some_and(|value| !in_numeric_range(value, *above, *below))
            && new.is_some_and(|value| in_numeric_range(value, *above, *below)),
    )
}

fn in_numeric_range(value: f64, above: Option<f64>, below: Option<f64>) -> bool {
    value.is_finite()
        && above.is_none_or(|bound| value > bound)
        && below.is_none_or(|bound| value < bound)
}

fn numeric_value(
    runtime: &Runtime,
    state: &State,
    attribute: Option<&str>,
    value_template: Option<&str>,
    variables: &Map<String, Value>,
) -> Result<Option<f64>> {
    let value = if let Some(value_template) = value_template {
        let mut variables = variables.clone();
        if let Some(Value::Object(trigger)) = variables.get_mut("trigger") {
            trigger.insert(
                "to_state".into(),
                serde_json::to_value(state).unwrap_or(Value::Null),
            );
        }
        render_template_value(runtime, value_template, &variables)?
    } else if let Some(attribute) = attribute {
        let Some(value) = state.attributes.get(attribute) else {
            return Ok(None);
        };
        value.clone()
    } else {
        Value::String(state.state.clone())
    };
    let parsed = value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok())
    });
    Ok(parsed.filter(|number| number.is_finite()))
}

fn template_truthy(
    runtime: &Runtime,
    template: &str,
    variables: &Map<String, Value>,
) -> Result<bool> {
    Ok(value_truthy(&render_template_value(
        runtime, template, variables,
    )?))
}

fn render_template_value(
    runtime: &Runtime,
    template: &str,
    variables: &Map<String, Value>,
) -> Result<Value> {
    TemplateEngine::new(runtime.clone()).render_value(template, variables)
}

fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" | "false" | "off" | "no" | "none" | "null" => false,
            "true" | "on" | "yes" => true,
            _ => true,
        },
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn trigger_for_still_matches(
    runtime: &Runtime,
    trigger: &Trigger,
    variables: &Map<String, Value>,
) -> Result<bool> {
    match trigger {
        Trigger::State {
            attribute,
            to,
            not_to,
            ..
        } => {
            let Some(Value::Object(trigger_vars)) = variables.get("trigger") else {
                return Ok(false);
            };
            let Some(entity_id) = trigger_vars.get("entity_id").and_then(Value::as_str) else {
                return Ok(false);
            };
            let Some(current) = runtime.get_state(entity_id) else {
                return Ok(false);
            };
            let expected = trigger_vars
                .get("to_state")
                .and_then(|state| serde_json::from_value::<State>(state.clone()).ok())
                .and_then(|state| state_value(&state, attribute.as_deref()));
            let current_value = state_value(&current, attribute.as_deref());
            Ok(current_value.is_some()
                && current_value == expected
                && to
                    .as_ref()
                    .is_none_or(|expected| current_value.as_ref() == Some(expected))
                && not_to
                    .as_ref()
                    .is_none_or(|forbidden| current_value.as_ref() != Some(forbidden)))
        }
        Trigger::NumericState {
            above,
            below,
            attribute,
            value_template,
            ..
        } => {
            let Some(Value::Object(trigger_vars)) = variables.get("trigger") else {
                return Ok(false);
            };
            let Some(entity_id) = trigger_vars.get("entity_id").and_then(Value::as_str) else {
                return Ok(false);
            };
            let Some(current) = runtime.get_state(entity_id) else {
                return Ok(false);
            };
            Ok(numeric_value(
                runtime,
                &current,
                attribute.as_deref(),
                value_template.as_deref(),
                variables,
            )?
            .is_some_and(|value| in_numeric_range(value, *above, *below)))
        }
        Trigger::Template { value_template, .. } => {
            template_truthy(runtime, value_template, variables)
        }
        _ => Ok(false),
    }
}

fn conditions_match(
    runtime: &Runtime,
    conditions: &[Condition],
    variables: &Map<String, Value>,
) -> Result<bool> {
    for condition in conditions {
        if !condition_matches(runtime, condition, variables)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn condition_matches(
    runtime: &Runtime,
    condition: &Condition,
    variables: &Map<String, Value>,
) -> Result<bool> {
    match condition {
        Condition::State {
            entity_id,
            state,
            attribute,
        } => Ok(runtime.get_state(entity_id).is_some_and(|current| {
            if let Some(attribute) = attribute {
                current.attributes.get(attribute).is_some_and(|value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned)
                        == *state
                })
            } else {
                current.state == *state
            }
        })),
        Condition::NumericState {
            entity_id,
            above,
            below,
            attribute,
            value_template,
        } => {
            let Some(current) = runtime.get_state(entity_id) else {
                return Ok(false);
            };
            Ok(numeric_value(
                runtime,
                &current,
                attribute.as_deref(),
                value_template.as_deref(),
                variables,
            )?
            .is_some_and(|value| in_numeric_range(value, *above, *below)))
        }
        Condition::Template { value_template } => {
            template_truthy(runtime, value_template, variables)
        }
        Condition::And { conditions } => conditions_match(runtime, conditions, variables),
        Condition::Or { conditions } => {
            for condition in conditions {
                if condition_matches(runtime, condition, variables)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Condition::Not { conditions } => {
            Ok(!condition_matches(runtime, &conditions[0], variables)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{MemoryStore, ServiceResult};
    use hass_notifications::NotificationIntegration;
    use hass_webhook::WebhookOrigin;
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

    fn call_notification_service(runtime: &Runtime, service: &str, data: Value) {
        runtime
            .call_service(ServiceCall {
                domain: "persistent_notification".into(),
                service: service.into(),
                data: data.as_object().cloned().unwrap_or_default(),
                target: Map::new(),
                context: Context::default(),
            })
            .unwrap();
    }

    async fn receive_notification_run(
        receiver: &mut mpsc::UnboundedReceiver<ServiceCall>,
    ) -> ServiceCall {
        tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
            .await
            .expect("automation action should run before timeout")
            .expect("automation action channel should remain open")
    }

    async fn receive_notification_messages(
        receiver: &mut mpsc::UnboundedReceiver<ServiceCall>,
        count: usize,
    ) -> Vec<String> {
        let mut messages = Vec::with_capacity(count);
        for _ in 0..count {
            let call = receive_notification_run(receiver).await;
            messages.push(call.data["message"].as_str().unwrap().to_owned());
        }
        messages.sort();
        messages
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

    #[tokio::test]
    async fn numeric_template_triggers_share_rendered_variables_and_conditions() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        for (entity_id, state) in [
            ("sensor.score", "5"),
            ("binary_sensor.ready", "off"),
            ("input_boolean.allowed", "on"),
        ] {
            runtime
                .set_state(entity_id, state, Map::new(), Context::default(), false)
                .unwrap();
        }
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: score_crossed
  triggers:
    - platform: numeric_state
      entity_id: sensor.score
      above: 10
  conditions:
    - condition: template
      value_template: "{{ is_state('input_boolean.allowed', 'on') and note == 'score crossed' }}"
  variables:
    note: "{{ 'score crossed' }}"
  actions:
    - action: test.notify
      data:
        message: "{{ note }}"
- id: ready_template
  triggers:
    - trigger: template
      value_template: "{{ is_state('binary_sensor.ready', 'on') }}"
  variables:
    note: ready
  actions:
    - action: test.notify
      data:
        message: "{{ note }}"
"#,
            ),
        )
        .unwrap();

        runtime
            .set_state("sensor.score", "11", Map::new(), Context::default(), false)
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

        let mut messages = Vec::new();
        for _ in 0..2 {
            let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            messages.push(call.data["message"].as_str().unwrap().to_owned());
        }
        messages.sort();
        assert_eq!(messages, ["ready", "score crossed"]);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn for_triggers_cancel_when_numeric_or_attribute_value_changes() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let mut attributes = Map::new();
        attributes.insert("brightness".into(), Value::from(1));
        runtime
            .set_state("light.desk", "on", attributes, Context::default(), false)
            .unwrap();
        runtime
            .set_state("sensor.score", "5", Map::new(), Context::default(), false)
            .unwrap();
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: numeric_hold
  triggers:
    - trigger: numeric_state
      entity_id: sensor.score
      above: 10
      for: 0.08
  actions:
    - action: test.notify
      data:
        message: numeric
- id: brightness_hold
  triggers:
    - trigger: state
      entity_id: light.desk
      attribute: brightness
      to: "2"
      for: 0.08
  actions:
    - action: test.notify
      data:
        message: brightness
"#,
            ),
        )
        .unwrap();

        runtime
            .set_state("sensor.score", "11", Map::new(), Context::default(), false)
            .unwrap();
        let mut attributes = Map::new();
        attributes.insert("brightness".into(), Value::from(2));
        runtime
            .set_state("light.desk", "on", attributes, Context::default(), false)
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        runtime
            .set_state("sensor.score", "5", Map::new(), Context::default(), false)
            .unwrap();
        let mut attributes = Map::new();
        attributes.insert("brightness".into(), Value::from(1));
        runtime
            .set_state("light.desk", "on", attributes, Context::default(), false)
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(120)).await;
        assert!(receiver.try_recv().is_err());

        runtime
            .set_state("sensor.score", "12", Map::new(), Context::default(), false)
            .unwrap();
        let mut attributes = Map::new();
        attributes.insert("brightness".into(), Value::from(2));
        runtime
            .set_state("light.desk", "on", attributes, Context::default(), false)
            .unwrap();
        let mut messages = Vec::new();
        for _ in 0..2 {
            let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            messages.push(call.data["message"].as_str().unwrap().to_owned());
        }
        messages.sort();
        assert_eq!(messages, ["brightness", "numeric"]);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn time_and_time_pattern_triggers_use_configured_timezone() {
        use chrono::{Duration as ChronoDuration, Utc};

        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let timezone = "America/New_York";
        let tz = timezone.parse::<Tz>().unwrap();
        let at = (Utc::now() + ChronoDuration::seconds(3))
            .with_timezone(&tz)
            .format("%H:%M:%S")
            .to_string();
        let yaml = format!(
            r#"
- id: pattern_tick
  triggers:
    - trigger: time_pattern
      seconds: "/1"
  actions:
    - action: test.notify
      data:
        message: pattern
- id: exact_time
  triggers:
    - trigger: time
      at: "{at}"
  actions:
    - action: test.notify
      data:
        message: exact
"#
        );
        let engine = start_with_timezone(runtime.clone(), config(&yaml), timezone).unwrap();

        let first = tokio::time::timeout(StdDuration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.data["message"], "pattern");
        let mut found_exact = false;
        for _ in 0..4 {
            let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if call.data["message"] == "exact" {
                found_exact = true;
                break;
            }
        }
        assert!(
            found_exact,
            "the configured local wall-clock time must fire"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_and_shutdown_triggers_run_through_shared_executor() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: lifecycle
  triggers:
    - trigger: homeassistant
      event: start
    - trigger: homeassistant
      event: shutdown
  actions:
    - action: test.notify
      data:
        message: "{{ trigger.event }}"
        platform: "{{ trigger.platform }}"
        event_type: "{{ trigger.event_type }}"
        description: "{{ trigger.description }}"
"#,
            ),
        )
        .unwrap();
        runtime
            .fire_event("homeassistant_started", Map::new(), Context::default())
            .unwrap();
        let startup = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(startup.data["message"], "start");
        assert_eq!(startup.data["platform"], "homeassistant");
        assert_eq!(startup.data["event_type"], "homeassistant_started");
        assert_eq!(startup.data["description"], "Home Assistant starting");
        engine.shutdown().await.unwrap();
        let shutdown = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shutdown.data["message"], "shutdown");
        assert_eq!(shutdown.data["platform"], "homeassistant");
        assert_eq!(shutdown.data["event_type"], "homeassistant");
        assert_eq!(shutdown.data["description"], "Home Assistant stopping");
    }

    #[tokio::test]
    async fn homeassistant_start_waits_for_real_event_and_is_consumed_once() {
        let runtime = runtime();
        runtime
            .set_state(
                "input_boolean.ready",
                "on",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: startup_metadata
  variables:
    details: "{{ trigger.platform ~ '|' ~ trigger.event ~ '|' ~ trigger.description ~ '|' ~ trigger.event_type }}"
  trigger_variables:
    source_name: test
  triggers:
    - trigger: homeassistant
      event: start
  actions:
    - action: test.notify
      data:
        message: "{{ details }}"
        source_name: "{{ source_name }}"
- id: disabled_at_start
  initial_state: false
  triggers:
    - trigger: homeassistant
      event: start
  actions:
    - action: test.notify
      data:
        message: disabled
- id: condition_false_at_start
  conditions:
    - condition: state
      entity_id: input_boolean.ready
      state: "off"
  triggers:
    - trigger: homeassistant
      event: start
  actions:
    - action: test.notify
      data:
        message: condition
"#,
            ),
        )
        .unwrap();

        runtime
            .fire_event(
                "homeassistant",
                serde_json::json!({"event":"start"})
                    .as_object()
                    .unwrap()
                    .clone(),
                Context::default(),
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "a data field named start must not substitute for the lifecycle event"
        );

        runtime
            .fire_event("homeassistant_started", Map::new(), Context::default())
            .unwrap();
        let startup = receive_notification_run(&mut receiver).await;
        assert_eq!(
            startup.data["message"],
            "homeassistant|start|Home Assistant starting|homeassistant_started"
        );
        assert_eq!(startup.data["source_name"], "test");
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "disabled and condition-false start triggers must not run"
        );

        runtime
            .call_service(ServiceCall {
                domain: "automation".into(),
                service: "turn_on".into(),
                data: Map::new(),
                target: serde_json::json!({"entity_id":"automation.disabled_at_start"})
                    .as_object()
                    .unwrap()
                    .clone(),
                context: Context::default(),
            })
            .unwrap();
        runtime
            .set_state(
                "input_boolean.ready",
                "off",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        runtime
            .fire_event("homeassistant_started", Map::new(), Context::default())
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(80)).await;
        assert!(
            receiver.try_recv().is_err(),
            "each start trigger must consume its first receipt, including disabled or failed conditions"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn homeassistant_start_is_not_replayed_for_a_late_engine() {
        let runtime = runtime();
        runtime
            .fire_event("homeassistant_started", Map::new(), Context::default())
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime,
            config(
                r#"
- id: late_start
  triggers:
    - trigger: homeassistant
      event: start
  actions:
    - action: test.notify
      data:
        message: should_not_run
"#,
            ),
        )
        .unwrap();
        tokio::time::sleep(StdDuration::from_millis(60)).await;
        assert!(receiver.try_recv().is_err());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mqtt_trigger_uses_only_native_signals_and_exposes_filters() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: mqtt_legacy
  trigger:
    - platform: mqtt
      topic: "sensors/+/door"
      payload: open
  action:
    - service: test.notify
"#,
        );
        assert_eq!(mqtt_topics(&automations), ["sensors/+/door"]);
        let engine = start(runtime.clone(), automations).unwrap();
        let mut data = Map::new();
        data.insert("topic".into(), Value::String("sensors/front/door".into()));
        data.insert("payload".into(), Value::String("open".into()));
        data.insert(
            "payload_bytes".into(),
            Value::Array(vec![
                Value::from(b'o'),
                Value::from(b'p'),
                Value::from(b'e'),
                Value::from(b'n'),
            ]),
        );
        data.insert("qos".into(), Value::from(0));
        data.insert("retain".into(), Value::Bool(false));

        runtime
            .fire_event("mqtt_message", data.clone(), Context::default())
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(30)).await;
        assert!(receiver.try_recv().is_err());
        runtime
            .dispatch_signal("mqtt_message", data, Context::default())
            .unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_some()
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn persistent_notification_trigger_fans_out_change_payloads() {
        let runtime = runtime();
        let _notifications = NotificationIntegration::start(runtime.clone()).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: notification_changes
  mode: queued
  max: 32
  variables:
    summary: "{{ trigger.platform ~ '|' ~ trigger.update_type ~ '|' ~ trigger.notification.notification_id ~ '|' ~ trigger.notification.message }}"
  triggers:
    - trigger: persistent_notification
  actions:
    - action: test.notify
      data:
        message: "{{ summary }}"
        notification: "{{ trigger.notification }}"
"#,
            ),
        )
        .unwrap();

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"alpha","message":"created","title":"Alpha"}),
        );
        let added = receive_notification_run(&mut receiver).await;
        assert_eq!(
            added.data["message"],
            "persistent_notification|added|alpha|created"
        );
        assert_eq!(added.data["notification"]["notification_id"], "alpha");
        assert_eq!(added.data["notification"]["message"], "created");
        assert_eq!(added.data["notification"]["title"], "Alpha");
        assert!(added.data["notification"]["created_at"].as_str().is_some());

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"alpha","message":"updated"}),
        );
        let updated = receive_notification_run(&mut receiver).await;
        assert_eq!(
            updated.data["message"],
            "persistent_notification|updated|alpha|updated"
        );
        assert!(updated.data["notification"]["title"].is_null());

        call_notification_service(
            &runtime,
            "dismiss",
            serde_json::json!({"notification_id":"alpha"}),
        );
        assert_eq!(
            receive_notification_run(&mut receiver).await.data["message"],
            "persistent_notification|removed|alpha|updated"
        );

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"beta","message":"second"}),
        );
        let _ = receive_notification_run(&mut receiver).await;
        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"gamma","message":"third"}),
        );
        let _ = receive_notification_run(&mut receiver).await;
        call_notification_service(&runtime, "dismiss_all", serde_json::json!({}));
        assert_eq!(
            receive_notification_messages(&mut receiver, 2).await,
            [
                "persistent_notification|removed|beta|second",
                "persistent_notification|removed|gamma|third"
            ]
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn persistent_notification_trigger_filters_scalar_and_list_update_types() {
        let runtime = runtime();
        let _notifications = NotificationIntegration::start(runtime.clone()).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: empty_filters
  mode: queued
  max: 16
  triggers:
    - trigger: persistent_notification
      notification_id: ""
      update_type: []
  actions:
    - action: test.notify
      data:
        message: "{{ 'empty:' ~ trigger.notification.notification_id ~ ':' ~ trigger.update_type }}"
- id: scalar_filter
  mode: queued
  max: 16
  triggers:
    - platform: persistent_notification
      notification_id: wanted
      update_type: added
  actions:
    - action: test.notify
      data:
        message: "{{ 'scalar:' ~ trigger.notification.notification_id ~ ':' ~ trigger.update_type }}"
- id: list_filter
  mode: queued
  max: 16
  triggers:
    - trigger: persistent_notification
      notification_id: wanted
      update_type: [updated, removed]
  actions:
    - action: test.notify
      data:
        message: "{{ 'list:' ~ trigger.notification.notification_id ~ ':' ~ trigger.update_type }}"
"#,
            ),
        )
        .unwrap();

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"other","message":"other"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 1).await,
            ["empty:other:added"]
        );

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"wanted","message":"first"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 2).await,
            ["empty:wanted:added", "scalar:wanted:added"]
        );

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"wanted","message":"replaced"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 2).await,
            ["empty:wanted:updated", "list:wanted:updated"]
        );

        call_notification_service(
            &runtime,
            "dismiss",
            serde_json::json!({"notification_id":"wanted"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 2).await,
            ["empty:wanted:removed", "list:wanted:removed"]
        );

        call_notification_service(
            &runtime,
            "dismiss",
            serde_json::json!({"notification_id":"other"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 1).await,
            ["empty:other:removed"]
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn persistent_notification_trigger_does_not_replay_or_use_public_events() {
        let runtime = runtime();
        let _notifications = NotificationIntegration::start(runtime.clone()).unwrap();
        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"existing","message":"before startup"}),
        );

        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: current_only
  mode: queued
  triggers:
    - trigger: persistent_notification
      update_type: current
  actions:
    - action: test.notify
      data:
        message: current
- id: private_notification
  mode: queued
  triggers:
    - trigger: persistent_notification
  actions:
    - action: test.notify
      data:
        message: private_signal
- id: public_event
  mode: queued
  triggers:
    - trigger: event
      event_type: persistent_notifications_updated
  actions:
    - action: test.notify
      data:
        message: public_event
"#,
            ),
        )
        .unwrap();
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "startup must not replay a current notification"
        );

        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"after","message":"after startup"}),
        );
        assert_eq!(
            receive_notification_messages(&mut receiver, 1).await,
            ["private_signal"]
        );
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "private notification signals must not reach public event triggers or synthesize current"
        );

        runtime
            .fire_event(
                PERSISTENT_NOTIFICATION_SIGNAL,
                Map::new(),
                Context::default(),
            )
            .unwrap();
        assert_eq!(
            receive_notification_messages(&mut receiver, 1).await,
            ["public_event"]
        );
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "public events must not invoke persistent notification signal triggers"
        );
        engine.shutdown().await.unwrap();
    }

    #[test]
    fn persistent_notification_trigger_rejects_invalid_update_types() {
        let invalid: std::result::Result<Vec<Automation>, _> = serde_yaml::from_str(
            r#"
- id: invalid_update_type
  triggers:
    - trigger: persistent_notification
      update_type: changed
  actions:
    - action: test.notify
"#,
        );
        assert!(invalid.is_err());
    }

    #[tokio::test]
    async fn persistent_notification_trigger_preserves_causality_and_shutdown() {
        let runtime = runtime();
        let _notifications = NotificationIntegration::start(runtime.clone()).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start(
            runtime.clone(),
            config(
                r#"
- id: self
  mode: queued
  triggers:
    - trigger: event
      event_type: begin
    - trigger: persistent_notification
      notification_id: self
      update_type: added
  actions:
    - action: test.notify
      data:
        message: ran
    - action: persistent_notification.create
      data:
        notification_id: self
        message: generated by automation
"#,
            ),
        )
        .unwrap();
        runtime
            .fire_event("begin", Map::new(), Context::default())
            .unwrap();
        assert_eq!(
            receive_notification_messages(&mut receiver, 1).await,
            ["ran"]
        );
        tokio::time::sleep(StdDuration::from_millis(60)).await;
        assert!(
            receiver.try_recv().is_err(),
            "an automation must not re-enter itself from its notification service call"
        );

        engine.shutdown().await.unwrap();
        call_notification_service(
            &runtime,
            "create",
            serde_json::json!({"notification_id":"after_shutdown","message":"ignored"}),
        );
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(
            receiver.try_recv().is_err(),
            "shutdown must stop the signal listener"
        );
    }

    #[tokio::test]
    async fn automation_services_persist_enable_state_and_allow_manual_trigger() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: persisted
  triggers:
    - trigger: event
      event_type: go
  actions:
    - action: test.notify
      data:
        message: ran
"#,
        );
        let engine = start(runtime.clone(), automations.clone()).unwrap();

        let call = |service: &str| ServiceCall {
            domain: "automation".into(),
            service: service.into(),
            data: Map::new(),
            target: serde_json::json!({ "entity_id": "automation.persisted" })
                .as_object()
                .unwrap()
                .clone(),
            context: Context::default(),
        };
        runtime.call_service(call("turn_off")).unwrap();
        assert_eq!(
            runtime.get_state("automation.persisted").unwrap().state,
            "off"
        );
        assert_eq!(
            runtime.get_data("automation.enabled.persisted").unwrap(),
            Some(Value::Bool(false))
        );
        runtime
            .fire_event("go", Map::new(), Context::default())
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(40)).await;
        assert!(receiver.try_recv().is_err());
        engine.shutdown().await.unwrap();

        let engine = start(runtime.clone(), automations).unwrap();
        assert_eq!(
            runtime.get_state("automation.persisted").unwrap().state,
            "off"
        );
        runtime.call_service(call("turn_on")).unwrap();
        runtime
            .fire_event("go", Map::new(), Context::default())
            .unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_some()
        );
        tokio::time::sleep(StdDuration::from_millis(30)).await;
        runtime.call_service(call("turn_off")).unwrap();
        runtime.call_service(call("trigger")).unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_some()
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scheduling_modes_single_restart_queued_and_parallel_are_distinct() {
        async fn verify(mode: &str) {
            let runtime = runtime();
            let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
            register_notifier(&runtime, notify_tx);
            let (started_tx, mut started_rx) = mpsc::unbounded_channel::<String>();
            let (finished_tx, mut finished_rx) = mpsc::unbounded_channel::<()>();
            let active = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));
            let active_for_enter = Arc::clone(&active);
            let max_for_enter = Arc::clone(&max_active);
            runtime
                .register_service(
                    "test",
                    "enter",
                    Value::Null,
                    Arc::new(move |_runtime, call| {
                        let count = active_for_enter.fetch_add(1, Ordering::SeqCst) + 1;
                        max_for_enter.fetch_max(count, Ordering::SeqCst);
                        let message = call
                            .data
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let _ = started_tx.send(message.to_owned());
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();
            let active_for_leave = Arc::clone(&active);
            runtime
                .register_service(
                    "test",
                    "leave",
                    Value::Null,
                    Arc::new(move |_runtime, call| {
                        active_for_leave.fetch_sub(1, Ordering::SeqCst);
                        let _ = finished_tx.send(());
                        Ok(ServiceResult {
                            changed_states: Vec::new(),
                            response: None,
                            context: call.context.clone(),
                        })
                    }),
                )
                .unwrap();

            let event_type = format!("run_{mode}");
            let yaml = format!(
                r#"
- id: mode_test
  mode: {mode}
  max: 2
  triggers:
    - trigger: event
      event_type: {event_type}
  actions:
    - action: test.enter
      data:
        message: "{{{{ trigger.value }}}}"
    - delay: 0.15
    - action: test.leave
    - action: test.notify
      data:
        message: "{{{{ trigger.value }}}}"
"#
            );
            let engine = start(runtime.clone(), config(&yaml)).unwrap();
            let emit = |value: &str| {
                let mut data = Map::new();
                data.insert("value".into(), Value::String(value.into()));
                runtime
                    .fire_event(&event_type, data, Context::default())
                    .unwrap();
            };
            emit("first");
            assert_eq!(
                tokio::time::timeout(StdDuration::from_secs(2), started_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                "first"
            );
            emit("second");

            match mode {
                "single" => {
                    let first = tokio::time::timeout(StdDuration::from_secs(2), notify_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(first.data["message"], "first");
                    tokio::time::sleep(StdDuration::from_millis(220)).await;
                    assert!(notify_rx.try_recv().is_err());
                }
                "restart" => {
                    assert_eq!(
                        tokio::time::timeout(StdDuration::from_secs(2), started_rx.recv())
                            .await
                            .unwrap()
                            .unwrap(),
                        "second"
                    );
                    let call = tokio::time::timeout(StdDuration::from_secs(2), notify_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(call.data["message"], "second");
                    tokio::time::sleep(StdDuration::from_millis(30)).await;
                    assert!(notify_rx.try_recv().is_err());
                }
                "queued" => {
                    emit("third");
                    let first = tokio::time::timeout(StdDuration::from_secs(2), notify_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    let second = tokio::time::timeout(StdDuration::from_secs(2), notify_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(first.data["message"], "first");
                    assert_eq!(second.data["message"], "second");
                    tokio::time::sleep(StdDuration::from_millis(30)).await;
                    assert!(notify_rx.try_recv().is_err());
                }
                "parallel" => {
                    assert_eq!(
                        tokio::time::timeout(StdDuration::from_millis(80), started_rx.recv())
                            .await
                            .unwrap()
                            .unwrap(),
                        "second"
                    );
                    emit("third");
                    for _ in 0..2 {
                        tokio::time::timeout(StdDuration::from_secs(2), finished_rx.recv())
                            .await
                            .unwrap()
                            .unwrap();
                    }
                    assert_eq!(max_active.load(Ordering::SeqCst), 2);
                    for _ in 0..2 {
                        assert!(
                            tokio::time::timeout(StdDuration::from_secs(2), notify_rx.recv())
                                .await
                                .unwrap()
                                .is_some()
                        );
                    }
                    assert!(notify_rx.try_recv().is_err());
                }
                _ => unreachable!(),
            }
            engine.shutdown().await.unwrap();
        }

        for mode in ["single", "restart", "queued", "parallel"] {
            verify(mode).await;
        }
    }

    #[tokio::test]
    async fn queued_run_overflow_is_reported_during_shutdown() {
        let runtime = runtime();
        let automations = (0..140)
            .map(|index| Automation {
                id: format!("queue_{index}"),
                alias: None,
                triggers: vec![Trigger::Event {
                    event_type: "flood".into(),
                    event_data: None,
                }],
                conditions: Vec::new(),
                variables: Map::new(),
                trigger_variables: Map::new(),
                mode: AutomationMode::Parallel,
                max: 128,
                initial_state: None,
                actions: vec![Action::Delay(DelayAction {
                    delay: DelayValue::Seconds(20.0),
                })],
            })
            .collect();
        let engine = start(runtime.clone(), automations).unwrap();
        runtime
            .fire_event("flood", Map::new(), Context::default())
            .unwrap();
        // On a busy runner the initial automation state events can take more
        // than a second to drain. Wait for the actual terminal queue failure
        // before shutdown, which would otherwise cancel processing first.
        tokio::time::timeout(StdDuration::from_secs(15), async {
            while !engine.task.as_ref().unwrap().is_finished() {
                tokio::time::sleep(StdDuration::from_millis(1)).await;
            }
        })
        .await
        .expect("queue overflow must terminate the automation listener");
        let error = engine.shutdown().await.unwrap_err();
        assert!(error.to_string().contains("pending run queue"));
    }

    #[tokio::test]
    async fn webhook_triggers_render_json_form_and_query_metadata_privately() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let mut public_events = runtime.subscribe();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let manager = WebhookManager::new();
        let engine = start_with_webhooks(
            runtime.clone(),
            config(
                r#"
- id: json_hook
  triggers:
    - trigger: webhook
      webhook_id: json-secret-id
      local_only: true
      allowed_methods: [post]
      id: json-trigger-id
      alias: json-trigger-alias
  actions:
    - action: test.notify
      data:
        platform: "{{ trigger.platform }}"
        description: "{{ trigger.description }}"
        webhook_id: "{{ trigger.webhook_id }}"
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
        trigger_alias: "{{ trigger.alias }}"
        payload: "{{ trigger.json.message }}"
        origin: "{{ trigger.query.origin }}"
- id: form_hook
  triggers:
    - trigger: webhook
      webhook_id: form-secret-id
      local_only: false
      id: form-trigger-id
      alias: form-trigger-alias
  actions:
    - action: test.notify
      data:
        platform: "{{ trigger.platform }}"
        webhook_id: "{{ trigger.webhook_id }}"
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
        trigger_alias: "{{ trigger.alias }}"
        payload: "{{ trigger.data.answer }}"
        origin: "{{ trigger.query.origin }}"
"#,
            ),
            manager.clone(),
        )
        .unwrap();

        assert_eq!(manager.admin_inventory().len(), 2);
        let remote = WebhookRequest::new(
            "POST",
            WebhookOrigin {
                client_ip: Some("203.0.113.7".parse().unwrap()),
                is_local: false,
            },
        );
        assert_eq!(manager.dispatch("json-secret-id", remote).await.status, 200);
        assert_eq!(
            manager
                .dispatch(
                    "json-secret-id",
                    WebhookRequest::new(
                        "GET",
                        WebhookOrigin {
                            client_ip: Some("127.0.0.1".parse().unwrap()),
                            is_local: true,
                        },
                    ),
                )
                .await
                .status,
            405
        );
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );

        let mut json_request = WebhookRequest::new(
            "POST",
            WebhookOrigin {
                client_ip: Some("127.0.0.1".parse().unwrap()),
                is_local: true,
            },
        );
        json_request.body = br#"{"message":"private-json-body"}"#.to_vec();
        json_request.json = Some(serde_json::json!({"message":"private-json-body"}));
        json_request
            .query
            .insert("origin".into(), "private-json-query".into());
        assert_eq!(
            manager
                .dispatch("json-secret-id", json_request)
                .await
                .status,
            200
        );
        let json_call = receive_notification_run(&mut receiver).await;
        assert_eq!(json_call.data["platform"], "webhook");
        assert_eq!(json_call.data["description"], "webhook");
        assert_eq!(json_call.data["webhook_id"], "json-secret-id");
        assert_eq!(json_call.data["trigger_id"], "json-trigger-id");
        assert_eq!(json_call.data["trigger_idx"], "0");
        assert_eq!(json_call.data["trigger_alias"], "json-trigger-alias");
        assert_eq!(json_call.data["payload"], "private-json-body");
        assert_eq!(json_call.data["origin"], "private-json-query");

        let mut form_request = WebhookRequest::new("POST", WebhookOrigin::default());
        form_request
            .form
            .insert("answer".into(), "private-form-body".into());
        form_request
            .query
            .insert("origin".into(), "private-form-query".into());
        assert_eq!(
            manager
                .dispatch("form-secret-id", form_request)
                .await
                .status,
            200
        );
        let form_call = receive_notification_run(&mut receiver).await;
        assert_eq!(form_call.data["platform"], "webhook");
        assert_eq!(form_call.data["webhook_id"], "form-secret-id");
        assert_eq!(form_call.data["trigger_id"], "form-trigger-id");
        assert_eq!(form_call.data["trigger_idx"], "0");
        assert_eq!(form_call.data["trigger_alias"], "form-trigger-alias");
        assert_eq!(form_call.data["payload"], "private-form-body");
        assert_eq!(form_call.data["origin"], "private-form-query");

        let stored_events = store.events().unwrap();
        let serialized_events = stored_events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        for private_value in [
            "json-secret-id",
            "form-secret-id",
            "private-json-body",
            "private-form-body",
            "private-json-query",
            "private-form-query",
        ] {
            assert!(!serialized_events.contains(private_value));
        }
        while let Ok(event) = public_events.try_recv() {
            let serialized = serde_json::to_string(&event).unwrap();
            assert!(!serialized.contains("json-secret-id"));
            assert!(!serialized.contains("form-secret-id"));
            assert!(!serialized.contains("private-json-body"));
            assert!(!serialized.contains("private-form-body"));
        }

        engine.shutdown().await.unwrap();
        assert!(manager.admin_inventory().is_empty());
    }

    #[tokio::test]
    async fn disabled_webhook_automation_keeps_registration_but_unload_removes_it() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let manager = WebhookManager::new();
        let engine = start_with_webhooks(
            runtime.clone(),
            config(
                r#"
- id: initially_disabled
  initial_state: false
  triggers:
    - trigger: webhook
      webhook_id: disabled-automation-id
      local_only: false
  actions:
    - action: test.notify
      data:
        message: ran
- id: disabled_trigger
  triggers:
    - trigger: webhook
      webhook_id: disabled-trigger-id
      enabled: false
  actions:
    - action: test.notify
      data:
        message: wrong-trigger-ran
"#,
            ),
            manager.clone(),
        )
        .unwrap();

        assert_eq!(manager.admin_inventory().len(), 1);
        assert_eq!(
            manager
                .dispatch(
                    "disabled-automation-id",
                    WebhookRequest::new("POST", WebhookOrigin::default())
                )
                .await
                .status,
            200
        );
        assert_eq!(
            manager
                .dispatch(
                    "disabled-trigger-id",
                    WebhookRequest::new("POST", WebhookOrigin::default())
                )
                .await
                .status,
            200
        );
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );

        let call = |service: &str| ServiceCall {
            domain: "automation".into(),
            service: service.into(),
            data: Map::new(),
            target: serde_json::json!({ "entity_id": "automation.initially_disabled" })
                .as_object()
                .unwrap()
                .clone(),
            context: Context::default(),
        };
        runtime.call_service(call("turn_on")).unwrap();
        assert_eq!(manager.admin_inventory().len(), 1);
        manager
            .dispatch(
                "disabled-automation-id",
                WebhookRequest::new("POST", WebhookOrigin::default()),
            )
            .await;
        assert_eq!(
            receive_notification_run(&mut receiver).await.data["message"],
            "ran"
        );

        runtime.call_service(call("turn_off")).unwrap();
        manager
            .dispatch(
                "disabled-automation-id",
                WebhookRequest::new("POST", WebhookOrigin::default()),
            )
            .await;
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );
        assert_eq!(manager.admin_inventory().len(), 1);

        engine.shutdown().await.unwrap();
        assert!(manager.admin_inventory().is_empty());
        assert_eq!(Arc::strong_count(&store), 2);
    }

    #[tokio::test]
    async fn duplicate_webhook_triggers_share_first_registration_and_fan_out() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let manager = WebhookManager::new();
        let engine = start_with_webhooks(
            runtime,
            config(
                r#"
- id: first_duplicate
  alias: first name
  triggers:
    - trigger: webhook
      webhook_id: shared-webhook-id
      local_only: true
      allowed_methods: [POST]
      id: first-trigger
  actions:
    - action: test.notify
      data: {message: first}
- id: second_duplicate
  alias: second name
  triggers:
    - trigger: webhook
      webhook_id: shared-webhook-id
      local_only: false
      allowed_methods: [GET]
      id: second-trigger
  actions:
    - action: test.notify
      data: {message: second}
"#,
            ),
            manager.clone(),
        )
        .unwrap();

        let inventory = manager.admin_inventory();
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].name, "first name");
        assert!(inventory[0].local_only);
        assert_eq!(inventory[0].allowed_methods, ["POST"]);

        let remote = WebhookRequest::new(
            "POST",
            WebhookOrigin {
                client_ip: Some("203.0.113.15".parse().unwrap()),
                is_local: false,
            },
        );
        assert_eq!(
            manager.dispatch("shared-webhook-id", remote).await.status,
            200
        );
        assert_eq!(
            manager
                .dispatch(
                    "shared-webhook-id",
                    WebhookRequest::new("GET", WebhookOrigin::default())
                )
                .await
                .status,
            405
        );
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .dispatch(
                    "shared-webhook-id",
                    WebhookRequest::new(
                        "POST",
                        WebhookOrigin {
                            client_ip: Some("127.0.0.1".parse().unwrap()),
                            is_local: true,
                        },
                    )
                )
                .await
                .status,
            200
        );
        let mut messages = vec![
            receive_notification_run(&mut receiver).await.data["message"]
                .as_str()
                .unwrap()
                .to_owned(),
            receive_notification_run(&mut receiver).await.data["message"]
                .as_str()
                .unwrap()
                .to_owned(),
        ];
        messages.sort();
        assert_eq!(messages, ["first", "second"]);

        engine.shutdown().await.unwrap();
        assert!(manager.admin_inventory().is_empty());
    }

    #[tokio::test]
    async fn webhook_registration_conflict_cleans_up_prior_additions_and_drop_is_weak() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let manager = WebhookManager::new();
        let foreign_calls = Arc::new(AtomicUsize::new(0));
        let foreign_callback_calls = Arc::clone(&foreign_calls);
        let _foreign = manager
            .register(
                "occupied-id",
                WebhookConfig::new("external", "reserved callback"),
                move |_| {
                    let calls = Arc::clone(&foreign_callback_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(None)
                    }
                },
            )
            .unwrap();

        let conflicting = config(
            r#"
- id: conflicting
  triggers:
    - trigger: webhook
      webhook_id: first-addition-id
      local_only: false
    - trigger: webhook
      webhook_id: occupied-id
      local_only: false
  actions:
    - action: test.notify
      data: {message: ran}
"#,
        );
        let error = match start_with_webhooks(runtime.clone(), conflicting, manager.clone()) {
            Ok(engine) => {
                drop(engine);
                panic!("external ID conflicts must fail startup");
            }
            Err(error) => error,
        };
        assert!(error.to_string().contains("already registered"));
        assert_eq!(manager.admin_inventory().len(), 1);
        assert_eq!(manager.admin_inventory()[0].webhook_id, "occupied-id");
        manager
            .dispatch(
                "occupied-id",
                WebhookRequest::new("POST", WebhookOrigin::default()),
            )
            .await;
        assert_eq!(foreign_calls.load(Ordering::SeqCst), 1);

        let (sender, _receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let engine = start_with_webhooks(
            runtime.clone(),
            config(
                r#"
- id: drop_cleanup
  triggers:
    - trigger: webhook
      webhook_id: drop-cleanup-id
  actions:
    - action: test.notify
      data: {message: ran}
"#,
            ),
            manager.clone(),
        )
        .unwrap();
        assert_eq!(manager.admin_inventory().len(), 2);
        drop(engine);
        assert_eq!(manager.admin_inventory().len(), 1);
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while Arc::strong_count(&store) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the engine must release its Runtime lease");
        assert_eq!(manager.admin_inventory()[0].webhook_id, "occupied-id");
    }
}
