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

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc};
use chrono_tz::Tz;
use hass_core::{Context, Error, Event, Result, Runtime, ServiceCall, ServiceResult, State};
use hass_registry::Registries;
use hass_script::{run_sequence, validate_sequence};
use hass_sun::{
    LegacySunCondition, LegacySunEvent, Observer, PureSunCondition, SolarError, SolarEvent,
    SunPeriod, TwilightBand, evaluate_condition as evaluate_sun_condition, event_on_date,
    next_event, position_at,
};
use hass_template::TemplateEngine;
use hass_webhook::{
    WebhookConfig, WebhookHandlerError, WebhookManager, WebhookRegistration, WebhookRequest,
};
use serde::{Deserialize, Deserializer, Serialize};
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
    /// A legacy `platform: sun` trigger or a pinned Sun registry trigger such
    /// as `platform: sun.sunrise`.
    Sun {
        /// Original legacy or registry trigger type, preserved in trigger data.
        #[serde(default, skip_serializing_if = "is_legacy_sun_platform")]
        platform: String,
        /// `sunrise` or `sunset` for the legacy form; registry forms infer it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<String>,
        /// Signed duration from the solar event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<Value>,
        /// Legacy `before`/`after` offset sign selector.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset_type: Option<String>,
        /// Dawn/dusk selector: civil, nautical, or astronomical.
        #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
        twilight_type: Option<String>,
        /// Golden/blue-hour selector: any, morning, or evening.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        period: Option<String>,
        /// Numeric elevation selector for the two elevation-change triggers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<Value>,
        /// Require a crossed elevation threshold to remain active for this duration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#for: Option<DelayValue>,
        /// Pinned trigger registry options mapping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Value>,
        /// Generic Home Assistant trigger identifier, defaulting to its index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Generic Home Assistant trigger alias.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
    /// Match a public `tag_scanned` event from a native tag provider.
    Tag {
        tag_id: EntityIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<EntityIds>,
        #[serde(default = "default_tag_trigger_enabled")]
        enabled: TriggerEnabled,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
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

/// Common trigger enable value, either a literal boolean or a template.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TriggerEnabled {
    /// Enable or disable the trigger directly.
    Boolean(bool),
    /// Render the value when the automation engine attaches its triggers.
    Template(String),
}

fn default_tag_trigger_enabled() -> TriggerEnabled {
    TriggerEnabled::Boolean(true)
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

fn deserialize_optional_trigger_metadata<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(value) => Ok(Some(value)),
        _ => Err(serde::de::Error::custom(
            "trigger id and alias must be strings",
        )),
    }
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
    Sun {
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        offset: Option<Value>,
        #[serde(default)]
        offset_type: Option<String>,
        #[serde(default, rename = "type")]
        twilight_type: Option<String>,
        #[serde(default)]
        period: Option<String>,
        #[serde(default)]
        threshold: Option<Value>,
        #[serde(default)]
        r#for: Option<DelayValue>,
        #[serde(default)]
        options: Option<Value>,
        #[serde(default, deserialize_with = "deserialize_optional_trigger_metadata")]
        id: Option<String>,
        #[serde(default, deserialize_with = "deserialize_optional_trigger_metadata")]
        alias: Option<String>,
    },
    Tag {
        #[serde(deserialize_with = "deserialize_tag_ids")]
        tag_id: EntityIds,
        #[serde(default, deserialize_with = "deserialize_optional_tag_ids")]
        device_id: Option<EntityIds>,
        #[serde(default = "default_tag_trigger_enabled")]
        enabled: TriggerEnabled,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        alias: Option<String>,
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
            TriggerRepr::Sun {
                event,
                offset,
                offset_type,
                twilight_type,
                period,
                threshold,
                r#for,
                options,
                id,
                alias,
            } => Self::Sun {
                platform: "sun".into(),
                event,
                offset,
                offset_type,
                twilight_type,
                period,
                threshold,
                r#for,
                options,
                id,
                alias,
            },
            TriggerRepr::Tag {
                tag_id,
                device_id,
                enabled,
                id,
                alias,
            } => Self::Tag {
                tag_id,
                device_id,
                enabled,
                id,
                alias,
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
        let trigger_value = object.remove("trigger");
        let platform_value = object.remove("platform");
        let trigger_type = match (trigger_value, platform_value) {
            (Some(trigger), Some(platform)) => {
                let trigger = trigger.as_str().unwrap_or_default();
                let platform = platform.as_str().unwrap_or_default();
                if trigger == "sun" && platform.starts_with("sun.") {
                    Some(platform.to_owned())
                } else {
                    return Err(serde::de::Error::custom(
                        "trigger must use either `trigger` or legacy `platform`, not both",
                    ));
                }
            }
            (Some(trigger), None) | (None, Some(trigger)) => trigger.as_str().map(str::to_owned),
            (None, None) => None,
        };
        if let Some(trigger_type) = trigger_type.as_deref() {
            let registry_event = sun_registry_event(trigger_type);
            if trigger_type == "sun" || registry_event.is_some() {
                let sun_platform = Some(trigger_type.to_owned());
                object.insert("trigger".into(), Value::String("sun".into()));
                if let Some(event) = registry_event {
                    if object.contains_key("event") {
                        return Err(serde::de::Error::custom(
                            "Sun registry event triggers do not accept an `event` field",
                        ));
                    }
                    object.insert("event".into(), Value::String(event.into()));
                }
                let Some(options) = object.remove("options") else {
                    return parse_trigger_repr(value, sun_platform)
                        .map_err(serde::de::Error::custom);
                };
                let Some(options) = options.as_object() else {
                    return Err(serde::de::Error::custom(
                        "Sun trigger `options` must be a mapping",
                    ));
                };
                for (key, value) in options {
                    if object.contains_key(key) {
                        return Err(serde::de::Error::custom(format!(
                            "Sun trigger option `{key}` is specified more than once"
                        )));
                    }
                    object.insert(key.clone(), value.clone());
                }
                return parse_trigger_repr(value, sun_platform).map_err(serde::de::Error::custom);
            }
        }
        if let Some(trigger_type) = trigger_type {
            object.insert("trigger".into(), Value::String(trigger_type));
        }
        parse_trigger_repr(value, None).map_err(serde::de::Error::custom)
    }
}

fn parse_trigger_repr(
    value: Value,
    sun_platform: Option<String>,
) -> std::result::Result<Trigger, String> {
    let mut trigger = Trigger::from(
        serde_json::from_value::<TriggerRepr>(value).map_err(|error| error.to_string())?,
    );
    if let (
        Some(platform),
        Trigger::Sun {
            platform: stored,
            event,
            ..
        },
    ) = (sun_platform, &mut trigger)
    {
        *stored = platform.clone();
        if platform.starts_with("sun.") {
            *event = None;
        }
    }
    Ok(trigger)
}

fn is_legacy_sun_platform(platform: &String) -> bool {
    platform == "sun"
}

fn sun_registry_event(trigger_type: &str) -> Option<&'static str> {
    match trigger_type {
        "sun.sunrise" => Some("sunrise"),
        "sun.sunset" => Some("sunset"),
        "sun.solar_noon" => Some("solar_noon"),
        "sun.solar_midnight" => Some("solar_midnight"),
        "sun.dawn" => Some("dawn"),
        "sun.dusk" => Some("dusk"),
        "sun.golden_hour_started" => Some("golden_hour_started"),
        "sun.golden_hour_ended" => Some("golden_hour_ended"),
        "sun.blue_hour_started" => Some("blue_hour_started"),
        "sun.blue_hour_ended" => Some("blue_hour_ended"),
        "sun.midnight_sun_started" => Some("midnight_sun_started"),
        "sun.midnight_sun_ended" => Some("midnight_sun_ended"),
        "sun.polar_night_started" => Some("polar_night_started"),
        "sun.polar_night_ended" => Some("polar_night_ended"),
        "sun.elevation_changed" => Some("elevation_changed"),
        "sun.elevation_crossed_threshold" => Some("elevation_crossed_threshold"),
        _ => None,
    }
}

fn is_sun_elevation_trigger(trigger: &Trigger) -> bool {
    matches!(trigger, Trigger::Sun { platform, .. }
        if matches!(platform.as_str(), "sun.elevation_changed" | "sun.elevation_crossed_threshold"))
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

fn deserialize_tag_ids<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EntityIds, D::Error> {
    tag_ids_from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

fn deserialize_optional_tag_ids<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<EntityIds>, D::Error> {
    deserialize_tag_ids(deserializer).map(Some)
}

fn tag_ids_from_value(value: Value) -> std::result::Result<EntityIds, String> {
    // Pinned EnsureList maps null to an empty list; cv.string then accepts
    // scalar strings, numbers and booleans, including within a list.
    fn scalar(value: Value) -> std::result::Result<String, String> {
        match value {
            Value::String(value) => Ok(value),
            Value::Bool(value) => Ok(if value { "True" } else { "False" }.into()),
            Value::Number(value) if value.is_f64() => {
                let formatted = format!("{:?}", value.as_f64().ok_or("Invalid tag number")?);
                if let Some((base, exponent)) = formatted.split_once('e') {
                    let exponent = exponent.parse::<i32>().map_err(|_| "Invalid tag number")?;
                    Ok(format!(
                        "{base}e{}{abs:02}",
                        if exponent < 0 { "-" } else { "+" },
                        abs = exponent.unsigned_abs()
                    ))
                } else {
                    Ok(formatted)
                }
            }
            Value::Number(value) => Ok(value.to_string()),
            _ => Err("Tag IDs must be scalar string values".into()),
        }
    }
    match value {
        Value::Null => Ok(EntityIds::Many(Vec::new())),
        Value::Array(values) => values
            .into_iter()
            .map(scalar)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(EntityIds::Many),
        value => scalar(value).map(EntityIds::One),
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
    /// Legacy Home Assistant `sun` condition with before/after bounds.
    Sun {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_offset: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_offset: Option<Value>,
        /// Legacy condition options are nested in Home Assistant's config form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_up`.
    #[serde(rename = "sun.is_up")]
    SunIsUp {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_set`.
    #[serde(rename = "sun.is_set")]
    SunIsSet {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_ascending`.
    #[serde(rename = "sun.is_ascending")]
    SunIsAscending {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_descending`.
    #[serde(rename = "sun.is_descending")]
    SunIsDescending {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_night`.
    #[serde(rename = "sun.is_night")]
    SunIsNight {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_morning_twilight`.
    #[serde(rename = "sun.is_morning_twilight")]
    SunMorningTwilight {
        #[serde(default, rename = "type")]
        twilight_type: Option<String>,
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_evening_twilight`.
    #[serde(rename = "sun.is_evening_twilight")]
    SunEveningTwilight {
        #[serde(default, rename = "type")]
        twilight_type: Option<String>,
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_golden_hour`.
    #[serde(rename = "sun.is_golden_hour")]
    SunGoldenHour {
        #[serde(default)]
        period: Option<String>,
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_blue_hour`.
    #[serde(rename = "sun.is_blue_hour")]
    SunBlueHour {
        #[serde(default)]
        period: Option<String>,
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_midnight_sun`.
    #[serde(rename = "sun.is_midnight_sun")]
    SunIsMidnightSun {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.is_polar_night`.
    #[serde(rename = "sun.is_polar_night")]
    SunIsPolarNight {
        #[serde(default)]
        options: Option<Value>,
    },
    /// Pinned Sun condition registry entry `sun.elevation`, with a numeric
    /// threshold supplied by a fixed number.
    #[serde(rename = "sun.elevation")]
    SunElevation {
        #[serde(default)]
        threshold: Option<Value>,
        #[serde(default)]
        options: Option<Value>,
    },
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
    pub days: Option<f64>,
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
                    parts.days,
                    parts.hours,
                    parts.minutes,
                    parts.seconds,
                    parts.milliseconds,
                ];
                if values.iter().all(Option::is_none) {
                    return Err("delay duration object must contain a time component".into());
                }
                validate_components(&values)?;
                parts.days.unwrap_or_default() * 86_400.0
                    + parts.hours.unwrap_or_default() * 3600.0
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
                Trigger::Sun { .. } => validate_sun_trigger(trigger).map_err(|message| {
                    Error::InvalidInput(format!(
                        "automation `{}` has an invalid Sun trigger: {message}",
                        automation.id
                    ))
                })?,
                Trigger::Tag { .. } => {}
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
    if matches!(
        condition,
        Condition::Sun { .. }
            | Condition::SunIsUp { .. }
            | Condition::SunIsSet { .. }
            | Condition::SunIsAscending { .. }
            | Condition::SunIsDescending { .. }
            | Condition::SunIsNight { .. }
            | Condition::SunMorningTwilight { .. }
            | Condition::SunEveningTwilight { .. }
            | Condition::SunGoldenHour { .. }
            | Condition::SunBlueHour { .. }
            | Condition::SunIsMidnightSun { .. }
            | Condition::SunIsPolarNight { .. }
            | Condition::SunElevation { .. }
    ) {
        return validate_sun_condition(condition).map_err(|error| {
            Error::InvalidInput(format!(
                "automation `{automation_id}` has invalid Sun condition: {error}"
            ))
        });
    }
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

#[derive(Clone)]
struct TagTriggerPlan {
    tag_ids: HashSet<String>,
    device_ids: Option<HashSet<String>>,
    id: String,
    idx: String,
    alias: Option<String>,
}

fn prepare_tag_trigger_plans(
    runtime: &Runtime,
    automations: &[Automation],
    registries: Option<&Registries>,
) -> HashMap<(String, usize), TagTriggerPlan> {
    let mut plans = HashMap::new();
    for automation in automations {
        for (trigger_index, trigger) in automation.triggers.iter().enumerate() {
            let Trigger::Tag {
                tag_id,
                device_id,
                enabled,
                id,
                alias,
            } = trigger
            else {
                continue;
            };

            let is_enabled = match enabled {
                TriggerEnabled::Boolean(enabled) => *enabled,
                TriggerEnabled::Template(template) => {
                    let variables = variables_for_trigger(runtime, automation, None, "tag", None);
                    match template_truthy(runtime, template, &variables) {
                        Ok(enabled) => enabled,
                        Err(error) => {
                            tracing::error!(
                                automation_id = %automation.id,
                                trigger_index,
                                %error,
                                "tag trigger enabled template failed"
                            );
                            false
                        }
                    }
                }
            };
            if !is_enabled {
                continue;
            }

            let tag_ids = tag_id.as_slice().into_iter().map(str::to_owned).collect();
            // Home Assistant expands pre-migration composite IDs once when
            // attaching the trigger. Preserve the configured ID if the shared
            // registry has no surviving split members.
            let device_ids: Option<HashSet<String>> = device_id.as_ref().map(|ids| {
                let mut resolved = HashSet::new();
                for device_id in ids.as_slice() {
                    let split_devices = registries
                        .map(|registries| registries.devices_for_composite_device_id(device_id))
                        .unwrap_or_default();
                    if split_devices.is_empty() {
                        resolved.insert(device_id.to_owned());
                    } else {
                        resolved.extend(split_devices.into_iter().map(|device| device.id));
                    }
                }
                resolved
            });
            plans.insert(
                (automation.id.clone(), trigger_index),
                TagTriggerPlan {
                    tag_ids,
                    device_ids,
                    id: id.clone().unwrap_or_else(|| trigger_index.to_string()),
                    idx: trigger_index.to_string(),
                    alias: alias.clone(),
                },
            );
        }
    }
    plans
}

fn tag_trigger_match(
    runtime: &Runtime,
    automation: &Automation,
    plan: &TagTriggerPlan,
    event: &Event,
) -> Result<Option<TriggerMatch>> {
    if event.event_type != "tag_scanned"
        || !event
            .data
            .get("tag_id")
            .and_then(Value::as_str)
            .is_some_and(|tag_id| plan.tag_ids.contains(tag_id))
        || plan.device_ids.as_ref().is_some_and(|device_ids| {
            !event
                .data
                .get("device_id")
                .and_then(Value::as_str)
                .is_some_and(|device_id| device_ids.contains(device_id))
        })
    {
        return Ok(None);
    }

    let event = serde_json::to_value(event)
        .map_err(|error| Error::Service(format!("cannot serialize tag trigger event: {error}")))?;
    let mut fields = Map::new();
    fields.insert("event".into(), event);
    fields.insert("description".into(), Value::String("Tag scanned".into()));
    fields.insert("id".into(), Value::String(plan.id.clone()));
    fields.insert("idx".into(), Value::String(plan.idx.clone()));
    fields.insert(
        "alias".into(),
        plan.alias.clone().map_or(Value::Null, Value::String),
    );
    Ok(Some(TriggerMatch {
        variables: variables_for_trigger_with_fields(
            runtime,
            automation,
            None,
            "tag",
            None,
            Some(fields),
        ),
        duration: None,
        entity_id: None,
    }))
}

/// Optional services shared with the root application when starting
/// automations. Passing the application's `Registries` enables tag trigger
/// compatibility for composite device IDs when registry data is available.
#[derive(Default)]
pub struct AutomationFeatures {
    /// Configured IANA timezone. Missing values use the Runtime timezone.
    pub time_zone: Option<String>,
    /// Process-local callback manager for webhook triggers.
    pub webhooks: Option<WebhookManager>,
    /// The application's shared registries; the automation engine does not
    /// create a second registry instance.
    pub registries: Option<Registries>,
}

/// Start automations with optional shared application services.
pub fn start_with_features(
    runtime: Runtime,
    automations: Vec<Automation>,
    features: AutomationFeatures,
) -> Result<AutomationEngine> {
    let timezone = features.time_zone.unwrap_or_else(|| runtime.time_zone());
    start_with_timezone_and_manager_and_registries(
        runtime,
        automations,
        timezone,
        features.webhooks,
        features.registries,
    )
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
    start_with_timezone_and_manager_and_registries(
        runtime,
        automations,
        timezone,
        webhook_manager,
        None,
    )
}

fn start_with_timezone_and_manager_and_registries(
    runtime: Runtime,
    automations: Vec<Automation>,
    timezone: impl AsRef<str>,
    webhook_manager: Option<WebhookManager>,
    registries: Option<Registries>,
) -> Result<AutomationEngine> {
    validate(&automations)?;
    let timezone = timezone
        .as_ref()
        .parse::<Tz>()
        .map_err(|_| Error::InvalidInput("unknown automation time zone".into()))?;
    let template_previous = initial_template_states(&runtime, &automations)?;
    let tag_trigger_plans = prepare_tag_trigger_plans(&runtime, &automations, registries.as_ref());
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
            tag_trigger_plans,
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
    tag_trigger_plans: HashMap<(String, usize), TagTriggerPlan>,
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
        tag_trigger_plans,
        mut cancel_rx,
        cancel_tx,
    } = input;
    let lineage = Arc::new(LineageRegistry::default());
    let mut active = JoinSet::<(u64, String, Result<()>)>::new();
    let mut scheduled = JoinSet::<(ScheduleIdentity, u64, Result<Option<PendingRun>>)>::new();
    let mut schedule_cancels = HashMap::<ScheduleIdentity, (u64, watch::Sender<bool>)>::new();
    let mut sun_schedules = HashMap::<ScheduleIdentity, Option<DateTime<Utc>>>::new();
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
                let utc_now = Utc::now();
                let now = utc_now.with_timezone(&timezone);
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
                if failure.is_none() {
                    match due_sun_trigger_runs(&runtime, &automations, utc_now, &mut sun_schedules) {
                        Ok(runs) => {
                            for run in runs {
                                if !automation_is_enabled(&controls, &run.automation.id) {
                                    continue;
                                }
                                match conditions_match_at(
                                    &runtime,
                                    &run.automation.conditions,
                                    &run.variables,
                                    utc_now,
                                ) {
                                    Ok(true) => {
                                        if run.automation.mode == AutomationMode::Restart {
                                            cancel_automation_runs(
                                                &run.automation.id,
                                                &mut pending,
                                                &mut active_by_automation,
                                                &mut schedule_cancels,
                                            );
                                        }
                                        if let Err(error) = enqueue_run(
                                            &mut pending,
                                            &active_by_automation,
                                            run,
                                        ) {
                                            failure = Some(error);
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => tracing::warn!(%error, "automation Sun condition evaluation failed"),
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "automation Sun schedule calculation failed"),
                    }
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
                        if event.event_type == "core_config_updated" {
                            sun_schedules.clear();
                            continue;
                        }
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
                                let matched = if matches!(trigger, Trigger::Tag { .. }) {
                                    match tag_trigger_plans
                                        .get(&(automation.id.clone(), trigger_index))
                                    {
                                        Some(plan) => {
                                            match tag_trigger_match(&runtime, automation, plan, &event)
                                            {
                                                Ok(matched) => matched,
                                                Err(error) => {
                                                    tracing::warn!(
                                                        automation_id = %automation.id,
                                                        %error,
                                                        "tag trigger evaluation failed"
                                                    );
                                                    None
                                                }
                                            }
                                        }
                                        None => None,
                                    }
                                } else if let Trigger::Template {
                                    value_template,
                                    r#for,
                                } = trigger
                                {
                                    if event.event_type != "state_changed" {
                                        continue;
                                    }
                                    let variables = variables_for_trigger(
                                        &runtime,
                                        automation,
                                        Some(&event),
                                        "template",
                                        None,
                                    );
                                    let value = match template_truthy(
                                        &runtime,
                                        value_template,
                                        &variables,
                                    ) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            tracing::warn!(
                                                automation_id = %automation.id,
                                                %error,
                                                "template trigger evaluation failed"
                                            );
                                            false
                                        }
                                    };
                                    let was_true = template_previous
                                        .insert((automation.id.clone(), trigger_index), value)
                                        .unwrap_or(false);
                                    (value && !was_true).then(|| TriggerMatch {
                                        variables,
                                        duration: r#for.as_ref().and_then(|duration| {
                                            duration.duration_with_limit(MAX_TRIGGER_FOR).ok()
                                        }),
                                        entity_id: None,
                                    })
                                } else {
                                    match match_event_trigger(
                                        &runtime,
                                        automation,
                                        trigger,
                                        trigger_index,
                                        &event,
                                        false,
                                    ) {
                                        Ok(matched) => matched,
                                        Err(error) => {
                                            tracing::warn!(
                                                automation_id = %automation.id,
                                                %error,
                                                "automation trigger evaluation failed"
                                            );
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
                            for (trigger_index, trigger) in
                                automation.triggers.iter().enumerate()
                            {
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
                                    match match_event_trigger(
                                        &runtime,
                                        automation,
                                        trigger,
                                        trigger_index,
                                        &event,
                                        true,
                                    ) {
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
    trigger_index: usize,
    event: &Event,
    native_signal: bool,
) -> Result<Option<TriggerMatch>> {
    if is_sun_elevation_trigger(trigger) {
        return sun_elevation_trigger_match(
            runtime,
            automation,
            trigger,
            trigger_index,
            event,
            native_signal,
        );
    }
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
        _ => variables_for_trigger(runtime, automation, Some(event), &platform, None),
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
        Trigger::PersistentNotification { .. } | Trigger::Sun { .. } => (false, &None, None),
        Trigger::Time { .. } | Trigger::TimePattern { .. } => (false, &None, None),
        Trigger::Tag { .. } => (false, &None, None),
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

fn sun_elevation_trigger_match(
    runtime: &Runtime,
    automation: &Automation,
    trigger: &Trigger,
    trigger_index: usize,
    event: &Event,
    native_signal: bool,
) -> Result<Option<TriggerMatch>> {
    let Trigger::Sun {
        platform,
        threshold,
        r#for,
        id,
        alias,
        ..
    } = trigger
    else {
        return Ok(None);
    };
    if native_signal || event.event_type != "state_changed" {
        return Ok(None);
    }
    let (old_state, new_state, entity_id) = trigger_state_values(event)?;
    if entity_id.as_deref() != Some("sun.sun") {
        return Ok(None);
    }
    let (Some(old_state), Some(new_state)) = (old_state, new_state) else {
        // EntityNumericalStateTriggerBase ignores first creation and removal
        // events, since it requires both states and an eligible origin state.
        return Ok(None);
    };
    if old_state.entity_id != "sun.sun"
        || new_state.entity_id != "sun.sun"
        || matches!(old_state.state.as_str(), "unknown" | "unavailable")
        || matches!(new_state.state.as_str(), "unknown" | "unavailable")
    {
        return Ok(None);
    }
    let Some(new_elevation) = sun_elevation_from_state(&new_state) else {
        return Ok(None);
    };
    let threshold = threshold
        .as_ref()
        .ok_or_else(|| Error::InvalidInput("Sun elevation trigger requires a threshold".into()))?;
    let parsed_threshold =
        parse_sun_elevation_threshold(threshold, platform == "sun.elevation_changed")
            .map_err(Error::InvalidInput)?;
    let old_elevation = sun_elevation_from_state(&old_state);
    let new_matches = sun_elevation_in_threshold(runtime, &parsed_threshold, new_elevation);
    let matched = if platform == "sun.elevation_changed" {
        old_elevation != Some(new_elevation) && new_matches
    } else {
        !old_elevation
            .is_some_and(|value| sun_elevation_in_threshold(runtime, &parsed_threshold, value))
            && new_matches
    };
    if !matched {
        return Ok(None);
    }
    let duration = r#for
        .as_ref()
        .map(|duration| {
            duration
                .duration_with_limit(MAX_TRIGGER_FOR)
                .map_err(Error::InvalidInput)
        })
        .transpose()?;
    let mut fields = Map::new();
    fields.insert(
        "id".into(),
        Value::String(id.clone().unwrap_or_else(|| trigger_index.to_string())),
    );
    fields.insert("idx".into(), Value::String(trigger_index.to_string()));
    fields.insert(
        "alias".into(),
        alias.clone().map_or(Value::Null, Value::String),
    );
    let variables = variables_for_trigger_with_fields(
        runtime,
        automation,
        Some(event),
        platform,
        None,
        Some(fields),
    );
    Ok(Some(TriggerMatch {
        variables,
        duration,
        entity_id: Some("sun.sun".into()),
    }))
}

fn trigger_platform_name(trigger: &Trigger) -> String {
    match trigger {
        Trigger::State { .. } => "state".into(),
        Trigger::NumericState { .. } => "numeric_state".into(),
        Trigger::Template { .. } => "template".into(),
        Trigger::Time { .. } => "time".into(),
        Trigger::TimePattern { .. } => "time_pattern".into(),
        Trigger::HomeAssistant { .. } => "homeassistant".into(),
        Trigger::Mqtt { .. } => "mqtt".into(),
        Trigger::PersistentNotification { .. } => "persistent_notification".into(),
        Trigger::Event { .. } => "event".into(),
        Trigger::Sun { platform, .. } => platform.clone(),
        Trigger::Tag { .. } => "tag".into(),
        Trigger::Webhook { .. } => "webhook".into(),
    }
}

fn validate_sun_trigger(trigger: &Trigger) -> std::result::Result<(), String> {
    let Trigger::Sun {
        platform,
        event,
        offset,
        offset_type,
        twilight_type,
        period,
        threshold,
        r#for,
        options,
        ..
    } = trigger
    else {
        return Ok(());
    };
    if options
        .as_ref()
        .is_some_and(|value| !value.as_object().is_some_and(Map::is_empty))
    {
        return Err("trigger options must be provided as supported top-level fields".into());
    }
    if platform == "sun" && offset_type.is_some() {
        return Err("legacy Sun triggers do not accept offset_type".into());
    }
    if offset_type
        .as_deref()
        .is_some_and(|value| !matches!(value, "before" | "after"))
    {
        return Err("offset_type must be `before` or `after`".into());
    }
    if let Some(offset) = offset {
        parse_sun_offset(offset)?;
    }
    if platform == "sun" {
        if !matches!(event.as_deref(), Some("sunrise" | "sunset")) {
            return Err("legacy event must be `sunrise` or `sunset`".into());
        }
        if twilight_type.is_some() || period.is_some() || threshold.is_some() || r#for.is_some() {
            return Err("legacy Sun triggers do not accept type, period, threshold, or for".into());
        }
    } else if matches!(
        platform.as_str(),
        "sun.elevation_changed" | "sun.elevation_crossed_threshold"
    ) {
        let changed = platform == "sun.elevation_changed";
        if event.is_some()
            || offset.is_some()
            || offset_type.is_some()
            || twilight_type.is_some()
            || period.is_some()
        {
            return Err("Sun elevation triggers accept only threshold and optional for".into());
        }
        let threshold = threshold
            .as_ref()
            .ok_or_else(|| "Sun elevation trigger requires a threshold".to_owned())?;
        parse_sun_elevation_threshold(threshold, changed)?;
        if changed && r#for.is_some() {
            return Err("sun.elevation_changed does not accept for".into());
        }
        if let Some(duration) = r#for {
            duration
                .duration_with_limit(MAX_TRIGGER_FOR)
                .map_err(|error| format!("invalid Sun elevation trigger for duration: {error}"))?;
        }
    } else if !matches!(
        platform.as_str(),
        "sun.sunrise"
            | "sun.sunset"
            | "sun.solar_noon"
            | "sun.solar_midnight"
            | "sun.dawn"
            | "sun.dusk"
            | "sun.golden_hour_started"
            | "sun.golden_hour_ended"
            | "sun.blue_hour_started"
            | "sun.blue_hour_ended"
            | "sun.midnight_sun_started"
            | "sun.midnight_sun_ended"
            | "sun.polar_night_started"
            | "sun.polar_night_ended"
    ) {
        return Err(format!("registry trigger `{platform}` is not supported"));
    } else if event.is_some() || threshold.is_some() || r#for.is_some() {
        return Err("Sun event triggers do not accept event, threshold, or for fields".into());
    }
    if matches!(platform.as_str(), "sun.dawn" | "sun.dusk")
        && twilight_type
            .as_deref()
            .is_some_and(|value| !matches!(value, "civil" | "nautical" | "astronomical"))
    {
        return Err("type must be `civil`, `nautical`, or `astronomical`".into());
    }
    if twilight_type.is_some() && !matches!(platform.as_str(), "sun.dawn" | "sun.dusk") {
        return Err("type is only supported by dawn and dusk triggers".into());
    }
    if matches!(
        platform.as_str(),
        "sun.golden_hour_started"
            | "sun.golden_hour_ended"
            | "sun.blue_hour_started"
            | "sun.blue_hour_ended"
    ) && period
        .as_deref()
        .is_some_and(|value| !matches!(value, "any" | "morning" | "evening"))
    {
        return Err("period must be `any`, `morning`, or `evening`".into());
    }
    if period.is_some()
        && !matches!(
            platform.as_str(),
            "sun.golden_hour_started"
                | "sun.golden_hour_ended"
                | "sun.blue_hour_started"
                | "sun.blue_hour_ended"
        )
    {
        return Err("period is only supported by golden and blue hour triggers".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum SunElevationThresholdBound {
    Number(f64),
    Entity(String),
}

#[derive(Clone, Debug, PartialEq)]
enum SunElevationThreshold {
    Any,
    Above(SunElevationThresholdBound),
    Below(SunElevationThresholdBound),
    Between {
        lower: SunElevationThresholdBound,
        upper: SunElevationThresholdBound,
    },
    Outside {
        lower: SunElevationThresholdBound,
        upper: SunElevationThresholdBound,
    },
}

fn parse_sun_elevation_threshold(
    value: &Value,
    allow_any: bool,
) -> std::result::Result<SunElevationThreshold, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Sun elevation threshold must be a mapping".to_owned())?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "value" | "value_min" | "value_max"))
    {
        return Err("Sun elevation threshold contains an unsupported field".into());
    }
    let threshold_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "Sun elevation threshold requires a string type".to_owned())?;
    match threshold_type {
        "any" if allow_any && object.len() == 1 => Ok(SunElevationThreshold::Any),
        "any" => Err("threshold type `any` is only allowed for sun.elevation_changed".into()),
        "above" | "below" => {
            if object.contains_key("value_min") || object.contains_key("value_max") {
                return Err("above/below thresholds do not accept value_min or value_max".into());
            }
            let bound = parse_sun_elevation_bound(
                object
                    .get("value")
                    .ok_or_else(|| "above/below threshold requires value".to_owned())?,
            )?;
            if threshold_type == "above" {
                Ok(SunElevationThreshold::Above(bound))
            } else {
                Ok(SunElevationThreshold::Below(bound))
            }
        }
        "between" | "outside" => {
            if object.contains_key("value") {
                return Err("between/outside thresholds do not accept value".into());
            }
            let lower = parse_sun_elevation_bound(
                object
                    .get("value_min")
                    .ok_or_else(|| "between/outside threshold requires value_min".to_owned())?,
            )?;
            let upper = parse_sun_elevation_bound(
                object
                    .get("value_max")
                    .ok_or_else(|| "between/outside threshold requires value_max".to_owned())?,
            )?;
            if let (
                SunElevationThresholdBound::Number(lower),
                SunElevationThresholdBound::Number(upper),
            ) = (&lower, &upper)
                && lower > upper
            {
                return Err("value_min must not be greater than value_max".into());
            }
            if threshold_type == "between" {
                Ok(SunElevationThreshold::Between { lower, upper })
            } else {
                Ok(SunElevationThreshold::Outside { lower, upper })
            }
        }
        _ => Err("threshold type must be any, above, below, between, or outside".into()),
    }
}

fn parse_sun_elevation_bound(
    value: &Value,
) -> std::result::Result<SunElevationThresholdBound, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Sun elevation threshold bound must be a mapping".to_owned())?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "active_choice" | "number" | "entity" | "unit_of_measurement"
        )
    }) {
        return Err("Sun elevation threshold bound contains an unsupported field".into());
    }
    let active_choice = match object.get("active_choice") {
        Some(Value::String(choice)) => Some(choice.as_str()),
        Some(_) => return Err("active_choice must be `number` or `entity`".into()),
        None => None,
    };
    if let Some(unit) = object.get("unit_of_measurement")
        && !unit.is_null()
        && !unit.is_string()
    {
        return Err("Sun elevation threshold unit_of_measurement must be a string or null".into());
    }
    let number = object
        .get("number")
        .map(parse_sun_elevation_number)
        .transpose()?;
    let entity = object
        .get("entity")
        .map(parse_sun_elevation_entity)
        .transpose()?;
    match active_choice {
        Some("number") => {
            number.ok_or_else(|| "numeric Sun elevation threshold requires number".to_owned())
        }
        Some("entity") => {
            entity.ok_or_else(|| "entity Sun elevation threshold requires entity".to_owned())
        }
        Some(_) => Err("active_choice must be `number` or `entity`".into()),
        None => match (number, entity) {
            (Some(_), Some(_)) => Err(
                "Sun elevation threshold bound with number and entity requires active_choice"
                    .into(),
            ),
            (Some(number), None) => Ok(number),
            (None, Some(entity)) => Ok(entity),
            (None, None) => Err("Sun elevation threshold bound requires number or entity".into()),
        },
    }
}

fn parse_sun_elevation_number(
    value: &Value,
) -> std::result::Result<SunElevationThresholdBound, String> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(number) => number.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|number| number.is_finite())
    .ok_or_else(|| "Sun elevation threshold number must be finite".to_owned())?;
    Ok(SunElevationThresholdBound::Number(number))
}

fn parse_sun_elevation_entity(
    value: &Value,
) -> std::result::Result<SunElevationThresholdBound, String> {
    let entity = value
        .as_str()
        .filter(|entity| {
            !entity.is_empty() && entity.contains('.') && !entity.chars().any(char::is_whitespace)
        })
        .ok_or_else(|| "Sun elevation threshold entity must be an entity ID".to_owned())?;
    Ok(SunElevationThresholdBound::Entity(entity.to_owned()))
}

fn sun_elevation_threshold_value(
    runtime: &Runtime,
    bound: &SunElevationThresholdBound,
) -> Option<f64> {
    match bound {
        SunElevationThresholdBound::Number(number) => Some(*number),
        SunElevationThresholdBound::Entity(entity_id) => runtime
            .get_state(entity_id)
            .filter(|state| {
                state
                    .attributes
                    .get("unit_of_measurement")
                    .and_then(Value::as_str)
                    == Some("°")
            })
            .and_then(|state| state.state.parse::<f64>().ok())
            .filter(|value| value.is_finite()),
    }
}

fn sun_elevation_in_threshold(
    runtime: &Runtime,
    threshold: &SunElevationThreshold,
    elevation: f64,
) -> bool {
    if !elevation.is_finite() {
        return false;
    }
    match threshold {
        SunElevationThreshold::Any => true,
        SunElevationThreshold::Above(bound) => {
            sun_elevation_threshold_value(runtime, bound).is_some_and(|bound| elevation > bound)
        }
        SunElevationThreshold::Below(bound) => {
            sun_elevation_threshold_value(runtime, bound).is_some_and(|bound| elevation < bound)
        }
        SunElevationThreshold::Between { lower, upper } => {
            sun_elevation_threshold_value(runtime, lower)
                .zip(sun_elevation_threshold_value(runtime, upper))
                .is_some_and(|(lower, upper)| lower <= elevation && elevation <= upper)
        }
        SunElevationThreshold::Outside { lower, upper } => {
            sun_elevation_threshold_value(runtime, lower)
                .zip(sun_elevation_threshold_value(runtime, upper))
                .is_some_and(|(lower, upper)| elevation < lower || elevation > upper)
        }
    }
}

fn sun_elevation_from_state(state: &State) -> Option<f64> {
    state
        .attributes
        .get("elevation")
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(number) => number.parse::<f64>().ok(),
            _ => None,
        })
        .filter(|elevation| elevation.is_finite())
}

fn parse_sun_offset(value: &Value) -> std::result::Result<ChronoDuration, String> {
    let seconds = match value {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| "offset number must be finite".to_owned())?,
        Value::String(text) => parse_sun_offset_text(text)?,
        Value::Object(parts) => {
            let mut total = 0.0;
            for (key, multiplier) in [
                ("days", 86_400.0),
                ("hours", 3_600.0),
                ("minutes", 60.0),
                ("seconds", 1.0),
                ("milliseconds", 0.001),
            ] {
                if let Some(value) = parts.get(key) {
                    let amount = value
                        .as_f64()
                        .ok_or_else(|| format!("offset component `{key}` must be numeric"))?;
                    if !amount.is_finite() {
                        return Err(format!("offset component `{key}` must be finite"));
                    }
                    total += amount * multiplier;
                }
            }
            if parts.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "days" | "hours" | "minutes" | "seconds" | "milliseconds"
                )
            }) {
                return Err("offset has an unknown duration component".into());
            }
            total
        }
        _ => return Err("offset must be seconds, HH:MM:SS, or duration components".into()),
    };
    if !seconds.is_finite() || seconds.abs() > i64::MAX as f64 / 1_000_000.0 {
        return Err("offset must be a finite duration within the supported range".into());
    }
    Ok(ChronoDuration::microseconds(
        (seconds * 1_000_000.0).round() as i64,
    ))
}

fn parse_sun_offset_text(text: &str) -> std::result::Result<f64, String> {
    let text = text.trim();
    let (sign, unsigned) = match text.strip_prefix('-') {
        Some(unsigned) => (-1.0, unsigned),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    let parts = unsigned.split(':').collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3) {
        return Err("offset text must use HH:MM or HH:MM:SS".into());
    }
    let hours = parts[0]
        .parse::<u64>()
        .map_err(|_| "offset hours are invalid".to_owned())?;
    let minutes = parts[1]
        .parse::<u64>()
        .map_err(|_| "offset minutes are invalid".to_owned())?;
    let seconds = if parts.len() == 3 {
        parts[2]
            .parse::<f64>()
            .map_err(|_| "offset seconds are invalid".to_owned())?
    } else {
        0.0
    };
    if minutes >= 60 || !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        return Err("offset text contains an invalid clock component".into());
    }
    Ok(sign * (hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds))
}

fn sun_offset(trigger: &Trigger) -> std::result::Result<ChronoDuration, String> {
    let Trigger::Sun {
        platform,
        offset,
        offset_type,
        ..
    } = trigger
    else {
        return Ok(ChronoDuration::zero());
    };
    let offset = offset
        .as_ref()
        .map(parse_sun_offset)
        .transpose()?
        .unwrap_or_default();
    Ok(
        if platform != "sun" && offset_type.as_deref() == Some("before") {
            -offset
        } else {
            offset
        },
    )
}

fn sun_trigger_events(trigger: &Trigger) -> std::result::Result<Vec<SolarEvent>, String> {
    let Trigger::Sun {
        platform,
        event,
        twilight_type,
        period,
        ..
    } = trigger
    else {
        return Ok(Vec::new());
    };
    let event = if platform == "sun" {
        event.as_deref().unwrap_or_default()
    } else {
        platform.strip_prefix("sun.").unwrap_or_default()
    };
    let twilight = match twilight_type.as_deref().unwrap_or("civil") {
        "civil" => 6.0,
        "nautical" => 12.0,
        "astronomical" => 18.0,
        other => return Err(format!("unsupported twilight type `{other}`")),
    };
    let period = match period.as_deref().unwrap_or("any") {
        "any" => (true, true),
        "morning" => (true, false),
        "evening" => (false, true),
        other => return Err(format!("unsupported period `{other}`")),
    };
    let events = match event {
        "sunrise" => vec![SolarEvent::Sunrise],
        "sunset" => vec![SolarEvent::Sunset],
        "solar_noon" => vec![SolarEvent::Noon],
        "solar_midnight" => vec![SolarEvent::Midnight],
        "dawn" => vec![SolarEvent::Dawn {
            depression: twilight,
        }],
        "dusk" => vec![SolarEvent::Dusk {
            depression: twilight,
        }],
        "golden_hour_started" => boundary_events(-4.0, 6.0, period),
        "golden_hour_ended" => boundary_events(6.0, -4.0, period),
        "blue_hour_started" => boundary_events(-6.0, -4.0, period),
        "blue_hour_ended" => boundary_events(-4.0, -6.0, period),
        _ => return Err(format!("unsupported Sun event `{event}`")),
    };
    Ok(events)
}

fn boundary_events(
    rising_elevation: f64,
    setting_elevation: f64,
    period: (bool, bool),
) -> Vec<SolarEvent> {
    let mut events = Vec::with_capacity(2);
    if period.0 {
        events.push(SolarEvent::Elevation {
            elevation: rising_elevation,
            rising: true,
        });
    }
    if period.1 {
        events.push(SolarEvent::Elevation {
            elevation: setting_elevation,
            rising: false,
        });
    }
    events
}

fn sun_environment(runtime: &Runtime) -> std::result::Result<(Observer, Tz), String> {
    let home = runtime.home_context();
    let timezone = home
        .time_zone
        .parse::<Tz>()
        .map_err(|_| format!("invalid configured timezone `{}`", home.time_zone))?;
    Ok((
        Observer {
            latitude: home.latitude,
            longitude: home.longitude,
            elevation_m: home.elevation,
        },
        timezone,
    ))
}

fn next_sun_trigger_time(
    runtime: &Runtime,
    trigger: &Trigger,
    now: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, String> {
    let (observer, timezone) = sun_environment(runtime)?;
    let offset = sun_offset(trigger)?;
    if let Trigger::Sun { platform, .. } = trigger {
        let polar = match platform.as_str() {
            "sun.midnight_sun_started" => Some((SolarEvent::Midnight, true)),
            "sun.midnight_sun_ended" => Some((SolarEvent::Midnight, false)),
            "sun.polar_night_started" => Some((SolarEvent::Noon, false)),
            "sun.polar_night_ended" => Some((SolarEvent::Noon, true)),
            _ => None,
        };
        if let Some((event, target_above)) = polar {
            return next_polar_transition(observer, timezone, event, now, target_above, offset);
        }
    }
    let mut next = None;
    for event in sun_trigger_events(trigger)? {
        match next_event(observer, now, timezone, event, offset) {
            Ok(candidate) if next.is_none_or(|current| candidate < current) => {
                next = Some(candidate);
            }
            Ok(_) => {}
            Err(SolarError::SearchExhausted { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(next)
}

fn next_polar_transition(
    observer: Observer,
    timezone: Tz,
    event: SolarEvent,
    now: DateTime<Utc>,
    target_above: bool,
    offset: ChronoDuration,
) -> std::result::Result<Option<DateTime<Utc>>, String> {
    if observer.latitude.abs() < 65.0 {
        return Ok(None);
    }
    let anchor = now
        .checked_sub_signed(offset)
        .ok_or_else(|| "Sun polar transition anchor overflowed".to_owned())?;
    let mut date = anchor
        .with_timezone(&timezone)
        .date_naive()
        .checked_sub_signed(ChronoDuration::days(2))
        .ok_or_else(|| "Sun polar transition date overflowed".to_owned())?;
    let mut previous_above = None;
    for _ in 0..400 {
        let event_time = event_on_date(observer, date, event)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "solar noon or midnight was missing".to_owned())?;
        let above = position_at(observer, event_time)
            .map_err(|error| error.to_string())?
            .elevation
            > -0.833;
        if previous_above.is_some_and(|previous| above == target_above && above != previous) {
            let fire_time = event_time
                .checked_add_signed(offset)
                .ok_or_else(|| "Sun polar transition offset overflowed".to_owned())?;
            if fire_time > now {
                return Ok(Some(fire_time));
            }
        }
        previous_above = Some(above);
        date = date
            .succ_opt()
            .ok_or_else(|| "Sun polar transition date overflowed".to_owned())?;
    }
    Ok(None)
}

fn sun_trigger_run(
    runtime: &Runtime,
    automation: &Automation,
    trigger: &Trigger,
    trigger_index: usize,
    fired_at: DateTime<Utc>,
) -> std::result::Result<PendingRun, String> {
    let Trigger::Sun {
        platform,
        event,
        twilight_type,
        period,
        id,
        alias,
        ..
    } = trigger
    else {
        unreachable!("sun trigger runner requires a Sun trigger")
    };
    let event = if platform == "sun" {
        event.as_deref().unwrap_or_default()
    } else {
        platform.strip_prefix("sun.").unwrap_or_default()
    };
    let context_name = if event.starts_with("golden_hour_") || event.starts_with("blue_hour_") {
        event
    } else {
        match event {
            "solar_noon" => "noon",
            "solar_midnight" => "midnight",
            other => other,
        }
    };
    let mut fields = Map::new();
    fields.insert(
        "description".into(),
        Value::String(format!("sun event {context_name}")),
    );
    fields.insert(
        "id".into(),
        Value::String(id.clone().unwrap_or_else(|| trigger_index.to_string())),
    );
    fields.insert("idx".into(), Value::String(trigger_index.to_string()));
    fields.insert(
        "alias".into(),
        alias.clone().map_or(Value::Null, Value::String),
    );
    if platform == "sun" {
        fields.insert("event".into(), Value::String(event.into()));
        fields.insert(
            "offset".into(),
            Value::String(format_sun_duration(sun_offset(trigger)?)),
        );
    }
    if matches!(event, "dawn" | "dusk") {
        fields.insert(
            "type".into(),
            Value::String(twilight_type.clone().unwrap_or_else(|| "civil".into())),
        );
    }
    if event.starts_with("golden_hour_") || event.starts_with("blue_hour_") {
        fields.insert(
            "period".into(),
            Value::String(period.clone().unwrap_or_else(|| "any".into())),
        );
    }
    let event = synthetic_event(
        "sun_event",
        serde_json::json!({"time": fired_at}),
        Context::default(),
    );
    let variables = variables_for_trigger_with_fields(
        runtime,
        automation,
        Some(&event),
        platform,
        None,
        Some(fields),
    );
    Ok(PendingRun {
        automation: automation.clone(),
        trigger_context: event.context,
        lineage: Vec::new(),
        variables,
    })
}

fn format_sun_duration(duration: ChronoDuration) -> String {
    let total_micros = duration
        .num_microseconds()
        .unwrap_or_else(|| duration.num_milliseconds().saturating_mul(1_000));
    let negative = total_micros < 0;
    let absolute_micros = total_micros.unsigned_abs();
    let day_micros = 86_400_000_000;
    let days = if negative {
        absolute_micros.div_ceil(day_micros)
    } else {
        absolute_micros / day_micros
    };
    let time_of_day_micros = if negative {
        let remainder = absolute_micros % day_micros;
        if remainder == 0 {
            0
        } else {
            day_micros - remainder
        }
    } else {
        absolute_micros % day_micros
    };
    let hours = time_of_day_micros / 3_600_000_000;
    let minutes = (time_of_day_micros / 60_000_000) % 60;
    let seconds = (time_of_day_micros / 1_000_000) % 60;
    let fraction = time_of_day_micros % 1_000_000;
    let time = if fraction == 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}.{fraction:06}")
    };
    if days == 0 {
        time
    } else {
        let day_suffix = if days == 1 { "day" } else { "days" };
        format!(
            "{}{days} {day_suffix}, {time}",
            if negative { "-" } else { "" }
        )
    }
}

fn due_sun_trigger_runs(
    runtime: &Runtime,
    automations: &[Automation],
    now: DateTime<Utc>,
    schedules: &mut HashMap<ScheduleIdentity, Option<DateTime<Utc>>>,
) -> std::result::Result<Vec<PendingRun>, String> {
    let mut active_schedules = HashSet::new();
    let mut runs = Vec::new();
    for automation in automations {
        for (trigger_index, trigger) in automation.triggers.iter().enumerate() {
            if !matches!(trigger, Trigger::Sun { .. }) || is_sun_elevation_trigger(trigger) {
                continue;
            }
            let identity = ScheduleIdentity {
                automation_id: automation.id.clone(),
                trigger_index,
                entity_id: None,
            };
            active_schedules.insert(identity.clone());
            if !schedules.contains_key(&identity) {
                schedules.insert(
                    identity.clone(),
                    next_sun_trigger_time(runtime, trigger, now)?,
                );
            }
            if let Some(Some(scheduled_at)) = schedules.get(&identity).copied()
                && scheduled_at <= now
            {
                runs.push(sun_trigger_run(
                    runtime,
                    automation,
                    trigger,
                    trigger_index,
                    scheduled_at,
                )?);
                schedules.insert(identity, next_sun_trigger_time(runtime, trigger, now)?);
            }
        }
    }
    schedules.retain(|identity, _| active_schedules.contains(identity));
    Ok(runs)
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
        Trigger::Sun {
            platform,
            threshold,
            ..
        } if platform == "sun.elevation_crossed_threshold" => {
            let Some(threshold) = threshold else {
                return Ok(false);
            };
            let parsed_threshold =
                parse_sun_elevation_threshold(threshold, false).map_err(Error::InvalidInput)?;
            let Some(current) = runtime.get_state("sun.sun") else {
                return Ok(false);
            };
            if matches!(current.state.as_str(), "unknown" | "unavailable") {
                return Ok(false);
            }
            Ok(sun_elevation_from_state(&current)
                .is_some_and(|value| sun_elevation_in_threshold(runtime, &parsed_threshold, value)))
        }
        _ => Ok(false),
    }
}

fn conditions_match(
    runtime: &Runtime,
    conditions: &[Condition],
    variables: &Map<String, Value>,
) -> Result<bool> {
    conditions_match_at(runtime, conditions, variables, Utc::now())
}

fn conditions_match_at(
    runtime: &Runtime,
    conditions: &[Condition],
    variables: &Map<String, Value>,
    now: DateTime<Utc>,
) -> Result<bool> {
    for condition in conditions {
        if !condition_matches_at(runtime, condition, variables, now)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn condition_matches_at(
    runtime: &Runtime,
    condition: &Condition,
    variables: &Map<String, Value>,
    now: DateTime<Utc>,
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
        sun_condition @ (Condition::Sun { .. }
        | Condition::SunIsUp { .. }
        | Condition::SunIsSet { .. }
        | Condition::SunIsAscending { .. }
        | Condition::SunIsDescending { .. }
        | Condition::SunIsNight { .. }
        | Condition::SunMorningTwilight { .. }
        | Condition::SunEveningTwilight { .. }
        | Condition::SunGoldenHour { .. }
        | Condition::SunBlueHour { .. }
        | Condition::SunIsMidnightSun { .. }
        | Condition::SunIsPolarNight { .. }
        | Condition::SunElevation { .. }) => {
            evaluate_sun_condition_config(runtime, sun_condition, now)
        }
        Condition::And { conditions } => conditions_match_at(runtime, conditions, variables, now),
        Condition::Or { conditions } => {
            for condition in conditions {
                if condition_matches_at(runtime, condition, variables, now)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Condition::Not { conditions } => Ok(!condition_matches_at(
            runtime,
            &conditions[0],
            variables,
            now,
        )?),
    }
}

fn evaluate_sun_condition_config(
    runtime: &Runtime,
    condition: &Condition,
    now: DateTime<Utc>,
) -> Result<bool> {
    let (observer, timezone) = sun_environment(runtime).map_err(Error::InvalidInput)?;
    let sun_condition = match condition {
        Condition::Sun {
            before,
            after,
            before_offset,
            after_offset,
            options,
        } => {
            let options = sun_options(options.as_ref())?;
            ensure_sun_option_keys(
                &options,
                &["before", "after", "before_offset", "after_offset"],
            )?;
            let before = legacy_bound(before.as_deref(), options.get("before"))?;
            let after = legacy_bound(after.as_deref(), options.get("after"))?;
            if before.is_none() && after.is_none() {
                return Err(Error::InvalidInput(
                    "Sun condition requires before or after".into(),
                ));
            }
            let before_offset =
                legacy_offset(before_offset.as_ref(), options.get("before_offset"))?;
            let after_offset = legacy_offset(after_offset.as_ref(), options.get("after_offset"))?;
            PureSunCondition::Legacy(LegacySunCondition {
                before,
                after,
                before_offset,
                after_offset,
            })
        }
        Condition::SunIsUp { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsUp
        }
        Condition::SunIsSet { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsSet
        }
        Condition::SunIsAscending { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsAscending
        }
        Condition::SunIsDescending { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsDescending
        }
        Condition::SunIsNight { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsNight
        }
        Condition::SunMorningTwilight {
            twilight_type,
            options,
        } => PureSunCondition::MorningTwilight(parse_twilight_band(
            twilight_type.as_deref(),
            &sun_options(options.as_ref())?,
        )?),
        Condition::SunEveningTwilight {
            twilight_type,
            options,
        } => PureSunCondition::EveningTwilight(parse_twilight_band(
            twilight_type.as_deref(),
            &sun_options(options.as_ref())?,
        )?),
        Condition::SunGoldenHour { period, options } => PureSunCondition::GoldenHour(
            parse_sun_period(period.as_deref(), &sun_options(options.as_ref())?)?,
        ),
        Condition::SunBlueHour { period, options } => PureSunCondition::BlueHour(parse_sun_period(
            period.as_deref(),
            &sun_options(options.as_ref())?,
        )?),
        Condition::SunIsMidnightSun { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsMidnightSun
        }
        Condition::SunIsPolarNight { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])?;
            PureSunCondition::IsPolarNight
        }
        Condition::SunElevation { threshold, options } => {
            let options = sun_options(options.as_ref())?;
            ensure_sun_option_keys(&options, &["threshold"])?;
            if threshold.is_some() && options.contains_key("threshold") {
                return Err(Error::InvalidInput(
                    "Sun elevation threshold is specified more than once".into(),
                ));
            }
            let threshold = threshold
                .as_ref()
                .or_else(|| options.get("threshold"))
                .ok_or_else(|| Error::InvalidInput("Sun elevation needs a threshold".into()))?;
            let elevation = hass_sun::position_at(observer, now)
                .map_err(|error| Error::InvalidInput(error.to_string()))?
                .elevation;
            return resolve_sun_threshold(runtime, threshold, elevation);
        }
        _ => return Err(Error::InvalidInput("expected a Sun condition".into())),
    };
    evaluate_sun_condition(observer, now, timezone, sun_condition)
        .map_err(|error| Error::InvalidInput(error.to_string()))
}

fn sun_options(options: Option<&Value>) -> Result<Map<String, Value>> {
    match options {
        None => Ok(Map::new()),
        Some(Value::Object(options)) => Ok(options.clone()),
        Some(_) => Err(Error::InvalidInput(
            "Sun condition options must be a mapping".into(),
        )),
    }
}

fn ensure_sun_option_keys(options: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(unknown) = options.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::InvalidInput(format!(
            "unsupported Sun condition option `{unknown}`"
        )));
    }
    Ok(())
}

fn validate_sun_condition(condition: &Condition) -> Result<()> {
    match condition {
        Condition::Sun {
            before,
            after,
            before_offset,
            after_offset,
            options,
        } => {
            let options = sun_options(options.as_ref())?;
            ensure_sun_option_keys(
                &options,
                &["before", "after", "before_offset", "after_offset"],
            )?;
            let before = legacy_bound(before.as_deref(), options.get("before"))?;
            let after = legacy_bound(after.as_deref(), options.get("after"))?;
            if before.is_none() && after.is_none() {
                return Err(Error::InvalidInput(
                    "condition requires before or after".into(),
                ));
            }
            legacy_offset(before_offset.as_ref(), options.get("before_offset"))?;
            legacy_offset(after_offset.as_ref(), options.get("after_offset"))?;
            Ok(())
        }
        Condition::SunIsUp { options }
        | Condition::SunIsSet { options }
        | Condition::SunIsAscending { options }
        | Condition::SunIsDescending { options }
        | Condition::SunIsNight { options }
        | Condition::SunIsMidnightSun { options }
        | Condition::SunIsPolarNight { options } => {
            ensure_sun_option_keys(&sun_options(options.as_ref())?, &[])
        }
        Condition::SunMorningTwilight {
            twilight_type,
            options,
        }
        | Condition::SunEveningTwilight {
            twilight_type,
            options,
        } => parse_twilight_band(twilight_type.as_deref(), &sun_options(options.as_ref())?)
            .map(|_| ()),
        Condition::SunGoldenHour { period, options }
        | Condition::SunBlueHour { period, options } => {
            parse_sun_period(period.as_deref(), &sun_options(options.as_ref())?).map(|_| ())
        }
        Condition::SunElevation { threshold, options } => {
            let options = sun_options(options.as_ref())?;
            ensure_sun_option_keys(&options, &["threshold"])?;
            if threshold.is_some() && options.contains_key("threshold") {
                return Err(Error::InvalidInput(
                    "threshold is specified more than once".into(),
                ));
            }
            validate_sun_threshold(
                threshold
                    .as_ref()
                    .or_else(|| options.get("threshold"))
                    .ok_or_else(|| Error::InvalidInput("Sun elevation needs a threshold".into()))?,
            )
        }
        _ => Err(Error::InvalidInput("expected a Sun condition".into())),
    }
}

fn validate_sun_threshold(value: &Value) -> Result<()> {
    parse_sun_elevation_threshold(value, false)
        .map(|_| ())
        .map_err(Error::InvalidInput)
}

fn legacy_bound(direct: Option<&str>, nested: Option<&Value>) -> Result<Option<LegacySunEvent>> {
    if direct.is_some() && nested.is_some() {
        return Err(Error::InvalidInput(
            "Sun condition bound is specified more than once".into(),
        ));
    }
    let value = direct
        .map(str::to_owned)
        .or_else(|| nested.and_then(Value::as_str).map(str::to_owned));
    if nested.is_some() && value.is_none() {
        return Err(Error::InvalidInput(
            "Sun condition event must be a string".into(),
        ));
    }
    match value.as_deref() {
        None => Ok(None),
        Some("sunrise") => Ok(Some(LegacySunEvent::Sunrise)),
        Some("sunset") => Ok(Some(LegacySunEvent::Sunset)),
        Some(other) => Err(Error::InvalidInput(format!(
            "unsupported Sun condition event `{other}`"
        ))),
    }
}

fn legacy_offset(direct: Option<&Value>, nested: Option<&Value>) -> Result<ChronoDuration> {
    if direct.is_some() && nested.is_some() {
        return Err(Error::InvalidInput(
            "Sun condition offset is specified more than once".into(),
        ));
    }
    direct
        .or(nested)
        .map(parse_sun_offset)
        .transpose()
        .map_err(Error::InvalidInput)
        .map(|value| value.unwrap_or_else(ChronoDuration::zero))
}

fn parse_twilight_band(direct: Option<&str>, options: &Map<String, Value>) -> Result<TwilightBand> {
    ensure_sun_option_keys(options, &["type"])?;
    if direct.is_some() && options.contains_key("type") {
        return Err(Error::InvalidInput(
            "Sun twilight type is specified more than once".into(),
        ));
    }
    let value = direct.map(str::to_owned).or_else(|| {
        options
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if options.contains_key("type") && value.is_none() {
        return Err(Error::InvalidInput(
            "Sun twilight type must be a string".into(),
        ));
    }
    match value.as_deref().unwrap_or("any") {
        "any" => Ok(TwilightBand::Any),
        "civil" => Ok(TwilightBand::Civil),
        "nautical" => Ok(TwilightBand::Nautical),
        "astronomical" => Ok(TwilightBand::Astronomical),
        other => Err(Error::InvalidInput(format!(
            "unsupported Sun twilight type `{other}`"
        ))),
    }
}

fn parse_sun_period(direct: Option<&str>, options: &Map<String, Value>) -> Result<SunPeriod> {
    ensure_sun_option_keys(options, &["period"])?;
    if direct.is_some() && options.contains_key("period") {
        return Err(Error::InvalidInput(
            "Sun period is specified more than once".into(),
        ));
    }
    let value = direct.map(str::to_owned).or_else(|| {
        options
            .get("period")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if options.contains_key("period") && value.is_none() {
        return Err(Error::InvalidInput("Sun period must be a string".into()));
    }
    match value.as_deref().unwrap_or("any") {
        "any" => Ok(SunPeriod::Any),
        "morning" => Ok(SunPeriod::Morning),
        "evening" => Ok(SunPeriod::Evening),
        other => Err(Error::InvalidInput(format!(
            "unsupported Sun period `{other}`"
        ))),
    }
}

fn resolve_sun_threshold(runtime: &Runtime, value: &Value, elevation: f64) -> Result<bool> {
    let threshold = parse_sun_elevation_threshold(value, false).map_err(Error::InvalidInput)?;
    Ok(sun_elevation_in_threshold(runtime, &threshold, elevation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{HomeContext, MemoryStore, ServiceResult};
    use hass_entity::EntityLifecycle;
    use hass_notifications::NotificationIntegration;
    use hass_registry::Registries;
    use hass_tag::TagIntegration;
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

    #[tokio::test]
    async fn native_tag_scan_uses_public_event_context_and_trigger_metadata() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let tag_integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let tags = tag_integration.manager();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: tag_metadata
  mode: queued
  trigger_variables:
    source_name: reader
    skip_trigger: false
  triggers:
    - platform: tag
      tag_id: [scan-1, scan-2]
      device_id: [scanner-1, scanner-2]
      id: custom-trigger-id
      alias: Scanner tag
    - trigger: tag
      tag_id: scan-1
      enabled: false
      id: disabled-trigger
    - trigger: tag
      tag_id: scan-1
      enabled: "{{ skip_trigger }}"
      id: templated-disabled-trigger
  actions:
    - action: test.notify
      data:
        name: custom
        platform: "{{ trigger.platform }}"
        description: "{{ trigger.description }}"
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
        trigger_alias: "{{ trigger.alias }}"
        source_name: "{{ source_name }}"
        event_type: "{{ trigger.event.event_type }}"
        event_tag_id: "{{ trigger.event.data.tag_id }}"
- id: tag_default_id
  triggers:
    - platform: tag
      tag_id: scan-1
      device_id: scanner-1
  actions:
    - action: test.notify
      data:
        name: default
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
- id: tag_optional_device
  mode: queued
  triggers:
    - trigger: tag
      tag_id: scan-2
  actions:
    - action: test.notify
      data:
        name: optional_device
        event_device_id: "{{ trigger.event.data.device_id }}"
"#,
        );
        let engine = start_with_features(
            runtime.clone(),
            automations,
            AutomationFeatures {
                registries: Some(registries),
                ..AutomationFeatures::default()
            },
        )
        .unwrap();
        let context = Context {
            id: "native-tag-context".into(),
            parent_id: Some("parent-context".into()),
            user_id: Some("reader-user".into()),
        };
        let assert_causal_context = |actual: &Context| {
            assert_ne!(actual.id, context.id);
            assert_eq!(actual.parent_id.as_deref(), Some(context.id.as_str()));
            assert_eq!(actual.user_id.as_deref(), context.user_id.as_deref());
        };

        tags.scan("unconfigured-tag", Some("scanner-1"), context.clone())
            .unwrap();
        tags.scan("scan-1", Some("other-scanner"), context.clone())
            .unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );

        // A trigger without a device filter accepts scans from any device.
        tags.scan("scan-2", Some("other-scanner"), context.clone())
            .unwrap();
        let any_device = receive_notification_run(&mut receiver).await;
        assert_eq!(any_device.data["name"], "optional_device");
        assert_eq!(any_device.data["event_device_id"], "other-scanner");
        assert_causal_context(&any_device.context);

        // Scalar/list filters match each configured value.
        tags.scan("scan-2", Some("scanner-2"), context.clone())
            .unwrap();
        let mut list_matches = [
            receive_notification_run(&mut receiver).await,
            receive_notification_run(&mut receiver).await,
        ];
        list_matches.sort_by_key(|call| call.data["name"].as_str().unwrap().to_owned());
        assert_eq!(list_matches[0].data["name"], "custom");
        assert_eq!(list_matches[1].data["name"], "optional_device");
        assert!(
            list_matches
                .iter()
                .all(
                    |call| call.context.parent_id.as_deref() == Some(context.id.as_str())
                        && call.context.user_id.as_deref() == context.user_id.as_deref()
                )
        );

        tags.scan("scan-1", Some("scanner-1"), context.clone())
            .unwrap();
        let mut calls = [
            receive_notification_run(&mut receiver).await,
            receive_notification_run(&mut receiver).await,
        ];
        calls.sort_by_key(|call| call.data["name"].as_str().unwrap().to_owned());
        assert_eq!(calls[0].data["name"], "custom");
        assert_eq!(calls[0].data["platform"], "tag");
        assert_eq!(calls[0].data["description"], "Tag scanned");
        assert_eq!(calls[0].data["trigger_id"], "custom-trigger-id");
        assert_eq!(calls[0].data["trigger_idx"], "0");
        assert_eq!(calls[0].data["trigger_alias"], "Scanner tag");
        assert_eq!(calls[0].data["source_name"], "reader");
        assert_eq!(calls[0].data["event_type"], "tag_scanned");
        assert_eq!(calls[0].data["event_tag_id"], "scan-1");
        assert_causal_context(&calls[0].context);
        assert_eq!(calls[1].data["name"], "default");
        assert_eq!(calls[1].data["trigger_id"], "0");
        assert_eq!(calls[1].data["trigger_idx"], "0");
        assert_causal_context(&calls[1].context);

        engine.shutdown().await.unwrap();
        tag_integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_tag_scan_matches_coerced_scalars_and_keeps_null_device_filter_empty() {
        let runtime = runtime();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let integration =
            TagIntegration::start(runtime.clone(), registries, lifecycle.clone()).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: scalar_ids
  mode: queued
  triggers:
    - trigger: tag
      tag_id: [42, true, 1.0e20]
      device_id: false
  actions:
    - action: test.notify
      data:
        name: "id:{{ trigger.event.data.tag_id }}"
- id: explicit_null_device
  triggers:
    - trigger: tag
      tag_id: 42
      device_id: null
  actions:
    - action: test.notify
      data:
        name: must_not_run
- id: empty_tags
  triggers:
    - trigger: tag
      tag_id: null
  actions:
    - action: test.notify
      data:
        name: must_not_run
"#,
        );
        let engine = start(runtime.clone(), automations).unwrap();
        for id in ["42", "True", "1e+20"] {
            integration
                .manager()
                .scan(id, Some("False"), Context::default())
                .unwrap();
            let call = tokio::time::timeout(StdDuration::from_secs(2), receiver.recv())
                .await
                .unwrap_or_else(|_| panic!("No action for tag {id}"))
                .unwrap();
            assert_eq!(call.data["name"], format!("id:{id}"));
        }
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );
        for invalid in ["{nested: value}", "[null]", "[[nested]]"] {
            let source = format!(
                "- id: invalid\n  triggers:\n    - trigger: tag\n      tag_id: {invalid}\n  actions: []\n"
            );
            assert!(serde_yaml::from_str::<Vec<Automation>>(&source).is_err());
        }
        engine.shutdown().await.unwrap();
        integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_tag_trigger_expands_split_legacy_device_ids() {
        let runtime = runtime();
        runtime
            .set_data(
                "core.registries",
                &serde_json::json!({
                    "version": 1,
                    "devices": [{
                        "id": "composite00000000000000000000ab",
                        "config_entry_id": "legacy-entry-one",
                        "config_entries": ["legacy-entry-one", "legacy-entry-two"],
                        "config_entries_subentries": {
                            "legacy-entry-one": [null],
                            "legacy-entry-two": [null]
                        },
                        "created_at": "2025-01-01T00:00:00Z",
                        "modified_at": "2025-01-01T00:00:00Z"
                    }]
                }),
            )
            .unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let split_devices =
            registries.devices_for_composite_device_id("composite00000000000000000000ab");
        assert_eq!(split_devices.len(), 2);

        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let tag_integration =
            TagIntegration::start(runtime.clone(), registries.clone(), lifecycle.clone()).unwrap();
        let tags = tag_integration.manager();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: split_device_tag
  mode: queued
  triggers:
    - trigger: tag
      tag_id: scan-1
      device_id: composite00000000000000000000ab
  actions:
    - action: test.notify
      data:
        scanned_device: "{{ trigger.event.data.device_id }}"
"#,
        );
        let engine = start_with_features(
            runtime.clone(),
            automations,
            AutomationFeatures {
                registries: Some(registries),
                ..AutomationFeatures::default()
            },
        )
        .unwrap();

        tags.scan(
            "scan-1",
            Some("composite00000000000000000000ab"),
            Context::default(),
        )
        .unwrap();
        tags.scan("scan-1", Some("unrelated-device"), Context::default())
            .unwrap();
        assert!(
            tokio::time::timeout(StdDuration::from_millis(60), receiver.recv())
                .await
                .is_err()
        );

        let mut matched_devices = Vec::new();
        for device in &split_devices {
            tags.scan("scan-1", Some(device.id.as_str()), Context::default())
                .unwrap();
            let call = receive_notification_run(&mut receiver).await;
            matched_devices.push(call.data["scanned_device"].as_str().unwrap().to_owned());
            tokio::task::yield_now().await;
        }
        let mut expected_devices = split_devices
            .into_iter()
            .map(|device| device.id)
            .collect::<Vec<_>>();
        matched_devices.sort();
        expected_devices.sort();
        assert_eq!(matched_devices, expected_devices);

        engine.shutdown().await.unwrap();
        tag_integration.shutdown().unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[test]
    fn sun_trigger_and_condition_offsets_accept_clocks_and_preserve_microseconds() {
        let trigger: Trigger =
            serde_yaml::from_str("platform: sun\nevent: sunrise\noffset: \"00:01\"\n").unwrap();
        assert_eq!(sun_offset(&trigger).unwrap(), ChronoDuration::minutes(1));

        let condition: Condition = serde_yaml::from_str(
            "condition: sun\noptions:\n  before: sunrise\n  before_offset: \"00:00:01.123456\"\n",
        )
        .unwrap();
        validate_sun_condition(&condition).unwrap();
        let Condition::Sun {
            before_offset,
            options,
            ..
        } = &condition
        else {
            unreachable!("Sun YAML condition should deserialize to Condition::Sun")
        };
        let options = sun_options(options.as_ref()).unwrap();
        assert_eq!(
            legacy_offset(before_offset.as_ref(), options.get("before_offset")).unwrap(),
            ChronoDuration::microseconds(1_123_456)
        );
        assert_eq!(
            parse_sun_offset(&Value::String("-00:00:01.123456".into())).unwrap(),
            ChronoDuration::microseconds(-1_123_456)
        );
    }

    #[test]
    fn sun_offset_string_matches_python_timedelta_format_and_precision() {
        assert_eq!(
            format_sun_duration(ChronoDuration::minutes(-30)),
            "-1 day, 23:30:00"
        );
        assert_eq!(
            format_sun_duration(ChronoDuration::microseconds(1)),
            "0:00:00.000001"
        );
        assert_eq!(
            format_sun_duration(ChronoDuration::hours(25)),
            "1 day, 1:00:00"
        );
        assert_eq!(
            format_sun_duration(ChronoDuration::microseconds(-1)),
            "-1 day, 23:59:59.999999"
        );
    }

    #[test]
    fn sun_trigger_metadata_uses_the_pinned_string_schema() {
        assert!(serde_yaml::from_str::<Trigger>("platform: sun\nevent: sunrise\nid: 1\n").is_err());
        assert!(
            serde_yaml::from_str::<Trigger>("platform: sun\nevent: sunrise\nalias: null\n")
                .is_err()
        );
        assert!(
            serde_yaml::from_str::<Trigger>("platform: sun\nevent: sunrise\nalias: [one, two]\n")
                .is_err()
        );
        assert!(
            serde_yaml::from_str::<Trigger>(
                "platform: sun\nevent: sunrise\nalias: {name: sunrise}\n"
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_and_registry_sun_trigger_yaml_use_pinned_names() {
        let legacy = config(
            r#"
- id: legacy_sun
  triggers:
    - platform: sun
      event: sunset
      offset: "-00:30:00"
  actions:
    - action: test.notify
"#,
        );
        validate(&legacy).unwrap();
        assert!(matches!(
            &legacy[0].triggers[0],
            Trigger::Sun { platform, event: Some(event), .. }
                if platform == "sun" && event == "sunset"
        ));
        assert_eq!(
            sun_offset(&legacy[0].triggers[0]).unwrap(),
            ChronoDuration::minutes(-30)
        );

        let invalid_legacy = config(
            r#"
- id: invalid_legacy_sun
  triggers:
    - platform: sun
      event: sunrise
      offset: "00:30:00"
      offset_type: before
  actions:
    - action: test.notify
"#,
        );
        assert!(validate(&invalid_legacy).is_err());

        let native = config(
            r#"
- id: registry_sun
  triggers:
    - platform: sun.dawn
      options:
        type: nautical
        offset: "00:15:00"
        offset_type: before
  actions:
    - action: test.notify
"#,
        );
        validate(&native).unwrap();
        assert!(matches!(
            &native[0].triggers[0],
            Trigger::Sun { platform, twilight_type: Some(twilight), .. }
                if platform == "sun.dawn" && twilight == "nautical"
        ));
        assert_eq!(
            sun_offset(&native[0].triggers[0]).unwrap(),
            ChronoDuration::minutes(-15)
        );
        let encoded = serde_yaml::to_string(&native[0].triggers[0]).unwrap();
        let decoded: Trigger = serde_yaml::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Trigger::Sun { platform, twilight_type: Some(twilight), .. }
                if platform == "sun.dawn" && twilight == "nautical"
        ));

        let elevation_changed = config(
            r#"
- id: elevation_changed
  triggers:
    - platform: sun.elevation_changed
      options:
        threshold:
          type: any
  actions:
    - action: test.notify
"#,
        );
        validate(&elevation_changed).unwrap();
        assert!(matches!(
            &elevation_changed[0].triggers[0],
            Trigger::Sun {
                platform,
                threshold: Some(Value::Object(threshold)),
                r#for: None,
                ..
            } if platform == "sun.elevation_changed"
                && threshold["type"] == "any"
        ));

        let elevation_crossed = config(
            r#"
- id: elevation_crossed
  triggers:
    - platform: sun.elevation_crossed_threshold
      options:
        threshold:
          type: between
          value_min:
            number: -10
          value_max:
            number: 10
        for:
          seconds: 2
  actions:
    - action: test.notify
"#,
        );
        validate(&elevation_crossed).unwrap();
        assert!(matches!(
            &elevation_crossed[0].triggers[0],
            Trigger::Sun {
                platform,
                threshold: Some(Value::Object(threshold)),
                r#for: Some(DelayValue::Parts(DelayParts { seconds: Some(2.0), .. })),
                ..
            } if platform == "sun.elevation_crossed_threshold"
                && threshold["type"] == "between"
        ));

        let invalid_crossed = config(
            r#"
- id: invalid_elevation_crossed
  triggers:
    - platform: sun.elevation_crossed_threshold
      options:
        threshold:
          type: any
  actions:
    - action: test.notify
"#,
        );
        assert!(validate(&invalid_crossed).is_err());
    }

    #[test]
    fn sun_elevation_thresholds_use_selector_inclusive_and_exclusive_edges() {
        let runtime = runtime();
        let between = parse_sun_elevation_threshold(
            &serde_json::json!({
                "type": "between",
                "value_min": {"number": -10},
                "value_max": {"number": 10}
            }),
            false,
        )
        .unwrap();
        assert!(sun_elevation_in_threshold(&runtime, &between, -10.0));
        assert!(sun_elevation_in_threshold(&runtime, &between, 10.0));
        assert!(!sun_elevation_in_threshold(&runtime, &between, 10.001));

        let outside = parse_sun_elevation_threshold(
            &serde_json::json!({
                "type": "outside",
                "value_min": {"number": -10},
                "value_max": {"number": 10}
            }),
            false,
        )
        .unwrap();
        assert!(!sun_elevation_in_threshold(&runtime, &outside, -10.0));
        assert!(!sun_elevation_in_threshold(&runtime, &outside, 10.0));
        assert!(sun_elevation_in_threshold(&runtime, &outside, 10.001));

        runtime
            .set_state(
                "input_number.sun_limit",
                "5.5",
                Map::from_iter([("unit_of_measurement".into(), Value::String("°".into()))]),
                Context::default(),
                false,
            )
            .unwrap();
        let entity_bound = parse_sun_elevation_threshold(
            &serde_json::json!({
                "type": "above",
                "value": {"entity": "input_number.sun_limit"}
            }),
            false,
        )
        .unwrap();
        assert!(!sun_elevation_in_threshold(&runtime, &entity_bound, 5.5));
        assert!(sun_elevation_in_threshold(&runtime, &entity_bound, 5.6));
    }

    #[test]
    fn sun_elevation_condition_matches_numeric_threshold_selector_contract() {
        use chrono::TimeZone;

        let runtime = runtime();
        runtime
            .set_home_context(HomeContext {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation: 0.0,
                time_zone: "America/Los_Angeles".into(),
            })
            .unwrap();
        runtime
            .set_state(
                "input_number.sun_limit",
                "90",
                Map::from_iter([("unit_of_measurement".into(), Value::String("°".into()))]),
                Context::default(),
                false,
            )
            .unwrap();

        let condition: Condition = serde_yaml::from_str(
            r#"
condition: sun.elevation
options:
  threshold:
    type: between
    value_min:
      number: -90
    value_max:
      entity: input_number.sun_limit
"#,
        )
        .unwrap();
        validate_sun_condition(&condition).unwrap();
        let noon = Utc
            .with_ymd_and_hms(2015, 9, 15, 20, 0, 0)
            .single()
            .unwrap();
        assert!(evaluate_sun_condition_config(&runtime, &condition, noon).unwrap());

        let outside = serde_json::json!({
            "type": "outside",
            "value_min": {"number": -1},
            "value_max": {"number": 1}
        });
        validate_sun_threshold(&outside).unwrap();
        let outside = resolve_sun_threshold(&runtime, &outside, 1.0).unwrap();
        assert!(!outside);
        let outside_threshold = parse_sun_elevation_threshold(
            &serde_json::json!({
                "type": "outside",
                "value_min": {"number": -1},
                "value_max": {"number": 1}
            }),
            false,
        )
        .unwrap();
        assert!(sun_elevation_in_threshold(
            &runtime,
            &outside_threshold,
            1.001
        ));
        assert!(
            validate_sun_threshold(&serde_json::json!({
                "type": "is",
                "value": {"number": 1}
            }))
            .is_err()
        );

        runtime
            .set_state(
                "input_number.sun_limit",
                "90",
                Map::from_iter([("unit_of_measurement".into(), Value::String("rad".into()))]),
                Context::default(),
                false,
            )
            .unwrap();
        assert!(!evaluate_sun_condition_config(&runtime, &condition, noon).unwrap());
    }

    #[tokio::test]
    async fn sun_elevation_changed_uses_sun_state_attribute_transitions() {
        let runtime = runtime();
        let mut initial_attributes = Map::new();
        initial_attributes.insert("elevation".into(), Value::from(5.0));
        initial_attributes.insert("azimuth".into(), Value::from(90.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                initial_attributes,
                Context::default(),
                false,
            )
            .unwrap();

        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: sun_elevation_changed
  triggers:
    - platform: event
      event_type: unused_sun_event
    - platform: sun.elevation_changed
      id: observed_elevation
      alias: Elevation changed
      options:
        threshold:
          type: any
  actions:
    - action: test.notify
      data:
        message: changed
        platform: "{{ trigger.platform }}"
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
        trigger_alias: "{{ trigger.alias }}"
        entity_id: "{{ trigger.entity_id }}"
        from_elevation: "{{ trigger.from_state.attributes.elevation }}"
        to_elevation: "{{ trigger.to_state.attributes.elevation }}"
"#,
        );
        let engine = start(runtime.clone(), automations).unwrap();

        let mut same_elevation = Map::new();
        same_elevation.insert("elevation".into(), Value::from(5.0));
        same_elevation.insert("azimuth".into(), Value::from(100.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                same_elevation,
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(25)).await;
        assert!(receiver.try_recv().is_err());

        let mut changed_elevation = Map::new();
        changed_elevation.insert("elevation".into(), Value::from(6.0));
        changed_elevation.insert("azimuth".into(), Value::from(110.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                changed_elevation,
                Context::default(),
                false,
            )
            .unwrap();
        let call = receive_notification_run(&mut receiver).await;
        assert_eq!(call.data["message"], "changed");
        assert_eq!(call.data["platform"], "sun.elevation_changed");
        assert_eq!(call.data["trigger_id"], "observed_elevation");
        assert_eq!(call.data["trigger_idx"], "1");
        assert_eq!(call.data["trigger_alias"], "Elevation changed");
        assert_eq!(call.data["entity_id"], "sun.sun");
        assert_ne!(call.data["from_elevation"], call.data["to_elevation"]);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sun_elevation_crossing_ignores_creation_and_cancels_for_on_exit() {
        let runtime = runtime();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);
        let automations = config(
            r#"
- id: sun_elevation_crossed
  triggers:
    - platform: sun.elevation_crossed_threshold
      options:
        threshold:
          type: above
          value:
            number: 10
        for: 0.08
  actions:
    - action: test.notify
      data:
        message: crossed
        platform: "{{ trigger.platform }}"
        entity_id: "{{ trigger.entity_id }}"
"#,
        );
        let engine = start(runtime.clone(), automations).unwrap();

        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(15.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert!(
            receiver.try_recv().is_err(),
            "first entity state is not a transition"
        );

        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(5.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(15.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(5.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert!(
            receiver.try_recv().is_err(),
            "leaving the threshold cancels for"
        );

        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(15.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        let mut elevation = Map::new();
        elevation.insert("elevation".into(), Value::from(20.0));
        runtime
            .set_state(
                "sun.sun",
                "above_horizon",
                elevation,
                Context::default(),
                false,
            )
            .unwrap();
        let call = receive_notification_run(&mut receiver).await;
        assert_eq!(call.data["message"], "crossed");
        assert_eq!(call.data["platform"], "sun.elevation_crossed_threshold");
        assert_eq!(call.data["entity_id"], "sun.sun");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sun_scheduler_delivers_legacy_trigger_data_through_shared_action_runner() {
        use chrono::TimeZone;

        let runtime = runtime();
        runtime
            .set_home_context(HomeContext {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation: 0.0,
                time_zone: "America/Los_Angeles".into(),
            })
            .unwrap();
        let automations = config(
            r#"
- id: sun_at_dawn
  mode: queued
  triggers:
    - platform: event
      event_type: never_sun_event
    - platform: sun
      event: sunrise
      offset: "-00:30:00.123456"
      alias: actual UTC sunrise
  actions:
    - action: test.notify
      data:
        platform: "{{ trigger.platform }}"
        event: "{{ trigger.event }}"
        offset: "{{ trigger.offset }}"
        trigger_id: "{{ trigger.id }}"
        trigger_idx: "{{ trigger.idx }}"
        trigger_alias: "{{ trigger.alias }}"
        description: "{{ trigger.description }}"
"#,
        );
        validate(&automations).unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        register_notifier(&runtime, sender);

        let start = Utc.with_ymd_and_hms(2015, 9, 15, 0, 0, 0).single().unwrap();
        let mut schedules = HashMap::new();
        assert!(
            due_sun_trigger_runs(&runtime, &automations, start, &mut schedules)
                .unwrap()
                .is_empty()
        );
        let identity = ScheduleIdentity {
            automation_id: "sun_at_dawn".into(),
            trigger_index: 1,
            entity_id: None,
        };
        let scheduled = schedules[&identity].expect("sunrise should be scheduled");
        let expected_sunrise = next_event(
            Observer {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation_m: 0.0,
            },
            start,
            "America/Los_Angeles".parse().unwrap(),
            SolarEvent::Sunrise,
            ChronoDuration::zero(),
        )
        .unwrap();
        assert_eq!(
            scheduled,
            expected_sunrise - ChronoDuration::minutes(30) - ChronoDuration::microseconds(123_456)
        );

        let runs = due_sun_trigger_runs(
            &runtime,
            &automations,
            scheduled + ChronoDuration::seconds(1),
            &mut schedules,
        )
        .unwrap();
        assert_eq!(runs.len(), 1);
        let run = runs.into_iter().next().unwrap();
        let trigger_context = run.trigger_context.id.clone();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        execute_automation(
            runtime,
            run,
            Arc::new(LineageRegistry::default()),
            cancel_rx,
        )
        .await
        .unwrap();
        let call = receive_notification_run(&mut receiver).await;
        assert_eq!(call.data["platform"], "sun");
        assert_eq!(call.data["event"], "sunrise");
        assert_eq!(call.data["offset"], "-1 day, 23:29:59.876544");
        assert_eq!(call.data["trigger_id"], "1");
        assert_eq!(call.data["trigger_idx"], "1");
        assert_eq!(call.data["trigger_alias"], "actual UTC sunrise");
        assert_eq!(call.data["description"], "sun event sunrise");
        assert_eq!(
            call.context.parent_id.as_deref(),
            Some(trigger_context.as_str())
        );
    }

    #[test]
    fn sun_registry_polar_and_hour_triggers_schedule_native_crossings() {
        use chrono::TimeZone;

        let polar_runtime = runtime();
        polar_runtime
            .set_home_context(HomeContext {
                latitude: 78.22,
                longitude: 15.65,
                elevation: 0.0,
                time_zone: "Europe/Oslo".into(),
            })
            .unwrap();
        let polar: Trigger = serde_yaml::from_str("platform: sun.midnight_sun_started").unwrap();
        validate_sun_trigger(&polar).unwrap();
        let anchor = Utc.with_ymd_and_hms(2015, 3, 1, 0, 0, 0).single().unwrap();
        let transition = next_sun_trigger_time(&polar_runtime, &polar, anchor)
            .unwrap()
            .expect("Svalbard has a midnight-sun transition");
        let expected = Utc
            .with_ymd_and_hms(2015, 4, 18, 22, 56, 36)
            .single()
            .unwrap();
        assert!((transition - expected).num_seconds().abs() <= 2);

        let hour: Trigger = serde_yaml::from_str(
            "platform: sun.golden_hour_started\noptions:\n  period: morning\n",
        )
        .unwrap();
        validate_sun_trigger(&hour).unwrap();
        let san_diego = runtime();
        san_diego
            .set_home_context(HomeContext {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation: 0.0,
                time_zone: "America/Los_Angeles".into(),
            })
            .unwrap();
        let hour_anchor = Utc.with_ymd_and_hms(2015, 9, 15, 1, 0, 0).single().unwrap();
        let scheduled = next_sun_trigger_time(&san_diego, &hour, hour_anchor)
            .unwrap()
            .expect("golden hour reaches the configured morning boundary");
        let expected = next_event(
            Observer {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation_m: 0.0,
            },
            hour_anchor,
            "America/Los_Angeles".parse().unwrap(),
            SolarEvent::Elevation {
                elevation: -4.0,
                rising: true,
            },
            ChronoDuration::zero(),
        )
        .unwrap();
        assert_eq!(scheduled, expected);
    }

    #[test]
    fn sun_registry_conditions_use_live_location_and_unrounded_solar_helpers() {
        use chrono::TimeZone;

        let runtime = runtime();
        runtime
            .set_home_context(HomeContext {
                latitude: 32.7157,
                longitude: -117.1611,
                elevation: 0.0,
                time_zone: "America/Los_Angeles".into(),
            })
            .unwrap();
        runtime
            .set_state(
                "sun.sun",
                "below_horizon",
                Map::from_iter([("elevation".into(), Value::from(-80.0))]),
                Context::default(),
                false,
            )
            .unwrap();
        let condition: Condition = serde_yaml::from_str("condition: sun.is_up").unwrap();
        let conditions = vec![condition];
        let variables = Map::new();
        let solar_noon = Utc
            .with_ymd_and_hms(2015, 9, 15, 20, 0, 0)
            .single()
            .unwrap();
        assert!(conditions_match_at(&runtime, &conditions, &variables, solar_noon).unwrap());
        let night = Utc
            .with_ymd_and_hms(2015, 9, 15, 8, 30, 0)
            .single()
            .unwrap();
        assert!(!conditions_match_at(&runtime, &conditions, &variables, night).unwrap());

        let legacy: Condition = serde_yaml::from_str(
            "condition: sun\noptions:\n  before: sunrise\n  before_offset: '00:30:00'\n",
        )
        .unwrap();
        validate_condition("sun", &legacy).unwrap();
    }
}
