//! Native time/date and uptime sensors pinned to Home Assistant's
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` behavior.
//!
//! This crate owns deterministic validation, formatting, scheduling, and
//! metadata. The app adapter supplies the shared entity lifecycle and runtime
//! configuration; it must publish these snapshots through that lifecycle
//! rather than allocating entity IDs here.

use chrono::{
    DateTime, Duration, NaiveDateTime, NaiveTime, Offset, SecondsFormat, TimeZone, Timelike, Utc,
};
use chrono_tz::{GapInfo, Tz};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use thiserror::Error;

pub const SENSOR_DOMAIN: &str = "sensor";
pub const TIME_DATE_DOMAIN: &str = "time_date";
pub const UPTIME_DOMAIN: &str = "uptime";
pub const UPTIME_ENTRY_TITLE: &str = "Uptime";
pub const UPTIME_DEVICE_CLASS: &str = "uptime";
pub const UPTIME_DEVICE_ENTRY_TYPE: &str = "service";
pub const UPTIME_SINGLE_CONFIG_ENTRY: bool = true;

/// The seven display options accepted by the pinned integration.
///
/// The upstream strings file still has a stale `beat` translation, but
/// `OPTION_TYPES` excludes it and both the YAML schema and user flow reject
/// it. It is intentionally not represented here.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayOption {
    Time,
    Date,
    DateTime,
    DateTimeUtc,
    DateTimeIso,
    TimeDate,
    TimeUtc,
}

impl DisplayOption {
    pub const ALL: [Self; 7] = [
        Self::Time,
        Self::Date,
        Self::DateTime,
        Self::DateTimeUtc,
        Self::DateTimeIso,
        Self::TimeDate,
        Self::TimeUtc,
    ];

    /// Return the upstream option key.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Date => "date",
            Self::DateTime => "date_time",
            Self::DateTimeUtc => "date_time_utc",
            Self::DateTimeIso => "date_time_iso",
            Self::TimeDate => "time_date",
            Self::TimeUtc => "time_utc",
        }
    }

    /// Return the English translation used by Home Assistant's state preview.
    pub const fn friendly_name(self) -> &'static str {
        match self {
            Self::Time => "Time",
            Self::Date => "Date",
            Self::DateTime => "Date & Time",
            Self::DateTimeUtc => "Date & Time (UTC)",
            Self::DateTimeIso => "Date & Time (ISO)",
            Self::TimeDate => "Time & Date",
            Self::TimeUtc => "Time (UTC)",
        }
    }

    /// Return the icon exposed by the upstream sensor.
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Date => "mdi:calendar",
            Self::DateTime | Self::DateTimeUtc | Self::DateTimeIso | Self::TimeDate => {
                "mdi:calendar-clock"
            }
            Self::Time | Self::TimeUtc => "mdi:clock",
        }
    }

    const fn updates_at_local_midnight(self) -> bool {
        matches!(self, Self::Date)
    }
}

impl TryFrom<&str> for DisplayOption {
    type Error = ConfigError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|option| option.key() == value)
            .ok_or_else(|| ConfigError::UnsupportedDisplayOption(value.to_owned()))
    }
}

/// Validation and timezone errors returned to config-flow and provider code.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConfigError {
    #[error("configuration must be an object")]
    ExpectedObject,
    #[error("unknown configuration field: {0}")]
    UnknownField(String),
    #[error("display_options is required in the user flow")]
    MissingDisplayOption,
    #[error("display_options must be a string or a list of strings")]
    InvalidDisplayOptionsShape,
    #[error("display_options entries must be strings")]
    InvalidDisplayOptionType,
    #[error("entity_namespace must be a string")]
    InvalidEntityNamespace,
    #[error("scan_interval is not a valid Home Assistant time period")]
    InvalidScanInterval,
    #[error("unsupported time/date display option: {0}")]
    UnsupportedDisplayOption(String),
    #[error("Home Assistant timezone is not configured")]
    TimezoneNotConfigured,
    #[error("unknown IANA timezone: {0}")]
    InvalidTimezone(String),
    #[error("uptime user flow accepts an empty object")]
    UptimeFlowMustBeEmpty,
    #[error("the local date is outside the supported calendar range")]
    DateOutOfRange,
    #[error("could not resolve local midnight for timezone {0}")]
    LocalMidnightUnresolvable(String),
}

/// Strict integration-specific subset of the YAML sensor platform config.
///
/// The surrounding YAML loader may handle the shared sensor platform fields;
/// this type rejects unknown `time_date`-specific keys. A missing option uses
/// the upstream default `["time"]`. A scalar option is normalized to a list,
/// matching `EnsureList` in the pinned source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeDateYamlConfig {
    pub display_options: Vec<DisplayOption>,
    /// Shared sensor-platform namespace used to prefix generated entity IDs.
    pub entity_namespace: Option<String>,
    /// Accepted by the shared platform schema but unused because time/date
    /// entities schedule their own minute or local-midnight updates.
    pub scan_interval: Option<Value>,
}

impl Default for TimeDateYamlConfig {
    fn default() -> Self {
        Self {
            display_options: vec![DisplayOption::Time],
            entity_namespace: None,
            scan_interval: None,
        }
    }
}

impl TimeDateYamlConfig {
    /// Parse a `serde_json::Value` representation of one YAML platform block.
    pub fn parse(value: &Value) -> Result<Self, ConfigError> {
        let object = value.as_object().ok_or(ConfigError::ExpectedObject)?;
        for key in object.keys() {
            if key != "display_options" && key != "entity_namespace" && key != "scan_interval" {
                return Err(ConfigError::UnknownField(key.clone()));
            }
        }
        let entity_namespace = object
            .get("entity_namespace")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(ConfigError::InvalidEntityNamespace)
            })
            .transpose()?;
        let scan_interval = object.get("scan_interval").cloned();
        if let Some(value) = scan_interval.as_ref() {
            validate_scan_interval(value)?;
        }
        let Some(raw) = object.get("display_options") else {
            return Ok(Self {
                entity_namespace,
                scan_interval,
                ..Self::default()
            });
        };
        let raw_options: Vec<&Value> = match raw {
            Value::String(_) => vec![raw],
            Value::Array(values) => values.iter().collect(),
            _ => return Err(ConfigError::InvalidDisplayOptionsShape),
        };
        let display_options = raw_options
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or(ConfigError::InvalidDisplayOptionType)
                    .and_then(DisplayOption::try_from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            display_options,
            entity_namespace,
            scan_interval,
        })
    }

    /// Produce one sensor descriptor per configured option, preserving source
    /// order and duplicates. The lifecycle owns final IDs and uniqueness.
    pub fn sensors(&self, config_entry_id: Option<&str>) -> Vec<TimeDateSensor> {
        self.display_options
            .iter()
            .copied()
            .map(|option| {
                TimeDateSensor::new_with_namespace(
                    option,
                    config_entry_id,
                    self.entity_namespace.as_deref(),
                )
            })
            .collect()
    }
}

/// One validated option from the single-select config flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeDateFlowConfig {
    pub display_option: DisplayOption,
}

impl TimeDateFlowConfig {
    /// Validate the pinned flow's one-string selector and required timezone.
    ///
    /// The upstream selector rejects `beat`; timezone absence is surfaced as
    /// its `timezone_not_exist` form error by the app adapter.
    pub fn parse(value: &Value, time_zone: Option<&str>) -> Result<Self, ConfigError> {
        let object = value.as_object().ok_or(ConfigError::ExpectedObject)?;
        for key in object.keys() {
            if key != "display_options" {
                return Err(ConfigError::UnknownField(key.clone()));
            }
        }
        let raw = object
            .get("display_options")
            .ok_or(ConfigError::MissingDisplayOption)?
            .as_str()
            .ok_or(ConfigError::InvalidDisplayOptionsShape)?;
        let display_option = DisplayOption::try_from(raw)?;
        parse_time_zone(time_zone.ok_or(ConfigError::TimezoneNotConfigured)?)?;
        Ok(Self { display_option })
    }

    /// Return the exact title pattern used by the config flow.
    pub fn entry_title(self) -> String {
        format!("Time & Date {}", self.display_option.key())
    }

    /// The config flow stores the selected display option in entry options.
    pub fn entry_data(self) -> Map<String, Value> {
        Map::new()
    }

    pub fn entry_options(self) -> Map<String, Value> {
        Map::from_iter([(
            "display_options".to_owned(),
            Value::String(self.display_option.key().to_owned()),
        )])
    }
}

/// Validate the no-input `uptime` config flow.
pub fn validate_uptime_user_input(value: &Value) -> Result<(), ConfigError> {
    let object = value.as_object().ok_or(ConfigError::ExpectedObject)?;
    if object.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::UptimeFlowMustBeEmpty)
    }
}

/// Parse a configured Home Assistant IANA timezone.
pub fn parse_time_zone(time_zone: &str) -> Result<Tz, ConfigError> {
    Tz::from_str(time_zone).map_err(|_| ConfigError::InvalidTimezone(time_zone.to_owned()))
}

fn validate_scan_interval(value: &Value) -> Result<(), ConfigError> {
    const MAX_TIME_PERIOD_SECONDS: f64 = 86_400.0 * 999_999_999.0 + 86_399.999_999;

    let total_seconds = match value {
        Value::String(value) => parse_scan_interval_string(value)?,
        Value::Number(value) => value.as_f64().ok_or(ConfigError::InvalidScanInterval)?,
        Value::Bool(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Value::Object(values) => {
            if values.is_empty()
                || values.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "days" | "hours" | "minutes" | "seconds" | "milliseconds"
                    )
                })
            {
                return Err(ConfigError::InvalidScanInterval);
            }
            values.iter().try_fold(0.0, |total, (key, value)| {
                let value = match value {
                    Value::String(value) => value
                        .parse::<f64>()
                        .map_err(|_| ConfigError::InvalidScanInterval)?,
                    Value::Number(value) => {
                        value.as_f64().ok_or(ConfigError::InvalidScanInterval)?
                    }
                    Value::Bool(value) => {
                        if *value {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    _ => return Err(ConfigError::InvalidScanInterval),
                };
                let multiplier = match key.as_str() {
                    "days" => 86_400.0,
                    "hours" => 3_600.0,
                    "minutes" => 60.0,
                    "seconds" => 1.0,
                    "milliseconds" => 0.001,
                    _ => unreachable!("keys were validated above"),
                };
                Ok(total + value * multiplier)
            })?
        }
        _ => return Err(ConfigError::InvalidScanInterval),
    };
    if !total_seconds.is_finite() || total_seconds.abs() > MAX_TIME_PERIOD_SECONDS {
        return Err(ConfigError::InvalidScanInterval);
    }
    Ok(())
}

fn parse_scan_interval_string(value: &str) -> Result<f64, ConfigError> {
    if value.contains(':') {
        let (negative, value) = if let Some(value) = value.strip_prefix('-') {
            (true, value)
        } else if let Some(value) = value.strip_prefix('+') {
            (false, value)
        } else {
            (false, value)
        };
        let parts = value.split(':').collect::<Vec<_>>();
        if !matches!(parts.len(), 2 | 3) {
            return Err(ConfigError::InvalidScanInterval);
        }
        let hours = parts[0]
            .parse::<i64>()
            .map_err(|_| ConfigError::InvalidScanInterval)?;
        let minutes = parts[1]
            .parse::<i64>()
            .map_err(|_| ConfigError::InvalidScanInterval)?;
        let seconds = parts
            .get(2)
            .map(|value| {
                value
                    .parse::<f64>()
                    .map_err(|_| ConfigError::InvalidScanInterval)
            })
            .transpose()?
            .unwrap_or(0.0);
        let result = hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds;
        Ok(if negative { -result } else { result })
    } else {
        value
            .parse::<f64>()
            .map_err(|_| ConfigError::InvalidScanInterval)
    }
}

/// Format the pinned `time_date` state at a supplied UTC instant.
///
/// Passing `now` explicitly keeps the calculation deterministic in tests;
/// production adapters should call this with `Utc::now()` and the current
/// `Runtime::time_zone()` value.
pub fn format_time_date_state(
    option: DisplayOption,
    now: DateTime<Utc>,
    time_zone: &str,
) -> Result<String, ConfigError> {
    let zone = parse_time_zone(time_zone)?;
    let local = now.with_timezone(&zone);
    let local_date = local.format("%Y-%m-%d").to_string();
    let local_time = local.format("%H:%M").to_string();
    let utc_date = now.format("%Y-%m-%d").to_string();
    let utc_time = now.format("%H:%M").to_string();
    Ok(match option {
        DisplayOption::Time => local_time,
        DisplayOption::Date => local_date,
        DisplayOption::DateTime => format!("{local_date}, {local_time}"),
        DisplayOption::DateTimeUtc => format!("{utc_date}, {utc_time}"),
        DisplayOption::DateTimeIso => format!("{local_date}T{local_time}:00"),
        DisplayOption::TimeDate => format!("{local_time}, {local_date}"),
        DisplayOption::TimeUtc => utc_time,
    })
}

/// Compute the next pinned update boundary for one `time_date` option.
///
/// `date` follows the next local midnight, including DST transitions. All
/// other options update on the next UTC minute boundary, even when `now` is
/// already exactly on a boundary (then the next update is one minute later).
pub fn next_time_date_update(
    option: DisplayOption,
    now: DateTime<Utc>,
    time_zone: &str,
) -> Result<DateTime<Utc>, ConfigError> {
    let zone = parse_time_zone(time_zone)?;
    if option.updates_at_local_midnight() {
        next_local_midnight(now, zone)
    } else {
        next_utc_minute(now)
    }
}

fn next_utc_minute(now: DateTime<Utc>) -> Result<DateTime<Utc>, ConfigError> {
    let nanos_into_minute = i128::from(now.timestamp().rem_euclid(60)) * 1_000_000_000
        + i128::from(now.timestamp_subsec_nanos());
    let nanos_to_boundary = 60_i128 * 1_000_000_000 - nanos_into_minute;
    let nanos_to_boundary =
        i64::try_from(nanos_to_boundary).map_err(|_| ConfigError::DateOutOfRange)?;
    now.checked_add_signed(Duration::nanoseconds(nanos_to_boundary))
        .ok_or(ConfigError::DateOutOfRange)
}

fn next_local_midnight(now: DateTime<Utc>, zone: Tz) -> Result<DateTime<Utc>, ConfigError> {
    let local_date = now.with_timezone(&zone).date_naive();
    let next_date = local_date.succ_opt().ok_or(ConfigError::DateOutOfRange)?;
    let midnight: NaiveDateTime = next_date.and_time(NaiveTime::MIN);
    match zone.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(value) => Ok(value.with_timezone(&Utc)),
        // Python's datetime.combine(..., tzinfo=ZoneInfo) uses fold=0.
        chrono::LocalResult::Ambiguous(earliest, _latest) => Ok(earliest.with_timezone(&Utc)),
        chrono::LocalResult::None => {
            // ZoneInfo attaches the pre-gap offset to a nonexistent local
            // midnight. Reproduce that mapping instead of panicking or
            // silently using a fixed 24-hour day.
            let gap = GapInfo::new(&midnight, &zone)
                .ok_or_else(|| ConfigError::LocalMidnightUnresolvable(zone.to_string()))?;
            if let Some((_, before_gap)) = gap.begin {
                let offset_seconds = before_gap.fix().local_minus_utc();
                let timestamp = midnight
                    .and_utc()
                    .timestamp()
                    .checked_sub(i64::from(offset_seconds))
                    .ok_or(ConfigError::DateOutOfRange)?;
                DateTime::from_timestamp(timestamp, 0).ok_or(ConfigError::DateOutOfRange)
            } else if let Some(after_gap) = gap.end {
                Ok(after_gap.with_timezone(&Utc))
            } else {
                Err(ConfigError::LocalMidnightUnresolvable(zone.to_string()))
            }
        }
    }
}

/// Timestamp state with Python `datetime.isoformat()` precision and UTC offset.
pub fn format_utc_timestamp(value: DateTime<Utc>) -> String {
    let microsecond_value = value
        .with_nanosecond(value.timestamp_subsec_micros() * 1_000)
        .unwrap_or(value);
    microsecond_value.to_rfc3339_opts(SecondsFormat::AutoSi, false)
}

/// Metadata needed by the shared entity lifecycle for one time/date sensor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeDateSensorMetadata {
    pub domain: &'static str,
    pub platform: &'static str,
    pub config_entry_id: Option<String>,
    pub unique_id: Option<String>,
    pub suggested_object_id: String,
    pub translation_key: &'static str,
    pub has_entity_name: bool,
    pub should_poll: bool,
    pub device_class: Option<&'static str>,
    pub state_class: Option<&'static str>,
    pub unit_of_measurement: Option<&'static str>,
    pub icon: &'static str,
    pub entity_category: Option<&'static str>,
}

/// Point-in-time state and attributes for one time/date entity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SensorSnapshot {
    pub state: String,
    pub attributes: Map<String, Value>,
}

/// One configured `time_date` sensor; the shared lifecycle supplies its ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeDateSensor {
    option: DisplayOption,
    config_entry_id: Option<String>,
    entity_namespace: Option<String>,
}

impl TimeDateSensor {
    pub fn new(option: DisplayOption, config_entry_id: Option<&str>) -> Self {
        Self::new_with_namespace(option, config_entry_id, None)
    }

    pub fn new_with_namespace(
        option: DisplayOption,
        config_entry_id: Option<&str>,
        entity_namespace: Option<&str>,
    ) -> Self {
        Self {
            option,
            config_entry_id: config_entry_id.map(str::to_owned),
            entity_namespace: entity_namespace.map(str::to_owned),
        }
    }

    fn provider_key(&self) -> String {
        match self.entity_namespace.as_deref() {
            Some(namespace) => format!("{namespace}:{}", self.option.key()),
            None => self.option.key().to_owned(),
        }
    }

    pub const fn option(&self) -> DisplayOption {
        self.option
    }

    pub fn metadata(&self) -> TimeDateSensorMetadata {
        TimeDateSensorMetadata {
            domain: SENSOR_DOMAIN,
            platform: TIME_DATE_DOMAIN,
            config_entry_id: self.config_entry_id.clone(),
            unique_id: self
                .config_entry_id
                .as_ref()
                .map(|_| self.option.key().to_owned()),
            suggested_object_id: self
                .entity_namespace
                .as_ref()
                .map(|namespace| format!("{namespace} {}", self.option.key()))
                .unwrap_or_else(|| self.option.key().to_owned()),
            translation_key: self.option.key(),
            has_entity_name: true,
            should_poll: false,
            device_class: None,
            state_class: None,
            unit_of_measurement: None,
            icon: self.option.icon(),
            entity_category: None,
        }
    }

    /// Read real UTC time for an ordinary provider update.
    pub fn snapshot(&self, time_zone: &str) -> Result<SensorSnapshot, ConfigError> {
        self.snapshot_at(Utc::now(), time_zone)
    }

    /// Calculate a deterministic state and source attributes at `now`.
    pub fn snapshot_at(
        &self,
        now: DateTime<Utc>,
        time_zone: &str,
    ) -> Result<SensorSnapshot, ConfigError> {
        let state = format_time_date_state(self.option, now, time_zone)?;
        let attributes = time_date_attributes(self.option);
        Ok(SensorSnapshot { state, attributes })
    }
}

/// Device metadata for the upstream `uptime` config-entry sensor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UptimeDeviceMetadata {
    pub name: String,
    pub identifiers: [(String, String); 1],
    pub entry_type: &'static str,
}

/// Metadata needed by the shared entity lifecycle for `uptime`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UptimeSensorMetadata {
    pub domain: &'static str,
    pub platform: &'static str,
    pub config_entry_id: String,
    pub unique_id: String,
    pub suggested_object_id: &'static str,
    pub has_entity_name: bool,
    pub name: Option<&'static str>,
    pub translation_key: Option<&'static str>,
    pub should_poll: bool,
    pub device_class: &'static str,
    pub state_class: Option<&'static str>,
    pub unit_of_measurement: Option<&'static str>,
    pub icon: Option<&'static str>,
    pub entity_category: Option<&'static str>,
    pub device: UptimeDeviceMetadata,
}

/// Metadata and fixed setup-time state for one uptime sensor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UptimeSensor {
    entry_id: String,
    entry_title: String,
    setup_time: DateTime<Utc>,
}

impl UptimeSensor {
    /// Construct using the real UTC instant at provider setup.
    pub fn new(entry_id: impl Into<String>, entry_title: impl Into<String>) -> Self {
        Self::at(entry_id, entry_title, Utc::now())
    }

    /// Construct with a known setup instant for deterministic tests or a host
    /// that captures one shared setup timestamp before registering entities.
    pub fn at(
        entry_id: impl Into<String>,
        entry_title: impl Into<String>,
        setup_time: DateTime<Utc>,
    ) -> Self {
        Self {
            entry_id: entry_id.into(),
            entry_title: entry_title.into(),
            setup_time,
        }
    }

    pub fn metadata(&self) -> UptimeSensorMetadata {
        UptimeSensorMetadata {
            domain: SENSOR_DOMAIN,
            platform: UPTIME_DOMAIN,
            config_entry_id: self.entry_id.clone(),
            unique_id: self.entry_id.clone(),
            suggested_object_id: "uptime",
            has_entity_name: true,
            name: None,
            translation_key: None,
            should_poll: false,
            device_class: UPTIME_DEVICE_CLASS,
            state_class: None,
            unit_of_measurement: None,
            icon: None,
            entity_category: None,
            device: UptimeDeviceMetadata {
                name: self.entry_title.clone(),
                identifiers: [(UPTIME_DOMAIN.to_owned(), self.entry_id.clone())],
                entry_type: UPTIME_DEVICE_ENTRY_TYPE,
            },
        }
    }

    /// The pinned sensor reports the timestamp captured at setup and does not
    /// poll or advance it afterward.
    pub fn snapshot(&self) -> SensorSnapshot {
        SensorSnapshot {
            state: format_utc_timestamp(self.setup_time),
            attributes: Map::from_iter([
                (
                    "device_class".to_owned(),
                    Value::String(UPTIME_DEVICE_CLASS.to_owned()),
                ),
                (
                    "friendly_name".to_owned(),
                    Value::String(UPTIME_ENTRY_TITLE.to_owned()),
                ),
            ]),
        }
    }

    pub fn setup_time(&self) -> DateTime<Utc> {
        self.setup_time
    }
}

/// The pinned uptime flow accepts no user data and creates a single entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UptimeFlowConfig;

impl UptimeFlowConfig {
    pub fn parse(value: &Value) -> Result<Self, ConfigError> {
        validate_uptime_user_input(value)?;
        Ok(Self)
    }

    pub const fn entry_title(self) -> &'static str {
        UPTIME_ENTRY_TITLE
    }

    pub fn entry_data(self) -> Map<String, Value> {
        Map::new()
    }

    pub fn entry_options(self) -> Map<String, Value> {
        Map::new()
    }
}

/// Construct the stable state attributes for `time_date` without applying
/// localization outside the integration's known English translations.
pub fn time_date_attributes(option: DisplayOption) -> Map<String, Value> {
    Map::from_iter([
        ("friendly_name".to_owned(), json!(option.friendly_name())),
        ("icon".to_owned(), json!(option.icon())),
    ])
}

#[cfg(feature = "runtime")]
mod runtime {
    use super::*;
    use hass_core::{Context, Error as CoreError, Event, Runtime};
    use hass_entity::{EntityHandle, EntityLifecycle, EntitySnapshot, EntitySpec};
    use hass_registry::ProviderEntityInfo;
    use std::collections::BTreeMap;
    use std::time::Duration as StdDuration;
    use tokio::sync::{broadcast, watch};
    use tokio::task::JoinHandle;

    /// Errors raised while registering or running a native system sensor.
    #[derive(Debug, Error)]
    pub enum ProviderError {
        #[error(transparent)]
        Config(#[from] ConfigError),
        #[error(transparent)]
        Runtime(#[from] CoreError),
        #[error("joining time/date update task failed: {0}")]
        Join(#[from] tokio::task::JoinError),
    }

    /// Running `time_date` provider backed by the shared entity lifecycle.
    ///
    /// The provider owns only its leases and update task. Entity IDs and registry
    /// cache remain inside `EntityLifecycle` and `EntityHandle`.
    pub struct TimeDateIntegration {
        sensors: Vec<TimeDateSensor>,
        handles: Vec<EntityHandle>,
        lifecycle: EntityLifecycle,
        provider_id: String,
        cancel: watch::Sender<bool>,
        task: Option<JoinHandle<Result<(), ProviderError>>>,
    }

    impl TimeDateIntegration {
        /// Register the configured sensors, publish their first real clock values,
        /// and start the minute/midnight scheduler.
        ///
        /// `provider_id` must be stable for this YAML platform instance or config
        /// entry. Entry-backed sensors carry the upstream option-key unique ID;
        /// YAML sensors have no registry unique ID in the pinned source.
        pub fn start(
            runtime: Runtime,
            lifecycle: &EntityLifecycle,
            config: TimeDateYamlConfig,
            provider_id: impl Into<String>,
            config_entry_id: Option<&str>,
        ) -> Result<Self, ProviderError> {
            let sensors = config.sensors(config_entry_id);
            let provider_id = match config_entry_id {
                Some(entry_id) => format!("{TIME_DATE_DOMAIN}:{entry_id}"),
                None => provider_id.into(),
            };
            Self::start_with_sensors(runtime, lifecycle, sensors, provider_id)
        }

        /// Register an already flattened YAML sensor set under one provider
        /// namespace. This preserves per-platform entity namespaces while
        /// allowing one reconciliation to release stale YAML sensors.
        pub fn start_with_sensors(
            runtime: Runtime,
            lifecycle: &EntityLifecycle,
            sensors: Vec<TimeDateSensor>,
            provider_id: impl Into<String>,
        ) -> Result<Self, ProviderError> {
            let zone_name = runtime.time_zone();
            parse_time_zone(&zone_name)?;
            let spawn_handle = if sensors.is_empty() {
                None
            } else {
                Some(tokio::runtime::Handle::try_current().map_err(|error| {
                    ProviderError::Runtime(CoreError::Service(format!(
                        "time/date sensors require a Tokio runtime: {error}"
                    )))
                })?)
            };
            let provider_id = provider_id.into();
            let specs = time_date_entity_specs(&sensors, &provider_id);
            // Subscribe before entity registration so a concurrent core config
            // update cannot be missed between the initial state and task launch.
            let events = runtime.subscribe();
            let handles = lifecycle.register_provider_batch(&provider_id, specs)?;
            let now = Utc::now();
            for (sensor, handle) in sensors.iter().zip(&handles) {
                if let Err(error) = publish_time_date(sensor, handle, now, &zone_name) {
                    let _ = lifecycle.register_provider_batch(&provider_id, Vec::new());
                    return Err(error);
                }
            }
            let (cancel, cancel_rx) = watch::channel(false);
            let task = if let Some(spawn_handle) = spawn_handle {
                let worker_runtime = runtime.clone();
                let worker_sensors = sensors.clone();
                let worker_handles = handles.clone();
                Some(spawn_handle.spawn(async move {
                    time_date_update_loop(
                        worker_runtime,
                        worker_sensors,
                        worker_handles,
                        events,
                        cancel_rx,
                    )
                    .await
                }))
            } else {
                None
            };
            Ok(Self {
                sensors,
                handles,
                lifecycle: lifecycle.clone(),
                provider_id,
                cancel,
                task,
            })
        }

        /// Read each current entity ID from its lifecycle handle.
        pub fn entity_ids(&self) -> Vec<Option<String>> {
            self.handles
                .iter()
                .map(EntityHandle::current_entity_id)
                .collect()
        }

        /// Return the configured sensors in the same order as their handles.
        pub fn sensors(&self) -> &[TimeDateSensor] {
            &self.sensors
        }

        /// Stop and join the update task, then release each entity lease.
        pub async fn shutdown(mut self) -> Result<(), ProviderError> {
            self.cancel.send_replace(true);
            let task_result = if let Some(task) = self.task.take() {
                task.await?
            } else {
                Ok(())
            };
            let release_result = self
                .lifecycle
                .register_provider_batch(&self.provider_id, Vec::new())
                .map(|_| ())
                .map_err(ProviderError::from);
            task_result?;
            release_result?;
            Ok(())
        }
    }

    impl Drop for TimeDateIntegration {
        fn drop(&mut self) {
            self.cancel.send_replace(true);
            if let Some(task) = self.task.take() {
                task.abort();
            }
            let _ = self
                .lifecycle
                .register_provider_batch(&self.provider_id, Vec::new());
        }
    }

    /// A registered, fixed-at-setup `uptime` sensor.
    pub struct UptimeIntegration {
        sensor: UptimeSensor,
        lifecycle: EntityLifecycle,
        provider_id: String,
        handle: Option<EntityHandle>,
    }

    impl UptimeIntegration {
        /// Register one uptime entry, publish its setup timestamp, and attach it to
        /// the service device created by the app's device-registry adapter.
        pub fn start(
            lifecycle: &EntityLifecycle,
            sensor: UptimeSensor,
            device_id: &str,
        ) -> Result<Self, ProviderError> {
            let metadata = sensor.metadata();
            let info = ProviderEntityInfo {
                original_device_class: Some(metadata.device_class.to_owned()),
                suggested_object_id: Some(metadata.suggested_object_id.to_owned()),
                has_entity_name: metadata.has_entity_name,
                unit_of_measurement: metadata.unit_of_measurement.map(str::to_owned),
                ..ProviderEntityInfo::default()
            };
            let spec = EntitySpec::new(
                metadata.domain,
                metadata.platform,
                format!("{UPTIME_DOMAIN}:{}", metadata.config_entry_id),
                metadata.unique_id.clone(),
            )
            .with_unique_id(metadata.unique_id.clone())
            .with_config_entry_id(metadata.config_entry_id.clone())
            .with_device_id(device_id.to_owned())
            .with_suggested_object_id(metadata.suggested_object_id)
            .with_info(info);
            let provider_id = format!("{UPTIME_DOMAIN}:{}", metadata.config_entry_id);
            let mut handles = lifecycle.register_provider_batch(&provider_id, vec![spec])?;
            let handle = handles.pop().expect("one uptime entity was registered");
            if let Err(error) = publish_uptime(&sensor, &handle) {
                let _ = lifecycle.register_provider_batch(&provider_id, Vec::new());
                return Err(error);
            }
            Ok(Self {
                sensor,
                lifecycle: lifecycle.clone(),
                provider_id,
                handle: Some(handle),
            })
        }

        /// Construct and register the sensor using actual setup-time UTC.
        pub fn start_now(
            lifecycle: &EntityLifecycle,
            entry_id: impl Into<String>,
            entry_title: impl Into<String>,
            device_id: &str,
        ) -> Result<Self, ProviderError> {
            Self::start(
                lifecycle,
                UptimeSensor::new(entry_id, entry_title),
                device_id,
            )
        }

        pub fn sensor(&self) -> &UptimeSensor {
            &self.sensor
        }

        pub fn entity_id(&self) -> Option<String> {
            self.handle
                .as_ref()
                .and_then(EntityHandle::current_entity_id)
        }

        /// Release the registered entity lease.
        pub fn shutdown(mut self) -> Result<(), ProviderError> {
            self.handle.take();
            self.lifecycle
                .register_provider_batch(&self.provider_id, Vec::new())?;
            Ok(())
        }
    }

    impl Drop for UptimeIntegration {
        fn drop(&mut self) {
            self.handle.take();
            let _ = self
                .lifecycle
                .register_provider_batch(&self.provider_id, Vec::new());
        }
    }

    fn time_date_entity_specs(sensors: &[TimeDateSensor], provider_id: &str) -> Vec<EntitySpec> {
        let mut occurrences = BTreeMap::<String, usize>::new();
        sensors
            .iter()
            .map(|sensor| {
                let metadata = sensor.metadata();
                let identity = sensor.provider_key();
                let occurrence = occurrences.entry(identity.clone()).or_default();
                *occurrence += 1;
                let provider_key = if *occurrence == 1 {
                    identity
                } else {
                    format!("{identity}:{occurrence}")
                };
                let info = ProviderEntityInfo {
                    original_icon: Some(metadata.icon.to_owned()),
                    suggested_object_id: Some(metadata.suggested_object_id.to_owned()),
                    has_entity_name: metadata.has_entity_name,
                    translation_key: Some(metadata.translation_key.to_owned()),
                    ..ProviderEntityInfo::default()
                };
                let mut spec = EntitySpec::new(
                    metadata.domain,
                    metadata.platform,
                    provider_id,
                    provider_key,
                )
                .with_suggested_object_id(metadata.suggested_object_id)
                .with_info(info);
                if let Some(unique_id) = metadata.unique_id {
                    spec = spec.with_unique_id(unique_id);
                }
                if let Some(entry_id) = metadata.config_entry_id {
                    spec = spec.with_config_entry_id(entry_id);
                }
                spec
            })
            .collect()
    }

    fn publish_time_date(
        sensor: &TimeDateSensor,
        handle: &EntityHandle,
        now: DateTime<Utc>,
        time_zone: &str,
    ) -> Result<(), ProviderError> {
        let snapshot = sensor.snapshot_at(now, time_zone)?;
        let _ = handle.publish(EntitySnapshot {
            state: Some(snapshot.state),
            available: true,
            attributes: snapshot.attributes,
            context: Context::default(),
        })?;
        Ok(())
    }

    fn publish_uptime(sensor: &UptimeSensor, handle: &EntityHandle) -> Result<(), ProviderError> {
        let snapshot = sensor.snapshot();
        let _ = handle.publish(EntitySnapshot {
            state: Some(snapshot.state),
            available: true,
            attributes: snapshot.attributes,
            context: Context::default(),
        })?;
        Ok(())
    }

    async fn time_date_update_loop(
        runtime: Runtime,
        sensors: Vec<TimeDateSensor>,
        handles: Vec<EntityHandle>,
        mut events: broadcast::Receiver<Event>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), ProviderError> {
        let now = Utc::now();
        let mut zone = runtime.time_zone();
        let mut deadlines = calculate_deadlines(&sensors, now, &zone)?;
        loop {
            if *cancel.borrow() {
                return Ok(());
            }
            let now = Utc::now();
            let next_deadline = deadlines
                .iter()
                .min()
                .copied()
                .expect("non-empty sensor set has a deadline");
            let delay = next_deadline
                .signed_duration_since(now)
                .to_std()
                .unwrap_or(StdDuration::ZERO);
            tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return Ok(());
                }
            }
                () = tokio::time::sleep(delay) => {
                    let update_time = Utc::now();
                    zone = runtime.time_zone();
                    for index in 0..sensors.len() {
                        if deadlines[index] <= update_time {
                            publish_time_date(&sensors[index], &handles[index], update_time, &zone)?;
                            deadlines[index] = next_time_date_update(sensors[index].option(), update_time, &zone)?;
                        }
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) if event.event_type == "core_config_updated" => {
                            let update_time = Utc::now();
                            zone = runtime.time_zone();
                            refresh_all_time_date(&sensors, &handles, update_time, &zone)?;
                            deadlines = calculate_deadlines(&sensors, update_time, &zone)?;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // A missed event may have been a core timezone update;
                            // refresh and recalculate from current runtime config.
                            let update_time = Utc::now();
                            zone = runtime.time_zone();
                            refresh_all_time_date(&sensors, &handles, update_time, &zone)?;
                            deadlines = calculate_deadlines(&sensors, update_time, &zone)?;
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }

    fn calculate_deadlines(
        sensors: &[TimeDateSensor],
        now: DateTime<Utc>,
        time_zone: &str,
    ) -> Result<Vec<DateTime<Utc>>, ProviderError> {
        sensors
            .iter()
            .map(|sensor| {
                next_time_date_update(sensor.option(), now, time_zone).map_err(Into::into)
            })
            .collect()
    }

    fn refresh_all_time_date(
        sensors: &[TimeDateSensor],
        handles: &[EntityHandle],
        now: DateTime<Utc>,
        time_zone: &str,
    ) -> Result<(), ProviderError> {
        for (sensor, handle) in sensors.iter().zip(handles) {
            publish_time_date(sensor, handle, now, time_zone)?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use hass_core::MemoryStore;
        use hass_registry::Registries;
        use std::sync::Arc;
        use std::time::Duration;

        fn test_lifecycle() -> (Runtime, EntityLifecycle) {
            let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
            let registries = Registries::new(runtime.clone()).unwrap();
            let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
            (runtime, lifecycle)
        }

        #[tokio::test]
        async fn config_entry_providers_are_scoped_and_release_only_their_entities() {
            let (runtime, lifecycle) = test_lifecycle();
            runtime.set_time_zone("UTC").unwrap();
            let first = TimeDateIntegration::start(
                runtime.clone(),
                &lifecycle,
                TimeDateYamlConfig::parse(&json!({ "display_options": "time" })).unwrap(),
                "shared-platform-id",
                Some("entry-a"),
            )
            .unwrap();
            let second = TimeDateIntegration::start(
                runtime.clone(),
                &lifecycle,
                TimeDateYamlConfig::parse(&json!({ "display_options": "date" })).unwrap(),
                "shared-platform-id",
                Some("entry-b"),
            )
            .unwrap();
            let first_id = first.entity_ids()[0].clone().unwrap();
            let second_id = second.entity_ids()[0].clone().unwrap();
            assert!(runtime.get_state(&first_id).is_some());
            assert!(runtime.get_state(&second_id).is_some());

            first.shutdown().await.unwrap();
            assert_eq!(runtime.get_state(&first_id).unwrap().state, "unavailable");
            assert!(runtime.get_state(&second_id).is_some());
            second.shutdown().await.unwrap();
            assert_eq!(runtime.get_state(&second_id).unwrap().state, "unavailable");
        }

        #[tokio::test]
        async fn core_timezone_update_refreshes_state_without_waiting_for_minute_tick() {
            let (runtime, lifecycle) = test_lifecycle();
            runtime.set_time_zone("UTC").unwrap();
            let integration = TimeDateIntegration::start(
                runtime.clone(),
                &lifecycle,
                TimeDateYamlConfig::parse(&json!({ "display_options": "time" })).unwrap(),
                "yaml-time-date",
                None,
            )
            .unwrap();
            let entity_id = integration.entity_ids()[0].clone().unwrap();
            let before = runtime.get_state(&entity_id).unwrap().state;

            runtime.set_time_zone("Asia/Kolkata").unwrap();
            runtime
                .fire_event("core_config_updated", Map::new(), Context::default())
                .unwrap();

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let current = runtime.get_state(&entity_id).unwrap().state;
                    let expected =
                        format_time_date_state(DisplayOption::Time, Utc::now(), "Asia/Kolkata")
                            .unwrap();
                    if current == expected && current != before {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("timezone event should refresh the state promptly");

            integration.shutdown().await.unwrap();
            lifecycle.shutdown().await.unwrap();
        }
    }
}

#[cfg(feature = "runtime")]
pub use runtime::{ProviderError, TimeDateIntegration, UptimeIntegration};

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(timestamp: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(timestamp, 0).single().unwrap()
    }

    fn at_local(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn options(value: Value) -> TimeDateYamlConfig {
        TimeDateYamlConfig::parse(&value).unwrap()
    }

    #[test]
    fn pinned_option_inventory_excludes_stale_beat_translation() {
        assert_eq!(
            DisplayOption::ALL.map(DisplayOption::key),
            [
                "time",
                "date",
                "date_time",
                "date_time_utc",
                "date_time_iso",
                "time_date",
                "time_utc",
            ]
        );
        assert_eq!(
            DisplayOption::try_from("beat"),
            Err(ConfigError::UnsupportedDisplayOption("beat".into()))
        );
    }

    #[test]
    fn pinned_utc_values_match_component_test_expectations() {
        let now = utc(1_495_068_856);
        let expected = [
            (DisplayOption::Time, "00:54"),
            (DisplayOption::Date, "2017-05-18"),
            (DisplayOption::DateTime, "2017-05-18, 00:54"),
            (DisplayOption::DateTimeUtc, "2017-05-18, 00:54"),
            (DisplayOption::DateTimeIso, "2017-05-18T00:54:00"),
            (DisplayOption::TimeDate, "00:54, 2017-05-18"),
            (DisplayOption::TimeUtc, "00:54"),
        ];
        for (option, state) in expected {
            assert_eq!(format_time_date_state(option, now, "UTC").unwrap(), state);
        }
    }

    #[test]
    fn pinned_timezone_values_match_new_york_and_prague_expectations() {
        let new_york_start = utc(1_495_068_856);
        let new_york_end = utc(1_602_952_963);
        let prague_end = new_york_end;
        let expected = [
            ("time", DisplayOption::Time, new_york_start, "20:54"),
            ("date", DisplayOption::Date, new_york_start, "2017-05-17"),
            (
                "date_time",
                DisplayOption::DateTime,
                new_york_start,
                "2017-05-17, 20:54",
            ),
            (
                "date_time_utc",
                DisplayOption::DateTimeUtc,
                new_york_start,
                "2017-05-18, 00:54",
            ),
            (
                "date_time_iso",
                DisplayOption::DateTimeIso,
                new_york_start,
                "2017-05-17T20:54:00",
            ),
            (
                "time_date",
                DisplayOption::TimeDate,
                new_york_start,
                "20:54, 2017-05-17",
            ),
            ("time_utc", DisplayOption::TimeUtc, new_york_start, "00:54"),
        ];
        for (_, option, now, expected) in expected {
            assert_eq!(
                format_time_date_state(option, now, "America/New_York").unwrap(),
                expected
            );
        }
        for (option, expected) in [
            (DisplayOption::Time, "12:42"),
            (DisplayOption::Date, "2020-10-17"),
            (DisplayOption::DateTime, "2020-10-17, 12:42"),
            (DisplayOption::DateTimeUtc, "2020-10-17, 16:42"),
            (DisplayOption::DateTimeIso, "2020-10-17T12:42:00"),
            (DisplayOption::TimeDate, "12:42, 2020-10-17"),
            (DisplayOption::TimeUtc, "16:42"),
        ] {
            assert_eq!(
                format_time_date_state(option, new_york_end, "America/New_York").unwrap(),
                expected
            );
        }
        assert_eq!(
            format_time_date_state(DisplayOption::DateTime, prague_end, "Europe/Prague").unwrap(),
            "2020-10-17, 18:42"
        );
    }

    #[test]
    fn sensor_snapshot_reads_the_current_timezone_each_time() {
        let sensor = TimeDateSensor::new(DisplayOption::Time, None);
        let now = utc(1_495_068_856);
        assert_eq!(sensor.snapshot_at(now, "UTC").unwrap().state, "00:54");
        assert_eq!(
            sensor.snapshot_at(now, "America/New_York").unwrap().state,
            "20:54"
        );
    }

    #[test]
    fn source_minute_boundaries_use_the_next_utc_minute() {
        let at_45_5 = DateTime::from_timestamp(45, 500_000_000).unwrap();
        assert_eq!(
            next_time_date_update(DisplayOption::Time, at_45_5, "UTC")
                .unwrap()
                .timestamp(),
            60
        );
        for option in [DisplayOption::DateTime, DisplayOption::TimeDate] {
            assert_eq!(
                next_time_date_update(option, utc(1_495_068_899), "UTC")
                    .unwrap()
                    .timestamp(),
                1_495_068_900
            );
        }
        assert_eq!(
            next_time_date_update(DisplayOption::Time, utc(120), "UTC")
                .unwrap()
                .timestamp(),
            180
        );
    }

    #[test]
    fn pinned_local_date_boundaries_include_dst_short_and_long_days() {
        let cases = [
            ("America/New_York", utc(50_000), 104_400),
            (
                "America/Edmonton",
                at_local("2017-11-13T19:47:19-07:00"),
                at_local("2017-11-14T00:00:00-07:00").timestamp(),
            ),
            (
                "Europe/Prague",
                at_local("2020-03-29T00:00:00+01:00"),
                at_local("2020-03-30T00:00:00+02:00").timestamp(),
            ),
            (
                "Europe/Prague",
                at_local("2020-03-29T03:00:00+02:00"),
                at_local("2020-03-30T00:00:00+02:00").timestamp(),
            ),
            (
                "Europe/Prague",
                at_local("2020-10-25T00:00:00+02:00"),
                at_local("2020-10-26T00:00:00+01:00").timestamp(),
            ),
            (
                "Europe/Prague",
                at_local("2020-10-25T23:59:00+01:00"),
                at_local("2020-10-26T00:00:00+01:00").timestamp(),
            ),
        ];
        for (zone, now, expected) in cases {
            assert_eq!(
                next_time_date_update(DisplayOption::Date, now, zone)
                    .unwrap()
                    .timestamp(),
                expected,
                "zone {zone} at {now}"
            );
        }
    }

    #[test]
    fn local_midnight_resolution_matches_zoneinfo_gaps_and_folds() {
        let cases = [
            // Sao Paulo skipped 00:00 at the start of DST; ZoneInfo attaches
            // the previous offset, which resolves to the first valid instant.
            (
                "America/Sao_Paulo",
                at_local("2018-11-03T12:00:00-03:00"),
                1_541_300_400,
            ),
            // Havana repeated 00:00 when DST ended; fold=0 chooses the first
            // occurrence, matching `LocalResult::Ambiguous`'s earliest value.
            (
                "America/Havana",
                at_local("2018-11-03T12:00:00-04:00"),
                1_541_304_000,
            ),
        ];
        for (zone, now, expected) in cases {
            assert_eq!(
                next_time_date_update(DisplayOption::Date, now, zone)
                    .unwrap()
                    .timestamp(),
                expected,
                "zone {zone} at {now}"
            );
        }
    }

    #[test]
    fn yaml_flow_and_timezone_validation_follow_pinned_schemas() {
        assert_eq!(
            options(json!({})).display_options,
            vec![DisplayOption::Time]
        );
        assert_eq!(
            options(json!({"display_options": "time_utc"})).display_options,
            vec![DisplayOption::TimeUtc]
        );
        assert_eq!(
            options(json!({"display_options": ["time", "date", "time"]})).display_options,
            vec![
                DisplayOption::Time,
                DisplayOption::Date,
                DisplayOption::Time
            ]
        );
        assert_eq!(
            TimeDateYamlConfig::parse(&json!({"display_options": ["beat"]})),
            Err(ConfigError::UnsupportedDisplayOption("beat".into()))
        );
        let shared_platform_options = options(json!({
            "display_options": "time",
            "entity_namespace": "office",
            "scan_interval": "00:00:30"
        }));
        assert_eq!(
            shared_platform_options.entity_namespace.as_deref(),
            Some("office")
        );
        assert_eq!(
            shared_platform_options.scan_interval,
            Some(json!("00:00:30"))
        );
        assert_eq!(
            shared_platform_options.sensors(None)[0]
                .metadata()
                .suggested_object_id,
            "office time"
        );
        for scan_interval in [
            json!(30),
            json!(0.5),
            json!("00:00:30"),
            json!("30"),
            json!({"seconds": 30}),
            json!({"minutes": "0.5"}),
        ] {
            assert!(TimeDateYamlConfig::parse(&json!({"scan_interval": scan_interval})).is_ok());
        }
        for scan_interval in [
            json!(null),
            json!("not a period"),
            json!({"months": 1}),
            json!({"seconds": "NaN"}),
        ] {
            assert_eq!(
                TimeDateYamlConfig::parse(&json!({"scan_interval": scan_interval})),
                Err(ConfigError::InvalidScanInterval)
            );
        }
        assert_eq!(
            TimeDateYamlConfig::parse(&json!({"display_options": 3})),
            Err(ConfigError::InvalidDisplayOptionsShape)
        );
        assert_eq!(
            TimeDateYamlConfig::parse(&json!({"unsupported": true})),
            Err(ConfigError::UnknownField("unsupported".into()))
        );
        assert_eq!(
            TimeDateFlowConfig::parse(&json!({"display_options": "time"}), Some("UTC"))
                .unwrap()
                .entry_title(),
            "Time & Date time"
        );
        let flow =
            TimeDateFlowConfig::parse(&json!({"display_options": "time"}), Some("UTC")).unwrap();
        assert!(flow.entry_data().is_empty());
        assert_eq!(flow.entry_options()["display_options"], "time");
        assert_eq!(
            TimeDateFlowConfig::parse(&json!({"display_options": "time"}), None),
            Err(ConfigError::TimezoneNotConfigured)
        );
        assert_eq!(
            TimeDateFlowConfig::parse(&json!({"display_options": ["time"]}), Some("UTC")),
            Err(ConfigError::InvalidDisplayOptionsShape)
        );
        assert_eq!(
            TimeDateFlowConfig::parse(&json!({"display_options": "time", "x": true}), Some("UTC")),
            Err(ConfigError::UnknownField("x".into()))
        );
        let yaml_scalar: Value = serde_yaml::from_str("display_options: time_date").unwrap();
        assert_eq!(
            TimeDateYamlConfig::parse(&yaml_scalar)
                .unwrap()
                .display_options,
            vec![DisplayOption::TimeDate]
        );
        let yaml_list: Value =
            serde_yaml::from_str("display_options:\n  - time\n  - date\n").unwrap();
        assert_eq!(
            TimeDateYamlConfig::parse(&yaml_list)
                .unwrap()
                .display_options,
            vec![DisplayOption::Time, DisplayOption::Date]
        );
        let yaml_beat: Value = serde_yaml::from_str("display_options: [beat]").unwrap();
        assert_eq!(
            TimeDateYamlConfig::parse(&yaml_beat),
            Err(ConfigError::UnsupportedDisplayOption("beat".into()))
        );
        assert!(parse_time_zone("America/New_York").is_ok());
        assert_eq!(
            parse_time_zone("not/a/timezone"),
            Err(ConfigError::InvalidTimezone("not/a/timezone".into()))
        );
    }

    #[test]
    fn uptime_flow_and_setup_snapshot_match_pinned_semantics() {
        let flow = UptimeFlowConfig::parse(&json!({})).unwrap();
        assert_eq!(flow.entry_title(), "Uptime");
        assert!(flow.entry_data().is_empty());
        assert!(flow.entry_options().is_empty());
        assert!(UptimeFlowConfig::parse(&json!({"unexpected": 1})).is_err());

        let sensor = UptimeSensor::at("entry-1", "Uptime", utc(1_646_092_800));
        assert_eq!(sensor.snapshot().state, "2022-03-01T00:00:00+00:00");
        assert_eq!(sensor.setup_time(), utc(1_646_092_800));
        assert_eq!(sensor.metadata().domain, "sensor");
        assert_eq!(sensor.metadata().platform, "uptime");
        assert_eq!(sensor.metadata().config_entry_id, "entry-1");
        assert_eq!(sensor.metadata().unique_id, "entry-1");
        assert_eq!(sensor.metadata().suggested_object_id, "uptime");
        assert!(sensor.metadata().has_entity_name);
        assert_eq!(
            sensor.metadata().device.identifiers[0],
            ("uptime".into(), "entry-1".into())
        );
        assert_eq!(sensor.metadata().device.name, "Uptime");
        assert_eq!(sensor.metadata().device.entry_type, "service");
        assert_eq!(sensor.metadata().device_class, "uptime");
        assert_eq!(sensor.metadata().name, None);
        assert_eq!(sensor.metadata().translation_key, None);
        assert!(!sensor.metadata().should_poll);
        assert_eq!(sensor.metadata().state_class, None);
        assert_eq!(sensor.metadata().unit_of_measurement, None);
        assert_eq!(sensor.metadata().icon, None);
        assert_eq!(sensor.metadata().entity_category, None);
        assert_eq!(sensor.snapshot().attributes["device_class"], "uptime");
        assert_eq!(sensor.snapshot().attributes["friendly_name"], "Uptime");
        assert_eq!(
            format_utc_timestamp(DateTime::from_timestamp(1_646_092_800, 123_456_789).unwrap()),
            "2022-03-01T00:00:00.123456+00:00"
        );
    }

    #[test]
    fn ordinary_sensor_constructors_read_the_real_clock() {
        let before = Utc::now();
        let uptime = UptimeSensor::new("entry-now", "Uptime");
        let uptime_after = Utc::now();
        assert!(uptime.setup_time() >= before);
        assert!(uptime.setup_time() <= uptime_after);

        let sensor = TimeDateSensor::new(DisplayOption::Time, None);
        let time_before = Utc::now();
        let snapshot = sensor.snapshot("UTC").unwrap();
        let time_after = Utc::now();
        let before_state = format_time_date_state(DisplayOption::Time, time_before, "UTC").unwrap();
        let after_state = format_time_date_state(DisplayOption::Time, time_after, "UTC").unwrap();
        assert!(snapshot.state == before_state || snapshot.state == after_state);
    }

    #[test]
    fn time_date_entity_metadata_matches_upstream_identity_and_icons() {
        for (option, expected_icon) in [
            (DisplayOption::Time, "mdi:clock"),
            (DisplayOption::Date, "mdi:calendar"),
            (DisplayOption::DateTime, "mdi:calendar-clock"),
            (DisplayOption::DateTimeUtc, "mdi:calendar-clock"),
            (DisplayOption::DateTimeIso, "mdi:calendar-clock"),
            (DisplayOption::TimeDate, "mdi:calendar-clock"),
            (DisplayOption::TimeUtc, "mdi:clock"),
        ] {
            let yaml_sensor = TimeDateSensor::new(option, None);
            let metadata = yaml_sensor.metadata();
            assert_eq!(metadata.domain, "sensor");
            assert_eq!(metadata.platform, "time_date");
            assert_eq!(metadata.unique_id, None);
            assert_eq!(metadata.suggested_object_id, option.key());
            assert_eq!(metadata.translation_key, option.key());
            assert!(metadata.has_entity_name);
            assert!(!metadata.should_poll);
            assert_eq!(metadata.device_class, None);
            assert_eq!(metadata.state_class, None);
            assert_eq!(metadata.unit_of_measurement, None);
            assert_eq!(metadata.icon, expected_icon);
            assert_eq!(metadata.entity_category, None);
            let snapshot = yaml_sensor.snapshot_at(utc(1_495_068_856), "UTC").unwrap();
            assert_eq!(snapshot.attributes["icon"], expected_icon);
            assert_eq!(snapshot.attributes["friendly_name"], option.friendly_name());

            let entry_sensor = TimeDateSensor::new(option, Some("entry-1"));
            assert_eq!(
                entry_sensor.metadata().unique_id.as_deref(),
                Some(option.key())
            );
            assert_eq!(
                entry_sensor.metadata().config_entry_id.as_deref(),
                Some("entry-1")
            );
        }
    }
}
