//! Native Home Assistant timer, input button, input datetime, and schedule
//! helpers. These helpers persist their state through [`hass_core::Runtime`].

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime,
    NaiveTime, Offset, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceHandler, ServiceResult};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const TIMER_METADATA_PREFIX: &str = "hass-native.timer.";
const STOPPED: &str = "idle";
const RUNNING: &str = "active";
const PAUSED: &str = "paused";

/// Top-level native helper configuration. The keys in each map are entity
/// object IDs, matching Home Assistant's flat YAML integration syntax.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    #[serde(default, deserialize_with = "deserialize_defaulted_entries")]
    pub timer: BTreeMap<String, TimerConfig>,
    #[serde(default, deserialize_with = "deserialize_defaulted_entries")]
    pub input_button: BTreeMap<String, InputButtonConfig>,
    pub input_datetime: BTreeMap<String, InputDatetimeConfig>,
    pub schedule: BTreeMap<String, ScheduleConfig>,
    /// IANA time zone used by local schedule transitions and datetime helpers.
    pub time_zone: String,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            timer: BTreeMap::new(),
            input_button: BTreeMap::new(),
            input_datetime: BTreeMap::new(),
            schedule: BTreeMap::new(),
            time_zone: "UTC".into(),
        }
    }
}

fn deserialize_defaulted_entries<'de, D, T>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let entries = BTreeMap::<String, Option<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .map(|(id, value)| (id, value.unwrap_or_default()))
        .collect())
}

impl NativeConfig {
    /// Validate the complete helper configuration without accessing storage or
    /// mutating runtime state. This is suitable for `hass --check`.
    pub fn validate(&self) -> Result<()> {
        let time_zone = self.time_zone.parse::<Tz>().map_err(|_| {
            Error::InvalidInput(format!("unknown IANA time zone {:?}", self.time_zone))
        })?;

        for (id, config) in &self.timer {
            validate_object_id(id, "timer")?;
            let millis = config.duration.millis(false)?;
            if millis < 0 {
                return Err(Error::InvalidInput(format!(
                    "timer.{id}.duration must not be negative"
                )));
            }
            validate_optional_string(config.name.as_deref(), "name", &format!("timer.{id}"))?;
            validate_optional_string(config.icon.as_deref(), "icon", &format!("timer.{id}"))?;
        }
        for (id, config) in &self.input_button {
            validate_object_id(id, "input_button")?;
            validate_optional_string(
                config.name.as_deref(),
                "name",
                &format!("input_button.{id}"),
            )?;
            validate_optional_string(
                config.icon.as_deref(),
                "icon",
                &format!("input_button.{id}"),
            )?;
        }
        for (id, config) in &self.input_datetime {
            validate_object_id(id, "input_datetime")?;
            if !config.has_date && !config.has_time {
                return Err(Error::InvalidInput(format!(
                    "input_datetime.{id} needs has_date or has_time"
                )));
            }
            validate_optional_string(
                config.name.as_deref(),
                "name",
                &format!("input_datetime.{id}"),
            )?;
            validate_optional_string(
                config.icon.as_deref(),
                "icon",
                &format!("input_datetime.{id}"),
            )?;
            if let Some(initial) = config.initial.as_deref().filter(|value| !value.is_empty()) {
                parse_initial_datetime(initial, config, time_zone).map_err(|error| {
                    Error::InvalidInput(format!("input_datetime.{id}.initial: {error}"))
                })?;
            }
        }
        for (id, config) in &self.schedule {
            validate_object_id(id, "schedule")?;
            if config.name.trim().is_empty() {
                return Err(Error::InvalidInput(format!(
                    "schedule.{id}.name must not be empty"
                )));
            }
            validate_optional_string(config.icon.as_deref(), "icon", &format!("schedule.{id}"))?;
            for (day_name, ranges) in config.weekdays() {
                let mut parsed = Vec::with_capacity(ranges.len());
                for (index, range) in ranges.iter().enumerate() {
                    let from = parse_schedule_time(&range.from, false).map_err(|message| {
                        Error::InvalidInput(format!(
                            "schedule.{id}.{day_name}[{index}].from: {message}"
                        ))
                    })?;
                    let to = parse_schedule_time(&range.to, true).map_err(|message| {
                        Error::InvalidInput(format!(
                            "schedule.{id}.{day_name}[{index}].to: {message}"
                        ))
                    })?;
                    if from >= to {
                        return Err(Error::InvalidInput(format!(
                            "schedule.{id}.{day_name}[{index}] has from at or after to"
                        )));
                    }
                    if range.data.values().any(|value| !is_schedule_scalar(value)) {
                        return Err(Error::InvalidInput(format!(
                            "schedule.{id}.{day_name}[{index}].data values must be scalar"
                        )));
                    }
                    parsed.push((from, to, index));
                }
                parsed.sort_by_key(|(from, _, _)| *from);
                let mut previous_to: Option<NaiveTime> = None;
                for (from, to, _) in parsed {
                    if previous_to.is_some_and(|previous| previous > from) {
                        return Err(Error::InvalidInput(format!(
                            "schedule.{id}.{day_name} contains overlapping time ranges"
                        )));
                    }
                    previous_to = Some(to);
                }
            }
        }
        Ok(())
    }
}

/// Configuration for one timer helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimerConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub duration: TimerDuration,
    pub restore: bool,
}

/// A Home Assistant time period: either a numeric number of seconds or a
/// `HH:MM:SS` string. This is also accepted for timer service durations.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TimerDuration {
    Seconds(f64),
    Text(String),
    /// Home Assistant also accepts `days`, `hours`, `minutes`, `seconds`, and
    /// `milliseconds` mappings for time periods.
    Parts(BTreeMap<String, Value>),
}

impl Default for TimerDuration {
    fn default() -> Self {
        Self::Seconds(0.0)
    }
}

impl TimerDuration {
    fn millis(&self, allow_negative: bool) -> Result<i64> {
        let value = match self {
            Self::Seconds(seconds) => {
                if !seconds.is_finite() {
                    return Err(Error::InvalidInput("duration must be finite".into()));
                }
                (seconds * 1000.0).round()
            }
            Self::Text(text) => parse_duration_text(text, allow_negative)? as f64,
            Self::Parts(parts) => {
                if parts.is_empty() {
                    return Err(Error::InvalidInput(
                        "duration mapping cannot be empty".into(),
                    ));
                }
                let mut total_seconds = 0.0;
                for (unit, value) in parts {
                    let scalar = value
                        .as_f64()
                        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
                        .ok_or_else(|| {
                            Error::InvalidInput(format!("duration {unit} must be numeric"))
                        })?;
                    if !scalar.is_finite() {
                        return Err(Error::InvalidInput(format!(
                            "duration {unit} must be finite"
                        )));
                    }
                    let multiplier = match unit.as_str() {
                        "days" => 86_400.0,
                        "hours" => 3_600.0,
                        "minutes" => 60.0,
                        "seconds" => 1.0,
                        "milliseconds" => 0.001,
                        _ => {
                            return Err(Error::InvalidInput(format!(
                                "unknown duration unit {unit:?}"
                            )));
                        }
                    };
                    total_seconds += scalar * multiplier;
                }
                (total_seconds * 1000.0).round()
            }
        };
        if value < 0.0 && !allow_negative {
            return Err(Error::InvalidInput("duration must not be negative".into()));
        }
        if value < i64::MIN as f64 || value > i64::MAX as f64 {
            return Err(Error::InvalidInput(
                "duration is outside the supported range".into(),
            ));
        }
        Ok(value as i64)
    }
}

/// Configuration for an input button helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputButtonConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
}

/// Configuration for one input datetime helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputDatetimeConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub has_date: bool,
    pub has_time: bool,
    pub initial: Option<String>,
}

/// Configuration for one schedule time range.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTimeRange {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub data: BTreeMap<String, Value>,
}

/// Configuration for one local-time weekly schedule.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub name: String,
    pub icon: Option<String>,
    pub monday: Vec<ScheduleTimeRange>,
    pub tuesday: Vec<ScheduleTimeRange>,
    pub wednesday: Vec<ScheduleTimeRange>,
    pub thursday: Vec<ScheduleTimeRange>,
    pub friday: Vec<ScheduleTimeRange>,
    pub saturday: Vec<ScheduleTimeRange>,
    pub sunday: Vec<ScheduleTimeRange>,
}

impl ScheduleConfig {
    fn weekdays(&self) -> [(&'static str, &[ScheduleTimeRange]); 7] {
        [
            ("monday", &self.monday),
            ("tuesday", &self.tuesday),
            ("wednesday", &self.wednesday),
            ("thursday", &self.thursday),
            ("friday", &self.friday),
            ("saturday", &self.saturday),
            ("sunday", &self.sunday),
        ]
    }
}

fn validate_object_id(id: &str, domain: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid {domain} object ID {id:?}: expected lowercase ASCII letters, digits, or underscores"
        )));
    }
    Ok(())
}

fn validate_optional_string(value: Option<&str>, field: &str, entity: &str) -> Result<()> {
    if value.is_some_and(|value| value.contains('\0')) {
        return Err(Error::InvalidInput(format!(
            "{entity}.{field} contains a NUL byte"
        )));
    }
    Ok(())
}

fn is_schedule_scalar(value: &Value) -> bool {
    value.is_boolean() || value.is_string() || value.is_number()
}

fn parse_duration_text(text: &str, allow_negative: bool) -> Result<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidInput("duration cannot be empty".into()));
    }
    if let Ok(seconds) = trimmed.parse::<f64>() {
        if !seconds.is_finite() || (seconds < 0.0 && !allow_negative) {
            return Err(Error::InvalidInput("duration is not valid".into()));
        }
        let millis = (seconds * 1000.0).round();
        if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
            return Err(Error::InvalidInput(
                "duration is outside the supported range".into(),
            ));
        }
        return Ok(millis as i64);
    }
    let (sign, body) = if let Some(body) = trimmed.strip_prefix('-') {
        if !allow_negative {
            return Err(Error::InvalidInput("duration must not be negative".into()));
        }
        (-1_i64, body)
    } else {
        (1_i64, trimmed.strip_prefix('+').unwrap_or(trimmed))
    };
    let parts: Vec<&str> = body.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return Err(Error::InvalidInput(format!("invalid duration {text:?}")));
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        let parsed = part
            .parse::<f64>()
            .map_err(|_| Error::InvalidInput(format!("invalid duration {text:?}")))?;
        if !parsed.is_finite() || parsed < 0.0 {
            return Err(Error::InvalidInput(format!("invalid duration {text:?}")));
        }
        values.push(parsed);
    }
    let (hours, minutes, seconds) = if values.len() == 3 {
        (values[0], values[1], values[2])
    } else {
        (values[0], values[1], 0.0)
    };
    if minutes >= 60.0 || seconds >= 60.0 {
        return Err(Error::InvalidInput(format!("invalid duration {text:?}")));
    }
    let millis = ((hours * 3600.0 + minutes * 60.0 + seconds) * 1000.0).round();
    if millis > i64::MAX as f64 {
        return Err(Error::InvalidInput(
            "duration is outside the supported range".into(),
        ));
    }
    Ok((millis as i64).saturating_mul(sign))
}

fn parse_schedule_time(
    text: &str,
    allow_end_of_day: bool,
) -> std::result::Result<NaiveTime, String> {
    let value = text.trim();
    if allow_end_of_day && (value == "24:00" || value == "24:00:00") {
        return Ok(end_of_day_time());
    }
    NaiveTime::parse_from_str(value, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M"))
        .map_err(|_| format!("invalid local time {text:?}; expected HH:MM or HH:MM:SS"))
}

fn end_of_day_time() -> NaiveTime {
    NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999)
        .expect("23:59:59.999999 is a valid local time")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TimerStatus {
    Idle,
    Active,
    Paused,
}

impl TimerStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => STOPPED,
            Self::Active => RUNNING,
            Self::Paused => PAUSED,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimerMetadata {
    status: TimerStatus,
    running_ms: i64,
    #[serde(default)]
    remaining_ms: Option<i64>,
    #[serde(default)]
    finishes_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_transition: Option<String>,
}

#[derive(Clone, Debug)]
struct TimerState {
    status: TimerStatus,
    configured_ms: i64,
    running_ms: i64,
    remaining_ms: Option<i64>,
    finishes_at: Option<DateTime<Utc>>,
    last_transition: Option<String>,
    generation: u64,
}

impl TimerState {
    fn idle(configured_ms: i64) -> Self {
        Self {
            status: TimerStatus::Idle,
            configured_ms,
            running_ms: configured_ms,
            remaining_ms: None,
            finishes_at: None,
            last_transition: None,
            generation: 0,
        }
    }

    fn metadata(&self) -> TimerMetadata {
        TimerMetadata {
            status: self.status,
            running_ms: self.running_ms,
            remaining_ms: self.remaining_ms,
            finishes_at: self.finishes_at,
            last_transition: self.last_transition.clone(),
        }
    }

    fn remaining_at(&self, now: DateTime<Utc>) -> Option<i64> {
        match self.status {
            TimerStatus::Idle => None,
            TimerStatus::Paused => self.remaining_ms,
            TimerStatus::Active => self
                .finishes_at
                .map(|end| (end - now).num_milliseconds().max(0)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimerDeadline {
    generation: u64,
    delay_ms: u64,
}

struct TimerShared {
    states: Mutex<BTreeMap<String, TimerState>>,
    controllers: BTreeMap<String, watch::Sender<Option<TimerDeadline>>>,
    configs: BTreeMap<String, TimerConfig>,
    active: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
struct ParsedRange {
    from: NaiveTime,
    to: NaiveTime,
    data: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
struct ParsedSchedule {
    id: String,
    config: ScheduleConfig,
    weekdays: [Vec<ParsedRange>; 7],
}

impl ParsedSchedule {
    fn from_config(id: &str, config: &ScheduleConfig) -> Result<Self> {
        let mut weekdays: [Vec<ParsedRange>; 7] = std::array::from_fn(|_| Vec::new());
        for (index, (_, ranges)) in config.weekdays().into_iter().enumerate() {
            for range in ranges {
                weekdays[index].push(ParsedRange {
                    from: parse_schedule_time(&range.from, false).map_err(Error::InvalidInput)?,
                    to: parse_schedule_time(&range.to, true).map_err(Error::InvalidInput)?,
                    data: range.data.clone(),
                });
            }
            weekdays[index].sort_by_key(|range| range.from);
        }
        Ok(Self {
            id: id.to_owned(),
            config: config.clone(),
            weekdays,
        })
    }
}

struct ScheduleShared {
    schedules: BTreeMap<String, ParsedSchedule>,
    active: Arc<AtomicBool>,
}

/// Running native helpers. All timer and schedule tasks are owned here and are
/// cancelled and joined by [`NativeIntegrations::shutdown`].
pub struct NativeIntegrations {
    tasks: Vec<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
    active: Arc<AtomicBool>,
}

impl NativeIntegrations {
    /// Validate configuration, restore timer metadata, register native
    /// services, and start the owned timer/schedule workers.
    pub async fn start(runtime: Runtime, config: NativeConfig) -> Result<Self> {
        config.validate()?;
        let time_zone = config.time_zone.parse::<Tz>().map_err(|_| {
            Error::InvalidInput(format!("unknown IANA time zone {:?}", config.time_zone))
        })?;
        let now = Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now);

        // Read every timer metadata row before changing state or registering a
        // service. A storage failure must leave the runtime untouched.
        let mut old_timer_metadata = BTreeMap::new();
        let mut timer_states = BTreeMap::new();
        let mut startup_events = Vec::new();
        for (id, timer_config) in &config.timer {
            let key = timer_metadata_key(id);
            let old = runtime.get_data(&key)?;
            let duration_ms = timer_config.duration.millis(false)?;
            let mut state = if timer_config.restore {
                restore_timer_state(
                    id,
                    timer_config,
                    duration_ms,
                    old.as_ref(),
                    runtime.get_state(&format!("timer.{id}")).as_ref(),
                )?
            } else {
                TimerState::idle(duration_ms)
            };
            if state.status == TimerStatus::Active {
                match state.finishes_at {
                    Some(end) if end > now => {
                        state.remaining_ms = Some((end - now).num_milliseconds().max(0));
                        state.last_transition = Some("restarted".into());
                        state.generation = state.generation.saturating_add(1);
                        startup_events.push((
                            "timer.restarted".to_owned(),
                            json!({"entity_id": format!("timer.{id}")}),
                        ));
                    }
                    Some(end) => {
                        state = TimerState::idle(duration_ms);
                        state.last_transition = Some("finished".into());
                        startup_events.push((
                            "timer.finished".to_owned(),
                            json!({"entity_id": format!("timer.{id}"), "finished_at": end.to_rfc3339()}),
                        ));
                    }
                    None => {
                        return Err(Error::InvalidInput(format!(
                            "restored active timer.{id} has no finishes_at value"
                        )));
                    }
                }
            }
            old_timer_metadata.insert(id.clone(), old);
            timer_states.insert(id.clone(), state);
        }

        let schedules = config
            .schedule
            .iter()
            .map(|(id, schedule)| {
                ParsedSchedule::from_config(id, schedule).map(|value| (id.clone(), value))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let mut state_writes = Vec::new();
        for (id, state) in &timer_states {
            state_writes.push(timer_state(id, state, &config.timer[id], now));
        }
        // YAML input buttons have no initial state. Their last-press state is
        // restored from the runtime if one exists and is first written on press.
        let datetime_values = config
            .input_datetime
            .iter()
            .map(|(id, helper)| {
                let restored = runtime.get_state(&format!("input_datetime.{id}"));
                let value = initial_datetime_value(helper, restored.as_ref(), time_zone, now);
                Ok((id.clone(), value?))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        for (id, helper) in &config.input_button {
            let entity_id = format!("input_button.{id}");
            let last_press = runtime
                .get_state(&entity_id)
                .map(|state| state.state)
                .unwrap_or_else(|| "unknown".into());
            let mut attributes = Map::new();
            add_identity_attributes(
                &mut attributes,
                helper.name.as_deref(),
                helper.icon.as_deref(),
            );
            attributes.insert("editable".into(), Value::Bool(false));
            state_writes.push((entity_id, last_press, attributes));
        }
        for (id, value) in &datetime_values {
            state_writes.push(datetime_state(
                id,
                &config.input_datetime[id],
                *value,
                time_zone,
            ));
        }
        for schedule in schedules.values() {
            let (entity_id, state, attributes, _) = schedule_state(schedule, now, time_zone);
            state_writes.push((entity_id, state, attributes));
        }

        // Metadata writes precede state changes. Roll back successful metadata
        // writes if a later metadata/state write fails.
        let mut metadata_written = Vec::new();
        for (id, state) in &timer_states {
            let key = timer_metadata_key(id);
            let value = serde_json::to_value(state.metadata()).map_err(|error| {
                Error::Storage(format!("serialize timer.{id} metadata: {error}"))
            })?;
            if let Err(error) = runtime.set_data(&key, &value) {
                rollback_timer_metadata(&runtime, &old_timer_metadata, &metadata_written);
                return Err(error);
            }
            metadata_written.push(id.clone());
        }
        for (entity_id, state, attributes) in &state_writes {
            if let Err(error) = runtime.set_state(
                entity_id,
                state,
                attributes.clone(),
                Context::default(),
                false,
            ) {
                rollback_timer_metadata(&runtime, &old_timer_metadata, &metadata_written);
                return Err(error);
            }
        }
        for (event_type, data) in startup_events {
            runtime.fire_event(&event_type, json_map(data)?, Context::default())?;
        }

        let active = Arc::new(AtomicBool::new(true));
        let (shutdown, _) = watch::channel(false);
        let mut tasks = Vec::new();

        let mut controllers = BTreeMap::new();
        let mut timer_receivers = Vec::new();
        for (id, state) in &timer_states {
            let (sender, receiver) = watch::channel(timer_deadline(state, Utc::now()));
            controllers.insert(id.clone(), sender);
            timer_receivers.push((id.clone(), receiver));
        }
        let timer_shared = Arc::new(TimerShared {
            states: Mutex::new(timer_states),
            controllers,
            configs: config.timer.clone(),
            active: active.clone(),
        });
        if !config.timer.is_empty() {
            register_timer_services(&runtime, timer_shared.clone())?;
        }
        if !config.input_button.is_empty() {
            register_button_service(
                &runtime,
                config.input_button.clone(),
                time_zone,
                active.clone(),
            )?;
        }
        if !config.input_datetime.is_empty() {
            register_datetime_service(
                &runtime,
                config.input_datetime.clone(),
                time_zone,
                active.clone(),
            )?;
        }

        if !schedules.is_empty() {
            let schedule_shared = Arc::new(ScheduleShared {
                schedules: schedules.clone(),
                active: active.clone(),
            });
            register_schedule_service(&runtime, schedule_shared.clone())?;
            for schedule in schedules.into_values() {
                tasks.push(tokio::spawn(schedule_worker(
                    runtime.clone(),
                    schedule,
                    time_zone,
                    shutdown.subscribe(),
                )));
            }
        }

        // Every configured timer has one owned worker, even while idle, so
        // services can activate it without spawning detached tasks.
        for (id, receiver) in timer_receivers {
            tasks.push(tokio::spawn(timer_worker(
                id,
                runtime.clone(),
                timer_shared.clone(),
                receiver,
                shutdown.subscribe(),
            )));
        }

        Ok(Self {
            tasks,
            shutdown,
            active,
        })
    }

    /// Stop and join all timer and schedule tasks started by this integration.
    pub async fn shutdown(mut self) -> Result<()> {
        self.active.store(false, Ordering::Release);
        let _ = self.shutdown.send(true);
        for task in self.tasks.drain(..) {
            task.await
                .map_err(|error| Error::Service(format!("native helper task failed: {error}")))?;
        }
        Ok(())
    }
}

impl Drop for NativeIntegrations {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn timer_metadata_key(id: &str) -> String {
    format!("{TIMER_METADATA_PREFIX}{id}")
}

fn rollback_timer_metadata(
    runtime: &Runtime,
    old: &BTreeMap<String, Option<Value>>,
    written: &[String],
) {
    for id in written.iter().rev() {
        let key = timer_metadata_key(id);
        match old.get(id).and_then(Option::as_ref) {
            Some(value) => {
                let _ = runtime.set_data(&key, value);
            }
            None => {
                let _ = runtime.delete_data(&key);
            }
        }
    }
}

fn restore_timer_state(
    id: &str,
    config: &TimerConfig,
    configured_ms: i64,
    metadata: Option<&Value>,
    old_state: Option<&hass_core::State>,
) -> Result<TimerState> {
    let mut state = if let Some(value) = metadata {
        let saved: TimerMetadata = serde_json::from_value(value.clone()).map_err(|error| {
            Error::InvalidInput(format!(
                "invalid persisted metadata for timer.{id}: {error}"
            ))
        })?;
        TimerState {
            status: saved.status,
            configured_ms,
            running_ms: saved.running_ms.max(0),
            remaining_ms: saved.remaining_ms.map(|value| value.max(0)),
            finishes_at: saved.finishes_at,
            last_transition: saved.last_transition,
            generation: 0,
        }
    } else if let Some(old) = old_state {
        state_from_old_timer(old, configured_ms)?
    } else {
        TimerState::idle(configured_ms)
    };
    state.configured_ms = configured_ms;
    if state.status == TimerStatus::Idle {
        state.running_ms = configured_ms;
        state.remaining_ms = None;
        state.finishes_at = None;
    }
    if state.status == TimerStatus::Paused && state.remaining_ms.is_none() {
        return Err(Error::InvalidInput(format!(
            "restored paused timer.{id} has no remaining duration"
        )));
    }
    if state.status == TimerStatus::Active && state.finishes_at.is_none() {
        return Err(Error::InvalidInput(format!(
            "restored active timer.{id} has no finishes_at value"
        )));
    }
    if !config.restore {
        Ok(TimerState::idle(configured_ms))
    } else {
        Ok(state)
    }
}

fn state_from_old_timer(old: &hass_core::State, configured_ms: i64) -> Result<TimerState> {
    let status = match old.state.as_str() {
        STOPPED => TimerStatus::Idle,
        RUNNING => TimerStatus::Active,
        PAUSED => TimerStatus::Paused,
        _ => return Ok(TimerState::idle(configured_ms)),
    };
    let running_ms = old
        .attributes
        .get("duration")
        .map(parse_json_duration)
        .transpose()?
        .unwrap_or(configured_ms)
        .max(0);
    let remaining_ms = old
        .attributes
        .get("remaining")
        .map(parse_json_duration)
        .transpose()?;
    let finishes_at = old
        .attributes
        .get("finishes_at")
        .and_then(Value::as_str)
        .map(parse_datetime_utc)
        .transpose()?;
    Ok(TimerState {
        status,
        configured_ms,
        running_ms,
        remaining_ms,
        finishes_at,
        last_transition: old
            .attributes
            .get("last_transition")
            .and_then(Value::as_str)
            .map(str::to_owned),
        generation: 0,
    })
}

fn parse_json_duration(value: &Value) -> Result<i64> {
    let period: TimerDuration = serde_json::from_value(value.clone())
        .map_err(|error| Error::InvalidInput(format!("invalid saved timer duration: {error}")))?;
    period.millis(true)
}

fn parse_datetime_utc(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| Error::InvalidInput(format!("invalid saved timer timestamp {text:?}")))
}

fn timer_deadline(state: &TimerState, now: DateTime<Utc>) -> Option<TimerDeadline> {
    if state.status != TimerStatus::Active {
        return None;
    }
    let delay_ms = state
        .finishes_at
        .map(|end| (end - now).num_milliseconds().max(0) as u64)?;
    Some(TimerDeadline {
        generation: state.generation,
        delay_ms,
    })
}

fn timer_state(
    id: &str,
    timer: &TimerState,
    config: &TimerConfig,
    now: DateTime<Utc>,
) -> (String, String, Map<String, Value>) {
    let mut attributes = Map::new();
    add_identity_attributes(
        &mut attributes,
        config.name.as_deref(),
        config.icon.as_deref(),
    );
    attributes.insert(
        "duration".into(),
        Value::String(format_duration(timer.running_ms)),
    );
    attributes.insert("editable".into(), Value::Bool(false));
    if let Some(last_transition) = &timer.last_transition {
        attributes.insert(
            "last_transition".into(),
            Value::String(last_transition.clone()),
        );
    }
    if let Some(end) = timer
        .finishes_at
        .filter(|_| timer.status == TimerStatus::Active)
    {
        attributes.insert("finishes_at".into(), Value::String(end.to_rfc3339()));
    }
    if let Some(remaining) = timer.remaining_at(now) {
        attributes.insert(
            "remaining".into(),
            Value::String(format_duration(remaining)),
        );
    }
    if config.restore {
        attributes.insert("restore".into(), Value::Bool(true));
    }
    (
        format!("timer.{id}"),
        timer.status.as_str().into(),
        attributes,
    )
}

fn format_duration(millis: i64) -> String {
    let negative = millis < 0;
    let total_seconds = millis.unsigned_abs() / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!(
        "{}{hours}:{minutes:02}:{seconds:02}",
        if negative { "-" } else { "" }
    )
}

fn add_identity_attributes(
    attributes: &mut Map<String, Value>,
    name: Option<&str>,
    icon: Option<&str>,
) {
    if let Some(name) = name {
        attributes.insert("friendly_name".into(), Value::String(name.to_owned()));
    }
    if let Some(icon) = icon {
        attributes.insert("icon".into(), Value::String(icon.to_owned()));
    }
}

fn parse_initial_datetime(
    initial: &str,
    config: &InputDatetimeConfig,
    time_zone: Tz,
) -> Result<DateTime<Tz>> {
    let current_date = Utc::now().with_timezone(&time_zone).date_naive();
    if config.has_date
        && config.has_time
        && let Ok(value) = DateTime::parse_from_rfc3339(initial)
    {
        return Ok(value.with_timezone(&time_zone));
    }
    let naive = if config.has_date && config.has_time {
        parse_naive_datetime(initial).ok_or_else(|| {
            Error::InvalidInput(format!("{initial:?} cannot be parsed as a datetime"))
        })?
    } else if config.has_date {
        let date = NaiveDate::parse_from_str(initial, "%Y-%m-%d")
            .map_err(|_| Error::InvalidInput(format!("{initial:?} cannot be parsed as a date")))?;
        date.and_time(NaiveTime::MIN)
    } else {
        let time = parse_schedule_time(initial, false)
            .map_err(|_| Error::InvalidInput(format!("{initial:?} cannot be parsed as a time")))?;
        current_date.and_time(time)
    };
    localize_datetime(time_zone, naive)
        .ok_or_else(|| Error::InvalidInput(format!("{initial:?} is not a valid local datetime")))
}

fn parse_naive_datetime(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok())
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").ok())
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M").ok())
}

fn initial_datetime_value(
    config: &InputDatetimeConfig,
    old_state: Option<&hass_core::State>,
    time_zone: Tz,
    now: DateTime<Utc>,
) -> Result<DateTime<Tz>> {
    if let Some(initial) = config.initial.as_deref().filter(|value| !value.is_empty()) {
        return parse_initial_datetime(initial, config, time_zone);
    }
    if let Some(old) = old_state
        && let Ok(value) = parse_datetime_state(&old.state, config, time_zone)
    {
        return Ok(value);
    }
    let local = now.with_timezone(&time_zone);
    let default = local.date_naive().and_time(NaiveTime::MIN);
    localize_datetime(time_zone, default)
        .ok_or_else(|| Error::InvalidInput("cannot construct the default local datetime".into()))
}

fn parse_datetime_state(
    state: &str,
    config: &InputDatetimeConfig,
    time_zone: Tz,
) -> Result<DateTime<Tz>> {
    let naive = if config.has_date && config.has_time {
        parse_naive_datetime(state)
            .ok_or_else(|| Error::InvalidInput(format!("invalid input_datetime state {state:?}")))?
    } else if config.has_date {
        NaiveDate::parse_from_str(state, "%Y-%m-%d")
            .map_err(|_| Error::InvalidInput(format!("invalid input_datetime date {state:?}")))?
            .and_time(NaiveTime::MIN)
    } else {
        let time = parse_schedule_time(state, false)
            .map_err(|_| Error::InvalidInput(format!("invalid input_datetime time {state:?}")))?;
        let date = Utc::now().with_timezone(&time_zone).date_naive();
        date.and_time(time)
    };
    localize_datetime(time_zone, naive)
        .ok_or_else(|| Error::InvalidInput(format!("invalid local input_datetime {state:?}")))
}

fn datetime_state(
    id: &str,
    config: &InputDatetimeConfig,
    value: DateTime<Tz>,
    time_zone: Tz,
) -> (String, String, Map<String, Value>) {
    let state = if config.has_date && config.has_time {
        value.format("%Y-%m-%d %H:%M:%S").to_string()
    } else if config.has_date {
        value.format("%Y-%m-%d").to_string()
    } else {
        value.format("%H:%M:%S").to_string()
    };
    let mut attributes = Map::new();
    add_identity_attributes(
        &mut attributes,
        config.name.as_deref(),
        config.icon.as_deref(),
    );
    attributes.insert("editable".into(), Value::Bool(false));
    attributes.insert("has_date".into(), Value::Bool(config.has_date));
    attributes.insert("has_time".into(), Value::Bool(config.has_time));
    if config.has_date {
        attributes.insert("year".into(), json!(value.year()));
        attributes.insert("month".into(), json!(value.month()));
        attributes.insert("day".into(), json!(value.day()));
    }
    if config.has_time {
        attributes.insert("hour".into(), json!(value.hour()));
        attributes.insert("minute".into(), json!(value.minute()));
        attributes.insert("second".into(), json!(value.second()));
    }
    let timestamp = if !config.has_date {
        i64::from(value.hour()) * 3600 + i64::from(value.minute()) * 60 + i64::from(value.second())
    } else if !config.has_time {
        let midnight = value.date_naive().and_time(NaiveTime::MIN);
        localize_datetime(time_zone, midnight)
            .map(|value| value.timestamp())
            .unwrap_or_else(|| value.timestamp())
    } else {
        value.timestamp()
    };
    attributes.insert("timestamp".into(), json!(timestamp));
    (format!("input_datetime.{id}"), state, attributes)
}

fn localize_datetime(time_zone: Tz, naive: NaiveDateTime) -> Option<DateTime<Tz>> {
    match time_zone.from_local_datetime(&naive) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(first, second) => Some(first.min(second)),
        LocalResult::None => {
            // A configured wall time can fall inside a daylight-saving gap.
            // Move to the first representable minute after that gap.
            for minutes in 1..=180 {
                let candidate = naive.checked_add_signed(ChronoDuration::minutes(minutes))?;
                match time_zone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) => return Some(value),
                    LocalResult::Ambiguous(first, second) => return Some(first.min(second)),
                    LocalResult::None => {}
                }
            }
            None
        }
    }
}

fn localize_schedule_datetime(
    time_zone: Tz,
    naive: NaiveDateTime,
    reference: DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    match time_zone.from_local_datetime(&naive) {
        LocalResult::Ambiguous(first, second) => {
            let reference_offset = reference.offset().fix().local_minus_utc();
            if first.offset().fix().local_minus_utc() == reference_offset {
                Some(first)
            } else if second.offset().fix().local_minus_utc() == reference_offset {
                Some(second)
            } else {
                Some(first.min(second))
            }
        }
        LocalResult::Single(value) => Some(value),
        LocalResult::None => localize_datetime(time_zone, naive),
    }
}

fn schedule_state(
    schedule: &ParsedSchedule,
    now_utc: DateTime<Utc>,
    time_zone: Tz,
) -> (String, String, Map<String, Value>, Option<DateTime<Tz>>) {
    let now = now_utc.with_timezone(&time_zone);
    let mut current_data: Option<&BTreeMap<String, Value>> = None;
    let today = &schedule.weekdays[now.weekday().num_days_from_monday() as usize];
    for range in today {
        if now.time() >= range.from && (now.time() < range.to || range.to == end_of_day_time()) {
            current_data = Some(&range.data);
            break;
        }
    }
    let next_event = next_schedule_event(schedule, now, time_zone);
    let mut attributes = Map::new();
    add_identity_attributes(
        &mut attributes,
        Some(schedule.config.name.as_str()),
        schedule.config.icon.as_deref(),
    );
    attributes.insert("editable".into(), Value::Bool(false));
    attributes.insert(
        "next_event".into(),
        next_event
            .map(|value| Value::String(value.to_rfc3339()))
            .unwrap_or(Value::Null),
    );
    if let Some(data) = current_data {
        attributes.extend(data.clone());
    }
    (
        format!("schedule.{}", schedule.id),
        if current_data.is_some() { "on" } else { "off" }.into(),
        attributes,
        next_event,
    )
}

fn next_schedule_event(
    schedule: &ParsedSchedule,
    now: DateTime<Tz>,
    time_zone: Tz,
) -> Option<DateTime<Tz>> {
    let mut candidates = Vec::new();
    for day_offset in 0..=7_i64 {
        let date = now
            .date_naive()
            .checked_add_signed(ChronoDuration::days(day_offset))?;
        let weekday = date.weekday().num_days_from_monday() as usize;
        for range in &schedule.weekdays[weekday] {
            candidates.push(date.and_time(range.from));
            if range.to == end_of_day_time() {
                let next_date = date.succ_opt()?;
                candidates.push(next_date.and_time(NaiveTime::MIN));
            } else {
                candidates.push(date.and_time(range.to));
            }
        }
    }
    candidates.sort();
    for naive in candidates {
        if let Some(candidate) =
            localize_schedule_datetime(time_zone, naive, now).filter(|value| *value > now)
        {
            return Some(candidate);
        }
    }
    None
}

fn schedule_service_value(schedule: &ParsedSchedule) -> Value {
    let mut result = serde_json::Map::new();
    for (day_name, ranges) in schedule.config.weekdays() {
        let mut ordered_ranges: Vec<_> = ranges.iter().collect();
        ordered_ranges.sort_by_key(|range| parse_schedule_time(&range.from, false).ok());
        let values = ordered_ranges
            .into_iter()
            .map(|range| {
                let mut value = serde_json::Map::new();
                value.insert(
                    "from".into(),
                    Value::String(normalized_schedule_time(&range.from, false)),
                );
                value.insert(
                    "to".into(),
                    Value::String(normalized_schedule_time(&range.to, true)),
                );
                if !range.data.is_empty() {
                    value.insert(
                        "data".into(),
                        serde_json::to_value(&range.data).unwrap_or(Value::Null),
                    );
                }
                Value::Object(value)
            })
            .collect();
        result.insert(day_name.into(), Value::Array(values));
    }
    Value::Object(result)
}

fn normalized_schedule_time(value: &str, allow_end_of_day: bool) -> String {
    match parse_schedule_time(value, allow_end_of_day) {
        Ok(time) if time == end_of_day_time() => "23:59:59.999999".into(),
        Ok(time) => time.format("%H:%M:%S").to_string(),
        Err(_) => value.to_owned(),
    }
}

fn json_map(value: Value) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Error::InvalidInput(
            "event data must be a JSON object".into(),
        )),
    }
}

fn target_entities(call: &ServiceCall, domain: &str) -> Result<Vec<String>> {
    for key in call.target.keys() {
        if key != "entity_id" {
            return Err(Error::InvalidInput(format!(
                "unsupported {domain} service target {key:?}; use entity_id"
            )));
        }
    }
    let raw = call
        .target
        .get("entity_id")
        .or_else(|| call.data.get("entity_id"))
        .ok_or_else(|| {
            Error::InvalidInput(format!("{domain} service requires entity_id target"))
        })?;
    let mut ids = Vec::new();
    match raw {
        Value::String(entity_id) => ids.push(entity_id.clone()),
        Value::Array(values) => {
            for value in values {
                let entity_id = value.as_str().ok_or_else(|| {
                    Error::InvalidInput(format!("{domain} entity_id list must contain strings"))
                })?;
                ids.push(entity_id.to_owned());
            }
        }
        _ => {
            return Err(Error::InvalidInput(format!(
                "{domain} entity_id must be a string or list of strings"
            )));
        }
    }
    if ids.is_empty() {
        return Err(Error::InvalidInput(format!(
            "{domain} service target is empty"
        )));
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn validate_service_fields(call: &ServiceCall, allowed: &[&str], domain: &str) -> Result<()> {
    for key in call.data.keys() {
        if key != "entity_id" && !allowed.contains(&key.as_str()) {
            return Err(Error::InvalidInput(format!(
                "unsupported {domain} service field {key:?}"
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum TimerAction {
    Start(Option<i64>),
    Pause,
    Cancel,
    Finish,
    Change(i64),
}

fn register_timer_services(runtime: &Runtime, shared: Arc<TimerShared>) -> Result<()> {
    for service in ["start", "pause", "cancel", "finish", "change"] {
        let shared = shared.clone();
        let handler: ServiceHandler = Arc::new(move |runtime, call| {
            let action = match call.service.as_str() {
                "start" => {
                    validate_service_fields(call, &["duration"], "timer")?;
                    TimerAction::Start(
                        call.data
                            .get("duration")
                            .map(|value| parse_duration_value(value, false))
                            .transpose()?,
                    )
                }
                "pause" => {
                    validate_service_fields(call, &[], "timer")?;
                    TimerAction::Pause
                }
                "cancel" => {
                    validate_service_fields(call, &[], "timer")?;
                    TimerAction::Cancel
                }
                "finish" => {
                    validate_service_fields(call, &[], "timer")?;
                    TimerAction::Finish
                }
                "change" => {
                    validate_service_fields(call, &["duration"], "timer")?;
                    TimerAction::Change(
                        call.data
                            .get("duration")
                            .map(|duration| parse_duration_value(duration, true))
                            .transpose()?
                            .unwrap_or(0),
                    )
                }
                _ => {
                    return Err(Error::Service(format!(
                        "unknown timer service {}",
                        call.service
                    )));
                }
            };
            let targets = target_entities(call, "timer")?;
            for entity_id in &targets {
                let object_id = entity_id.strip_prefix("timer.").ok_or_else(|| {
                    Error::InvalidInput(format!("timer service cannot target {entity_id:?}"))
                })?;
                if !shared.configs.contains_key(object_id) {
                    return Err(Error::NotFound(format!("timer helper {entity_id}")));
                }
            }
            let mut changed_states = Vec::new();
            for entity_id in targets {
                let object_id = entity_id
                    .strip_prefix("timer.")
                    .expect("validated timer target");
                if let Some(state) = transition_timer(
                    runtime,
                    &shared,
                    object_id,
                    action,
                    call.context.clone(),
                    None,
                )? {
                    changed_states.push(state);
                }
            }
            Ok(ServiceResult {
                changed_states,
                response: None,
                context: call.context.clone(),
            })
        });
        runtime.register_service("timer", service, json!({}), handler)?;
    }
    Ok(())
}

fn parse_duration_value(value: &Value, allow_negative: bool) -> Result<i64> {
    let period: TimerDuration = serde_json::from_value(value.clone())
        .map_err(|error| Error::InvalidInput(format!("invalid timer duration: {error}")))?;
    period.millis(allow_negative)
}

fn transition_timer(
    runtime: &Runtime,
    shared: &TimerShared,
    id: &str,
    action: TimerAction,
    context: Context,
    expected_generation: Option<u64>,
) -> Result<Option<hass_core::State>> {
    if !shared.active.load(Ordering::Acquire) {
        return Err(Error::Service("native integrations are shut down".into()));
    }
    let mut states = shared
        .states
        .lock()
        .map_err(|_| Error::Storage("native timer state lock poisoned".into()))?;
    let original = states
        .get(id)
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("timer.{id}")))?;
    if expected_generation.is_some_and(|generation| {
        original.generation != generation || original.status != TimerStatus::Active
    }) {
        return Ok(None);
    }
    let mut next = original.clone();
    let now = Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now);
    let mut finished_at: Option<DateTime<Utc>> = None;
    let event_type = match action {
        TimerAction::Start(requested_duration) => {
            let was_running = next.status != TimerStatus::Idle;
            let current_remaining = next.remaining_at(now);
            let duration = requested_duration
                .filter(|duration| *duration != 0)
                .or(current_remaining)
                .unwrap_or(next.running_ms);
            if duration < 0 {
                return Err(Error::InvalidInput(
                    "timer.start duration must not be negative".into(),
                ));
            }
            next.status = TimerStatus::Active;
            next.running_ms = requested_duration
                .filter(|duration| *duration != 0)
                .unwrap_or(next.running_ms);
            next.remaining_ms = Some(duration);
            next.finishes_at = Some(
                now.checked_add_signed(ChronoDuration::milliseconds(duration))
                    .ok_or_else(|| {
                        Error::InvalidInput("timer deadline is outside supported range".into())
                    })?,
            );
            let event_name = if was_running { "restarted" } else { "started" };
            next.last_transition = Some(event_name.into());
            Some(if was_running {
                "timer.restarted"
            } else {
                "timer.started"
            })
        }
        TimerAction::Pause => {
            if next.status != TimerStatus::Active || next.finishes_at.is_none() {
                return Ok(None);
            }
            next.remaining_ms = next
                .finishes_at
                .map(|end| (end - now).num_milliseconds().max(0));
            next.status = TimerStatus::Paused;
            next.finishes_at = None;
            next.last_transition = Some("paused".into());
            Some("timer.paused")
        }
        TimerAction::Cancel => {
            if next.status == TimerStatus::Idle {
                return Ok(None);
            }
            next.status = TimerStatus::Idle;
            next.running_ms = next.configured_ms;
            next.remaining_ms = None;
            next.finishes_at = None;
            next.last_transition = Some("cancelled".into());
            Some("timer.cancelled")
        }
        TimerAction::Finish => {
            if next.status == TimerStatus::Idle {
                return Ok(None);
            }
            finished_at = Some(next.finishes_at.unwrap_or(now));
            next.status = TimerStatus::Idle;
            next.running_ms = next.configured_ms;
            next.remaining_ms = None;
            next.finishes_at = None;
            next.last_transition = Some("finished".into());
            Some("timer.finished")
        }
        TimerAction::Change(delta_ms) => {
            let end = next
                .finishes_at
                .filter(|_| next.status == TimerStatus::Active)
                .ok_or_else(|| Error::Service(format!("timer.{id} is not running")))?;
            let adjusted_end = end
                .checked_add_signed(ChronoDuration::milliseconds(delta_ms))
                .ok_or_else(|| {
                    Error::InvalidInput("timer.change result is outside supported range".into())
                })?;
            let remaining = (adjusted_end - now).num_milliseconds();
            if next.remaining_at(now).is_some_and(|old| old > 0) && remaining > next.running_ms {
                return Err(Error::InvalidInput(format!(
                    "timer.change would exceed the running duration for timer.{id}"
                )));
            }
            if next.remaining_at(now).is_some_and(|old| old > 0) && remaining < 0 {
                return Err(Error::InvalidInput(format!(
                    "timer.change would make the remaining duration negative for timer.{id}"
                )));
            }
            next.finishes_at = Some(adjusted_end);
            next.remaining_ms = Some(remaining.max(0));
            // `timer.changed` does not change last_transition upstream.
            Some("timer.changed")
        }
    };
    next.generation = next.generation.saturating_add(1);

    let key = timer_metadata_key(id);
    let old_metadata = runtime.get_data(&key)?;
    let metadata_value = serde_json::to_value(next.metadata())
        .map_err(|error| Error::Storage(format!("serialize timer.{id} metadata: {error}")))?;
    runtime.set_data(&key, &metadata_value)?;
    let config = shared
        .configs
        .get(id)
        .ok_or_else(|| Error::NotFound(format!("timer.{id}")))?;
    let (entity_id, state_value, attributes) = timer_state(id, &next, config, now);
    let state_change =
        match runtime.set_state(&entity_id, &state_value, attributes, context.clone(), false) {
            Ok(change) => change,
            Err(error) => {
                match old_metadata {
                    Some(value) => {
                        let _ = runtime.set_data(&key, &value);
                    }
                    None => {
                        let _ = runtime.delete_data(&key);
                    }
                }
                return Err(error);
            }
        };
    states.insert(id.to_owned(), next.clone());
    if let Some(controller) = shared.controllers.get(id) {
        controller.send_replace(timer_deadline(&next, now));
    }
    drop(states);

    if let Some(event_type) = event_type {
        let mut data = Map::new();
        data.insert("entity_id".into(), Value::String(entity_id));
        if let Some(finished_at) = finished_at {
            data.insert(
                "finished_at".into(),
                Value::String(finished_at.to_rfc3339()),
            );
        }
        runtime.fire_event(event_type, data, context)?;
    }
    Ok(Some(state_change.state))
}

async fn timer_worker(
    id: String,
    runtime: Runtime,
    shared: Arc<TimerShared>,
    mut deadline_rx: watch::Receiver<Option<TimerDeadline>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() || !shared.active.load(Ordering::Acquire) {
            return;
        }
        let Some(deadline) = *deadline_rx.borrow() else {
            tokio::select! {
                changed = deadline_rx.changed() => if changed.is_err() { return; },
                changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
            }
            continue;
        };
        tokio::select! {
            _ = sleep(StdDuration::from_millis(deadline.delay_ms)) => {
                if let Err(error) = transition_timer(
                    &runtime,
                    &shared,
                    &id,
                    TimerAction::Finish,
                    Context::default(),
                    Some(deadline.generation),
                ) {
                    tracing::warn!(entity_id = %format!("timer.{id}"), %error, "timer finish persistence failed; retrying");
                    tokio::select! {
                        _ = sleep(StdDuration::from_secs(1)) => {},
                        changed = deadline_rx.changed() => if changed.is_err() { return; },
                        changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
                    }
                }
            }
            changed = deadline_rx.changed() => if changed.is_err() { return; },
            changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
        }
    }
}

fn register_button_service(
    runtime: &Runtime,
    configs: BTreeMap<String, InputButtonConfig>,
    _time_zone: Tz,
    active: Arc<AtomicBool>,
) -> Result<()> {
    let handler: ServiceHandler = Arc::new(move |runtime, call| {
        validate_service_fields(call, &[], "input_button")?;
        if !active.load(Ordering::Acquire) {
            return Err(Error::Service("native integrations are shut down".into()));
        }
        let targets = target_entities(call, "input_button")?;
        for entity_id in &targets {
            let object_id = entity_id.strip_prefix("input_button.").ok_or_else(|| {
                Error::InvalidInput(format!("input_button.press cannot target {entity_id:?}"))
            })?;
            if !configs.contains_key(object_id) {
                return Err(Error::NotFound(format!("input button {entity_id}")));
            }
        }
        let pressed_at = Utc::now().to_rfc3339();
        let mut changed_states = Vec::new();
        for entity_id in targets {
            let object_id = entity_id
                .strip_prefix("input_button.")
                .expect("validated button target");
            let config = &configs[object_id];
            let mut attributes = Map::new();
            add_identity_attributes(
                &mut attributes,
                config.name.as_deref(),
                config.icon.as_deref(),
            );
            attributes.insert("editable".into(), Value::Bool(false));
            let change = runtime.set_state(
                &entity_id,
                &pressed_at,
                attributes,
                call.context.clone(),
                false,
            )?;
            changed_states.push(change.state);
        }
        Ok(ServiceResult {
            changed_states,
            response: None,
            context: call.context.clone(),
        })
    });
    runtime.register_service("input_button", "press", json!({}), handler)
}

#[derive(Clone, Debug, Default)]
struct DatetimeInput {
    date: Option<NaiveDate>,
    time: Option<NaiveTime>,
}

fn parse_datetime_service(data: &Map<String, Value>, time_zone: Tz) -> Result<DatetimeInput> {
    for key in data.keys() {
        if !["entity_id", "date", "time", "datetime", "timestamp"].contains(&key.as_str()) {
            return Err(Error::InvalidInput(format!(
                "unsupported input_datetime.set_datetime field {key:?}"
            )));
        }
    }
    let has_date = data.contains_key("date");
    let has_time = data.contains_key("time");
    let has_datetime = data.contains_key("datetime");
    let has_timestamp = data.contains_key("timestamp");
    if !(has_date || has_time || has_datetime || has_timestamp) {
        return Err(Error::InvalidInput(
            "input_datetime.set_datetime requires date, time, datetime, or timestamp".into(),
        ));
    }
    if (has_datetime || has_timestamp) && (has_date || has_time) || has_datetime && has_timestamp {
        return Err(Error::InvalidInput(
            "input_datetime.set_datetime cannot combine date/time with datetime or timestamp"
                .into(),
        ));
    }
    if let Some(value) = data.get("datetime") {
        let text = value.as_str().ok_or_else(|| {
            Error::InvalidInput("input_datetime datetime must be a string".into())
        })?;
        let local = if let Ok(date_time) = DateTime::parse_from_rfc3339(text) {
            date_time.with_timezone(&time_zone)
        } else {
            let naive = parse_naive_datetime(text)
                .ok_or_else(|| Error::InvalidInput(format!("invalid datetime {text:?}")))?;
            localize_datetime(time_zone, naive)
                .ok_or_else(|| Error::InvalidInput(format!("invalid local datetime {text:?}")))?
        };
        return Ok(DatetimeInput {
            date: Some(local.date_naive()),
            time: Some(local.time()),
        });
    }
    if let Some(value) = data.get("timestamp") {
        let seconds = value.as_f64().ok_or_else(|| {
            Error::InvalidInput("input_datetime timestamp must be numeric".into())
        })?;
        if !seconds.is_finite() {
            return Err(Error::InvalidInput(
                "input_datetime timestamp must be finite".into(),
            ));
        }
        let whole_seconds = seconds.floor();
        if whole_seconds < i64::MIN as f64 || whole_seconds > i64::MAX as f64 {
            return Err(Error::InvalidInput(
                "input_datetime timestamp is out of range".into(),
            ));
        }
        let nanos = ((seconds - whole_seconds) * 1_000_000_000.0).round() as u32;
        let utc = DateTime::from_timestamp(whole_seconds as i64, nanos).ok_or_else(|| {
            Error::InvalidInput("input_datetime timestamp is out of range".into())
        })?;
        let local = utc.with_timezone(&time_zone);
        return Ok(DatetimeInput {
            date: Some(local.date_naive()),
            time: Some(local.time()),
        });
    }
    let date = data
        .get("date")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("input_datetime date must be a string".into()))
                .and_then(|text| {
                    NaiveDate::parse_from_str(text, "%Y-%m-%d")
                        .map_err(|_| Error::InvalidInput(format!("invalid date {text:?}")))
                })
        })
        .transpose()?;
    let time = data
        .get("time")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("input_datetime time must be a string".into()))
                .and_then(|text| {
                    parse_schedule_time(text, false)
                        .map_err(|_| Error::InvalidInput(format!("invalid time {text:?}")))
                })
        })
        .transpose()?;
    Ok(DatetimeInput { date, time })
}

fn register_datetime_service(
    runtime: &Runtime,
    configs: BTreeMap<String, InputDatetimeConfig>,
    time_zone: Tz,
    active: Arc<AtomicBool>,
) -> Result<()> {
    let handler: ServiceHandler = Arc::new(move |runtime, call| {
        if !active.load(Ordering::Acquire) {
            return Err(Error::Service("native integrations are shut down".into()));
        }
        let values = parse_datetime_service(&call.data, time_zone)?;
        let targets = target_entities(call, "input_datetime")?;
        let mut updates = Vec::with_capacity(targets.len());
        for entity_id in &targets {
            let object_id = entity_id.strip_prefix("input_datetime.").ok_or_else(|| {
                Error::InvalidInput(format!(
                    "input_datetime.set_datetime cannot target {entity_id:?}"
                ))
            })?;
            let config = configs
                .get(object_id)
                .ok_or_else(|| Error::NotFound(format!("input datetime {entity_id}")))?;
            let current = runtime
                .get_state(entity_id)
                .map(|state| parse_datetime_state(&state.state, config, time_zone))
                .transpose()?;
            let current = match current {
                Some(value) => value,
                None => initial_datetime_value(config, None, time_zone, Utc::now())?,
            };
            let date = if config.has_date { values.date } else { None };
            let time = if config.has_time { values.time } else { None };
            if date.is_none() && time.is_none() {
                return Err(Error::InvalidInput(format!(
                    "input_datetime.set_datetime has no applicable field for {entity_id}"
                )));
            }
            let naive = date
                .unwrap_or_else(|| current.date_naive())
                .and_time(time.unwrap_or_else(|| current.time()));
            let next = localize_datetime(time_zone, naive).ok_or_else(|| {
                Error::InvalidInput(format!("invalid local datetime for {entity_id}"))
            })?;
            updates.push((object_id.to_owned(), next));
        }
        let mut changed_states = Vec::new();
        for (object_id, value) in updates {
            let (entity_id, state, attributes) =
                datetime_state(&object_id, &configs[&object_id], value, time_zone);
            let change =
                runtime.set_state(&entity_id, &state, attributes, call.context.clone(), false)?;
            changed_states.push(change.state);
        }
        Ok(ServiceResult {
            changed_states,
            response: None,
            context: call.context.clone(),
        })
    });
    runtime.register_service("input_datetime", "set_datetime", json!({}), handler)
}

fn register_schedule_service(runtime: &Runtime, shared: Arc<ScheduleShared>) -> Result<()> {
    let handler: ServiceHandler = Arc::new(move |_runtime, call| {
        if !shared.active.load(Ordering::Acquire) {
            return Err(Error::Service("native integrations are shut down".into()));
        }
        validate_service_fields(call, &[], "schedule")?;
        let targets = target_entities(call, "schedule")?;
        if targets.len() != 1 {
            return Err(Error::InvalidInput(
                "schedule.get_schedule requires exactly one entity_id".into(),
            ));
        }
        let object_id = targets[0].strip_prefix("schedule.").ok_or_else(|| {
            Error::InvalidInput("schedule.get_schedule target must use schedule domain".into())
        })?;
        let schedule = shared
            .schedules
            .get(object_id)
            .ok_or_else(|| Error::NotFound(format!("schedule.{}", object_id)))?;
        Ok(ServiceResult {
            changed_states: Vec::new(),
            response: Some(schedule_service_value(schedule)),
            context: call.context.clone(),
        })
    });
    runtime.register_service(
        "schedule",
        "get_schedule",
        json!({"supports_response":"only"}),
        handler,
    )
}

async fn schedule_worker(
    runtime: Runtime,
    schedule: ParsedSchedule,
    time_zone: Tz,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        let now = Utc::now();
        let (entity_id, state, attributes, next_event) = schedule_state(&schedule, now, time_zone);
        if let Err(error) =
            runtime.set_state(&entity_id, &state, attributes, Context::default(), false)
        {
            tracing::warn!(%entity_id, %error, "schedule state persistence failed");
            tokio::select! {
                _ = sleep(StdDuration::from_secs(1)) => {},
                changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
            }
            continue;
        }
        let Some(next_event) = next_event else {
            tokio::select! {
                changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
            }
            continue;
        };
        let delay = (next_event.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        tokio::select! {
            _ = sleep(delay) => {},
            changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { return; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{Event, MemoryStore, State, Store};
    use std::sync::atomic::AtomicBool;

    fn service_call(domain: &str, service: &str, target: Value, data: Value) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: json_map(data).expect("test service data is an object"),
            target: json_map(target).expect("test service target is an object"),
            context: Context::default(),
        }
    }

    fn timer_config(duration_seconds: f64, restore: bool) -> NativeConfig {
        NativeConfig {
            timer: BTreeMap::from([(
                "kettle".into(),
                TimerConfig {
                    name: Some("Kettle".into()),
                    duration: TimerDuration::Seconds(duration_seconds),
                    restore,
                    ..TimerConfig::default()
                },
            )]),
            ..NativeConfig::default()
        }
    }

    #[tokio::test]
    async fn timer_counts_down_emits_events_and_finishes_idle() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = NativeIntegrations::start(runtime.clone(), timer_config(0.05, true))
            .await
            .unwrap();
        runtime
            .call_service(service_call(
                "timer",
                "start",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, RUNNING);
        tokio::time::sleep(StdDuration::from_millis(120)).await;
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, STOPPED);
        let events = store.events().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "timer.started")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "timer.finished")
        );
        assert_eq!(
            events
                .iter()
                .find(|event| event.event_type == "timer.finished")
                .unwrap()
                .data["entity_id"],
            "timer.kettle"
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timer_change_and_cancel_validate_duration_shapes_and_bounds() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = NativeIntegrations::start(runtime.clone(), timer_config(3.0, true))
            .await
            .unwrap();
        runtime
            .call_service(service_call(
                "timer",
                "start",
                json!({"entity_id":"timer.kettle"}),
                json!({"duration":{"minutes":0,"seconds":2}}),
            ))
            .unwrap();
        runtime
            .call_service(service_call(
                "timer",
                "change",
                json!({"entity_id":"timer.kettle"}),
                json!({"duration":-1}),
            ))
            .unwrap();
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, RUNNING);
        assert!(
            runtime
                .call_service(service_call(
                    "timer",
                    "change",
                    json!({"entity_id":"timer.kettle"}),
                    json!({"duration":5}),
                ))
                .is_err()
        );
        runtime
            .call_service(service_call(
                "timer",
                "cancel",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, STOPPED);
        assert_eq!(
            runtime.get_state("timer.kettle").unwrap().attributes["last_transition"],
            "cancelled"
        );
        let events = store.events().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "timer.changed")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "timer.cancelled")
        );
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn timer_duration_accepts_upstream_clock_and_component_forms() {
        assert_eq!(
            TimerDuration::Text("01:30".into()).millis(false).unwrap(),
            5_400_000
        );
        assert_eq!(
            TimerDuration::Parts(BTreeMap::from([
                ("days".into(), json!(1)),
                ("hours".into(), json!(2)),
                ("minutes".into(), json!(3)),
                ("seconds".into(), json!(4)),
                ("milliseconds".into(), json!(5)),
            ]))
            .millis(false)
            .unwrap(),
            93_784_005
        );
    }

    #[tokio::test]
    async fn paused_timer_restores_across_runtime_restart_and_resumes() {
        let store = Arc::new(MemoryStore::default());
        let config = timer_config(0.18, true);
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = NativeIntegrations::start(runtime.clone(), config.clone())
            .await
            .unwrap();
        runtime
            .call_service(service_call(
                "timer",
                "start",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(35)).await;
        runtime
            .call_service(service_call(
                "timer",
                "pause",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        let before = runtime.get_state("timer.kettle").unwrap();
        assert_eq!(before.state, PAUSED);
        let remaining_before = before.attributes["remaining"].as_str().unwrap().to_owned();
        integration.shutdown().await.unwrap();
        drop(runtime);

        let restarted_runtime = Runtime::new(store).unwrap();
        let restarted = NativeIntegrations::start(restarted_runtime.clone(), config)
            .await
            .unwrap();
        let restored = restarted_runtime.get_state("timer.kettle").unwrap();
        assert_eq!(restored.state, PAUSED);
        assert_eq!(restored.attributes["remaining"], remaining_before);
        restarted_runtime
            .call_service(service_call(
                "timer",
                "start",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(220)).await;
        assert_eq!(
            restarted_runtime.get_state("timer.kettle").unwrap().state,
            STOPPED
        );
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_timer_workers_without_finishing_them() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = NativeIntegrations::start(runtime.clone(), timer_config(0.08, true))
            .await
            .unwrap();
        runtime
            .call_service(service_call(
                "timer",
                "start",
                json!({"entity_id":"timer.kettle"}),
                json!({}),
            ))
            .unwrap();
        integration.shutdown().await.unwrap();
        tokio::time::sleep(StdDuration::from_millis(120)).await;
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, RUNNING);
        assert!(
            !store
                .events()
                .unwrap()
                .iter()
                .any(|event| event.event_type == "timer.finished")
        );
    }

    #[tokio::test]
    async fn input_button_press_writes_utc_timestamp_and_state_event() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = NativeConfig {
            input_button: BTreeMap::from([(
                "doorbell".into(),
                InputButtonConfig {
                    name: Some("Doorbell".into()),
                    ..InputButtonConfig::default()
                },
            )]),
            ..NativeConfig::default()
        };
        let integration = NativeIntegrations::start(runtime.clone(), config)
            .await
            .unwrap();
        assert_eq!(
            runtime.get_state("input_button.doorbell").unwrap().state,
            "unknown"
        );
        runtime
            .call_service(service_call(
                "input_button",
                "press",
                json!({"entity_id":"input_button.doorbell"}),
                json!({}),
            ))
            .unwrap();
        let state = runtime.get_state("input_button.doorbell").unwrap();
        assert!(DateTime::parse_from_rfc3339(&state.state).is_ok());
        assert_eq!(state.attributes["friendly_name"], "Doorbell");
        assert!(
            store
                .events()
                .unwrap()
                .iter()
                .any(|event| event.event_type == "state_changed")
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn input_button_restores_last_press_state() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime
            .set_state(
                "input_button.doorbell",
                "2021-01-01T23:59:59+00:00",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let config = NativeConfig {
            input_button: BTreeMap::from([("doorbell".into(), InputButtonConfig::default())]),
            ..NativeConfig::default()
        };
        let integration = NativeIntegrations::start(runtime.clone(), config)
            .await
            .unwrap();
        assert_eq!(
            runtime.get_state("input_button.doorbell").unwrap().state,
            "2021-01-01T23:59:59+00:00"
        );
        integration.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn input_datetime_set_service_validates_and_preserves_other_component() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = NativeConfig {
            time_zone: "America/New_York".into(),
            input_datetime: BTreeMap::from([(
                "alarm".into(),
                InputDatetimeConfig {
                    name: Some("Alarm".into()),
                    has_date: true,
                    has_time: true,
                    initial: Some("2024-01-01 08:15:30".into()),
                    ..InputDatetimeConfig::default()
                },
            )]),
            ..NativeConfig::default()
        };
        let integration = NativeIntegrations::start(runtime.clone(), config)
            .await
            .unwrap();
        runtime
            .call_service(service_call(
                "input_datetime",
                "set_datetime",
                json!({"entity_id":"input_datetime.alarm"}),
                json!({"date":"2024-02-29"}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_datetime.alarm").unwrap().state,
            "2024-02-29 08:15:30"
        );
        assert_eq!(
            runtime
                .get_state("input_datetime.alarm")
                .unwrap()
                .attributes["has_date"],
            true
        );
        runtime
            .call_service(service_call(
                "input_datetime",
                "set_datetime",
                json!({"entity_id":"input_datetime.alarm"}),
                json!({"datetime":"2024-03-01T10:11:12-05:00"}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_datetime.alarm").unwrap().state,
            "2024-03-01 10:11:12"
        );
        runtime
            .call_service(service_call(
                "input_datetime",
                "set_datetime",
                json!({"entity_id":"input_datetime.alarm"}),
                json!({"timestamp":0}),
            ))
            .unwrap();
        let epoch_state = runtime.get_state("input_datetime.alarm").unwrap();
        assert_eq!(epoch_state.state, "1969-12-31 19:00:00");
        assert_eq!(epoch_state.attributes["timestamp"], 0);
        assert!(
            runtime
                .call_service(service_call(
                    "input_datetime",
                    "set_datetime",
                    json!({"entity_id":"input_datetime.alarm"}),
                    json!({"date":"2024-02-30"}),
                ))
                .is_err()
        );
        integration.shutdown().await.unwrap();
    }

    #[test]
    fn schedule_evaluates_local_time_and_follows_the_current_dst_fold() {
        let config = ScheduleConfig {
            name: "Workday".into(),
            monday: vec![ScheduleTimeRange {
                from: "09:00".into(),
                to: "10:00".into(),
                data: BTreeMap::from([("mode".into(), json!("work"))]),
            }],
            sunday: vec![ScheduleTimeRange {
                from: "01:30".into(),
                to: "02:30".into(),
                data: BTreeMap::new(),
            }],
            ..ScheduleConfig::default()
        };
        let parsed = ParsedSchedule::from_config("workday", &config).unwrap();
        let timezone: Tz = "America/New_York".parse().unwrap();
        let monday = Utc.with_ymd_and_hms(2024, 1, 1, 14, 30, 0).unwrap();
        let (entity_id, state, attributes, next) = schedule_state(&parsed, monday, timezone);
        assert_eq!(entity_id, "schedule.workday");
        assert_eq!(state, "on");
        assert_eq!(attributes["mode"], "work");
        assert_eq!(
            next.unwrap().with_timezone(&Utc),
            Utc.with_ymd_and_hms(2024, 1, 1, 15, 0, 0).unwrap()
        );

        let second_fold = Utc.with_ymd_and_hms(2024, 11, 3, 6, 0, 0).unwrap();
        let event =
            next_schedule_event(&parsed, second_fold.with_timezone(&timezone), timezone).unwrap();
        assert_eq!(
            event.with_timezone(&Utc),
            Utc.with_ymd_and_hms(2024, 11, 3, 6, 30, 0).unwrap()
        );
    }

    #[test]
    fn schedule_get_response_contains_only_weekdays_and_serializes_end_of_day() {
        let config = ScheduleConfig {
            name: "All day".into(),
            icon: Some("mdi:calendar".into()),
            sunday: vec![ScheduleTimeRange {
                from: "00:00".into(),
                to: "24:00".into(),
                data: BTreeMap::from([("label".into(), json!("open"))]),
            }],
            ..ScheduleConfig::default()
        };
        let parsed = ParsedSchedule::from_config("all_day", &config).unwrap();
        let value = schedule_service_value(&parsed);
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 7);
        assert!(object.get("name").is_none());
        assert!(object.get("icon").is_none());
        assert_eq!(object["sunday"][0]["to"], "23:59:59.999999");
        assert_eq!(object["sunday"][0]["data"]["label"], "open");
    }

    #[test]
    fn native_config_rejects_invalid_dates_timezones_and_overlapping_slots() {
        let bad_date = NativeConfig {
            input_datetime: BTreeMap::from([(
                "invalid".into(),
                InputDatetimeConfig {
                    has_date: true,
                    initial: Some("2024-02-30".into()),
                    ..InputDatetimeConfig::default()
                },
            )]),
            ..NativeConfig::default()
        };
        assert!(bad_date.validate().is_err());
        assert!(
            NativeConfig {
                time_zone: "Mars/Olympus".into(),
                ..NativeConfig::default()
            }
            .validate()
            .is_err()
        );
        let overlapping = NativeConfig {
            schedule: BTreeMap::from([(
                "bad".into(),
                ScheduleConfig {
                    name: "Bad".into(),
                    monday: vec![
                        ScheduleTimeRange {
                            from: "10:00".into(),
                            to: "12:00".into(),
                            data: BTreeMap::new(),
                        },
                        ScheduleTimeRange {
                            from: "11:00".into(),
                            to: "13:00".into(),
                            data: BTreeMap::new(),
                        },
                    ],
                    ..ScheduleConfig::default()
                },
            )]),
            ..NativeConfig::default()
        };
        assert!(overlapping.validate().is_err());
    }

    #[tokio::test]
    async fn start_validates_every_helper_before_runtime_mutation() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = NativeConfig {
            timer: BTreeMap::from([("would_be_created".into(), TimerConfig::default())]),
            schedule: BTreeMap::from([(
                "invalid".into(),
                ScheduleConfig {
                    name: "Invalid schedule".into(),
                    monday: vec![ScheduleTimeRange {
                        from: "12:00".into(),
                        to: "11:00".into(),
                        data: BTreeMap::new(),
                    }],
                    ..ScheduleConfig::default()
                },
            )]),
            ..NativeConfig::default()
        };
        assert!(
            NativeIntegrations::start(runtime.clone(), config)
                .await
                .is_err()
        );
        assert!(runtime.states().is_empty());
        assert!(runtime.services().as_object().unwrap().is_empty());
        assert!(store.events().unwrap().is_empty());
    }

    #[test]
    fn native_config_deserializes_pinned_flat_helper_syntax_strictly() {
        let config: NativeConfig = serde_yaml::from_str(
            "timer:\n  kettle:\n    duration: '01:30'\n    restore: true\ninput_button:\n  doorbell:\ninput_datetime:\n  alarm:\n    has_date: true\n    has_time: true\n    initial: '2024-01-01 08:00:00'\nschedule:\n  workday:\n    name: Workday\n    monday:\n      - from: '09:00'\n        to: '17:00'\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.timer["kettle"].duration.millis(false).unwrap(),
            5_400_000
        );
        assert!(config.input_button.contains_key("doorbell"));
        assert!(
            serde_yaml::from_str::<NativeConfig>(
                "input_button:\n  doorbell:\n    unsupported: true\n"
            )
            .is_err()
        );
    }

    struct InjectedStore {
        inner: MemoryStore,
        fail_metadata_reads: AtomicBool,
        fail_metadata_writes: AtomicBool,
    }

    impl Default for InjectedStore {
        fn default() -> Self {
            Self {
                inner: MemoryStore::default(),
                fail_metadata_reads: AtomicBool::new(false),
                fail_metadata_writes: AtomicBool::new(false),
            }
        }
    }

    impl Store for InjectedStore {
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
            if self.fail_metadata_reads.load(Ordering::Acquire) {
                return Err(Error::Storage("injected metadata read failure".into()));
            }
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
            if self.fail_metadata_writes.load(Ordering::Acquire) {
                return Err(Error::Storage("injected metadata write failure".into()));
            }
            self.inner.set_data(key, value)
        }

        fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
            self.inner.delete_data(key)
        }

        fn repack(&self) -> hass_core::Result<()> {
            self.inner.repack()
        }

        fn history_query(
            &self,
            query: &hass_core::HistoryQuery,
        ) -> hass_core::Result<Vec<hass_core::HistorySeries>> {
            self.inner.history_query(query)
        }
        fn event_history_query(
            &self,
            query: &hass_core::EventHistoryQuery,
        ) -> hass_core::Result<Vec<hass_core::Event>> {
            self.inner.event_history_query(query)
        }
        fn purge_before(
            &self,
            cutoff: chrono::DateTime<chrono::Utc>,
        ) -> hass_core::Result<hass_core::PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn history(
            &self,
            entity_id: &str,
            start: DateTime<Utc>,
            end: DateTime<Utc>,
        ) -> hass_core::Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }
    }

    #[tokio::test]
    async fn metadata_read_failure_does_not_mutate_runtime_or_register_services() {
        let store = Arc::new(InjectedStore::default());
        store.fail_metadata_reads.store(true, Ordering::Release);
        let runtime = Runtime::new(store).unwrap();
        assert!(
            NativeIntegrations::start(runtime.clone(), timer_config(5.0, true))
                .await
                .is_err()
        );
        assert!(runtime.get_state("timer.kettle").is_none());
        assert!(runtime.services().get("timer").is_none());
    }

    #[tokio::test]
    async fn metadata_write_failure_keeps_timer_state_unchanged() {
        let store = Arc::new(InjectedStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = NativeIntegrations::start(runtime.clone(), timer_config(5.0, true))
            .await
            .unwrap();
        store.fail_metadata_writes.store(true, Ordering::Release);
        assert!(
            runtime
                .call_service(service_call(
                    "timer",
                    "start",
                    json!({"entity_id":"timer.kettle"}),
                    json!({}),
                ))
                .is_err()
        );
        assert_eq!(runtime.get_state("timer.kettle").unwrap().state, STOPPED);
        integration.shutdown().await.unwrap();
    }
}
