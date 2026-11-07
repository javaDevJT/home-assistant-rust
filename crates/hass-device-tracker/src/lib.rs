//! Native Home Assistant device tracker service and state provider.
//!
//! The first slice models the `device_tracker.see` service and explicit
//! tracked-device metadata. It does not load scanner platforms or write
//! `known_devices.yaml`.

use chrono::Duration;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use thiserror::Error;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use hass_core::{Context, Error as CoreError, Runtime, ServiceHandler, ServiceResult, State};
use hass_zone::ZoneManager;
use md5::{Digest, Md5};

const SERVICE_DOMAIN: &str = "device_tracker";
const SERVICE_SEE: &str = "see";
/// Event emitted when `device_tracker.see` first creates a device record.
pub const EVENT_NEW_DEVICE: &str = "device_tracker_new_device";
/// Private dispatcher signal for an integration's actual connected device.
pub const CONNECTED_DEVICE_REGISTERED: &str = "device_tracker_connected_device_registered";
const METADATA_KEY: &str = "hass-device-tracker.native.v1";
const HOME_ZONE_ID: &str = "zone.home";
const STATE_HOME: &str = "home";
const STATE_NOT_HOME: &str = "not_home";
const TRACKING_TYPE_POSITION: &str = "position";

/// Connection metadata supplied by a native router/tracker integration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectedDevice {
    pub ip: Option<String>,
    pub host_name: Option<String>,
    pub mac: Option<String>,
}

/// Forward a genuine integration observation to private consumers such as DHCP.
/// This does not create a tracker, persist an event or publish to the public bus.
pub fn dispatch_connected_device(
    runtime: &Runtime,
    device: &ConnectedDevice,
    context: Context,
) -> hass_core::Result<()> {
    runtime.dispatch_signal(
        CONNECTED_DEVICE_REGISTERED,
        Map::from_iter([
            ("ip".into(), json!(device.ip)),
            ("host_name".into(), json!(device.host_name)),
            ("mac".into(), json!(device.mac)),
        ]),
        context,
    )?;
    Ok(())
}

/// Errors returned by the native provider.
#[derive(Debug, Error)]
pub enum TrackerError {
    /// An underlying runtime operation failed.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// A supplied native configuration is invalid.
    #[error("invalid tracker configuration: {0}")]
    InvalidConfig(String),
    /// A `device_tracker.see` payload is invalid.
    #[error("invalid device_tracker.see payload: {0}")]
    InvalidServiceData(String),
    /// Persisted private provider metadata is invalid.
    #[error("invalid persisted tracker metadata: {0}")]
    InvalidMetadata(String),
    /// The integration was unloaded or has not finished starting.
    #[error("device tracker integration is not running")]
    NotRunning,
    /// Startup needs an active Tokio executor for consider_home timers.
    #[error("device tracker integration requires an active Tokio runtime")]
    TokioRuntimeUnavailable,
}

/// Result type for tracker provider operations.
pub type Result<T> = std::result::Result<T, TrackerError>;

/// Device tracker position or connection source.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    /// A GPS position report.
    #[default]
    Gps,
    /// A router or network presence report.
    Router,
    /// A Bluetooth presence report.
    Bluetooth,
    /// A Bluetooth Low Energy presence report.
    BluetoothLe,
}

/// One report accepted by the `device_tracker.see` contract.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackerUpdate {
    /// Stable device id. At least this or `mac` must be present.
    pub dev_id: Option<String>,
    /// Hardware address used to resolve a previously configured device.
    pub mac: Option<String>,
    /// Reported host name.
    pub host_name: Option<String>,
    /// Legacy location label; when nonempty it takes precedence over GPS state.
    pub location_name: Option<String>,
    /// GPS latitude and longitude in degrees.
    pub gps: Option<[f64; 2]>,
    /// GPS accuracy in meters. The upstream schema accepts zero.
    pub gps_accuracy: Option<u32>,
    /// Battery percentage as reported by the device.
    pub battery: Option<u32>,
    /// Additional JSON state attributes, merged into the device attributes.
    pub attributes: Option<Map<String, Value>>,
    /// Source class for this observation. The upstream default is GPS.
    pub source_type: SourceType,
    /// Optional per-report consider-home time period.
    pub consider_home: Option<Duration>,
    /// Accepted iOS compatibility field. Upstream drops this field.
    pub hostname: Option<String>,
    /// Accepted iOS compatibility field. Upstream drops this field.
    pub battery_status: Option<String>,
}

/// Outcome of applying a tracker report.
#[derive(Clone, Debug, PartialEq)]
pub struct SeeResult {
    /// Entity id resolved for this report.
    pub entity_id: String,
    /// The current state, absent when the device is configured with `track=false`.
    pub state: Option<State>,
    /// Whether this report created the device record.
    pub created: bool,
    /// Whether the runtime emitted a state change.
    pub changed: bool,
}

/// Public synchronous state manager for typed device tracker reports.
#[derive(Clone)]
pub struct TrackerManager {
    runtime: Runtime,
    inner: Arc<TrackerInner>,
}

/// Owner of the tracker service registration and staleness worker.
pub struct DeviceTrackerIntegration {
    manager: TrackerManager,
    owner: String,
    stop_tx: watch::Sender<bool>,
    worker: Option<JoinHandle<Result<()>>>,
}

struct TrackerInner {
    zones: ZoneManager,
    config: TrackerConfig,
    devices: Mutex<PersistedTrackerData>,
    wake: Notify,
    running: AtomicBool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedTrackerData {
    schema_version: u8,
    devices: BTreeMap<String, PersistedDevice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedDevice {
    config: TrackerDeviceConfig,
    origin: DeviceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gps: Option<[f64; 2]>,
    #[serde(default)]
    gps_accuracy: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_type: Option<SourceType>,
    #[serde(default)]
    attributes: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    expired: bool,
    /// Last runtime context that proved ownership of the state row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_state_context_id: Option<String>,
    /// Crash-recovery context written before a corresponding state update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_state_context_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeviceOrigin {
    /// Device metadata supplied by native configuration or a YAML adapter.
    Config,
    /// Device metadata created by an accepted `see` report.
    See,
}

/// Native tracker integration defaults and configured devices.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    /// Time a report without a fresh GPS fix remains home.
    #[serde(default = "default_consider_home", with = "duration_seconds")]
    pub consider_home: Duration,
    /// Whether new `see` reports create tracked device metadata by default.
    #[serde(default = "default_track_new_devices")]
    pub track_new_devices: bool,
    /// Explicitly configured tracked-device metadata.
    #[serde(default)]
    pub devices: Vec<TrackerDeviceConfig>,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            consider_home: default_consider_home(),
            track_new_devices: default_track_new_devices(),
            devices: Vec::new(),
        }
    }
}

impl TrackerConfig {
    /// Validate tracker defaults and every configured device without touching
    /// runtime state or persisted metadata.
    pub fn validate(&self) -> Result<()> {
        validate_duration(self.consider_home, "consider_home")?;
        let mut ids = BTreeSet::new();
        let mut macs = BTreeSet::new();
        for device in &self.devices {
            let normalized = normalize_device_id(&device.dev_id);
            if normalized.is_empty() || normalized.len() > 240 {
                return Err(TrackerError::InvalidConfig(format!(
                    "invalid device id {:?}",
                    device.dev_id
                )));
            }
            if !ids.insert(normalized.clone()) {
                return Err(TrackerError::InvalidConfig(format!(
                    "duplicate device id after slug normalization: {normalized}"
                )));
            }
            if let Some(mac) = &device.mac {
                let normalized_mac = mac.to_ascii_uppercase();
                if !normalized_mac.is_empty() && !macs.insert(normalized_mac.clone()) {
                    return Err(TrackerError::InvalidConfig(format!(
                        "duplicate device MAC address: {normalized_mac}"
                    )));
                }
            }
            if let Some(duration) = device.consider_home {
                validate_duration(duration, "device consider_home")?;
            }
            if let Some(icon) = &device.icon {
                validate_icon(icon)?;
            }
        }
        Ok(())
    }
}

/// Parse the contents of Home Assistant's read-only `known_devices.yaml`
/// document after the application's YAML loader has converted it to JSON.
///
/// The parser accepts the pinned legacy schema (`name`, `icon`, `track`, `mac`,
/// `gravatar`, `picture`, `consider_home`) and ignores only `vendor` and
/// `hide_if_away`, as upstream does. A non-null `gravatar` is lowercased and
/// hashed locally into the picture URL, and takes precedence over `picture`.
/// This function does not read or write any file or make network requests.
pub fn parse_known_devices(document: Value) -> Result<Vec<TrackerDeviceConfig>> {
    let entries = document.as_object().ok_or_else(|| {
        TrackerError::InvalidConfig("known_devices.yaml must contain a mapping".into())
    })?;
    let mut devices = Vec::with_capacity(entries.len());
    let mut ids = BTreeSet::new();
    let mut macs = BTreeSet::new();

    for (raw_id, raw_config) in entries {
        let fields = raw_config.as_object().ok_or_else(|| {
            TrackerError::InvalidConfig(format!("known device {raw_id:?} must contain a mapping"))
        })?;
        let dev_id = normalize_device_id(raw_id);
        if dev_id.is_empty() || dev_id.len() > 240 {
            return Err(TrackerError::InvalidConfig(format!(
                "invalid known device id {raw_id:?}"
            )));
        }
        if !ids.insert(dev_id.clone()) {
            return Err(TrackerError::InvalidConfig(format!(
                "duplicate known device id after slug normalization: {dev_id}"
            )));
        }
        for field in fields.keys() {
            if !matches!(
                field.as_str(),
                "name"
                    | "icon"
                    | "track"
                    | "mac"
                    | "gravatar"
                    | "picture"
                    | "consider_home"
                    | "vendor"
                    | "hide_if_away"
            ) {
                return Err(TrackerError::InvalidConfig(format!(
                    "unsupported known_devices.yaml field {field:?} for {raw_id:?}"
                )));
            }
        }

        let name = required_string(fields, "name", raw_id)?;
        let icon = optional_string(fields, "icon", raw_id)?
            .map(|icon| validate_icon(&icon))
            .transpose()?;
        let gravatar = optional_string(fields, "gravatar", raw_id)?;
        let picture = gravatar
            .as_deref()
            .map(gravatar_picture_url)
            .or(optional_string(fields, "picture", raw_id)?);
        let track = match fields.get("track") {
            Some(Value::Bool(value)) => *value,
            None => false,
            _ => {
                return Err(TrackerError::InvalidConfig(format!(
                    "known device {raw_id:?} field track must be a boolean"
                )));
            }
        };
        let mac = optional_string(fields, "mac", raw_id)?.map(|value| value.to_ascii_uppercase());
        if let Some(mac) = &mac
            && !mac.is_empty()
            && !macs.insert(mac.clone())
        {
            return Err(TrackerError::InvalidConfig(format!(
                "duplicate device MAC address: {mac}"
            )));
        }
        let consider_home = fields
            .get("consider_home")
            .filter(|value| !value.is_null())
            .map(|value| parse_time_period(value).map_err(TrackerError::InvalidConfig))
            .transpose()?;
        if let Some(duration) = consider_home {
            validate_duration(duration, "known-device consider_home")?;
        }

        devices.push(TrackerDeviceConfig {
            dev_id,
            name: Some(name),
            mac,
            track,
            consider_home,
            icon,
            picture,
        });
    }

    Ok(devices)
}

fn gravatar_picture_url(email: &str) -> String {
    let normalized_email = email.to_ascii_lowercase();
    let hash = format!("{:x}", Md5::digest(normalized_email.as_bytes()));
    format!("https://www.gravatar.com/avatar/{hash}.jpg?s=80&d=wavatar")
}

/// Metadata corresponding to one `known_devices.yaml` entry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerDeviceConfig {
    /// Stable device identifier (the key in Home Assistant's YAML file).
    pub dev_id: String,
    /// Configured display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// MAC address used to resolve reports to this device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Whether state should be published for this device.
    #[serde(default)]
    pub track: bool,
    /// Per-device stale-report threshold.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_seconds_option"
    )]
    pub consider_home: Option<Duration>,
    /// Configured icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Configured picture URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

fn default_consider_home() -> Duration {
    Duration::seconds(180)
}

const fn default_track_new_devices() -> bool {
    true
}

mod duration_seconds {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(duration.num_milliseconds() as f64 / 1_000.0)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = f64::deserialize(deserializer)?;
        if !seconds.is_finite() {
            return Err(serde::de::Error::custom("duration must be finite"));
        }
        let millis = seconds * 1_000.0;
        if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
            return Err(serde::de::Error::custom("duration is out of range"));
        }
        Ok(Duration::milliseconds(millis.round() as i64))
    }
}

mod duration_seconds_option {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(duration) => {
                serializer.serialize_some(&(duration.num_milliseconds() as f64 / 1_000.0))
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(seconds) = Option::<f64>::deserialize(deserializer)? else {
            return Ok(None);
        };
        if !seconds.is_finite() {
            return Err(serde::de::Error::custom("duration must be finite"));
        }
        let millis = seconds * 1_000.0;
        if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
            return Err(serde::de::Error::custom("duration is out of range"));
        }
        Ok(Some(Duration::milliseconds(millis.round() as i64)))
    }
}

fn normalize_device_id(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut previous_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            previous_separator = false;
        } else if !previous_separator && !result.is_empty() {
            result.push('_');
            previous_separator = true;
        }
    }
    while result.ends_with('_') {
        result.pop();
    }
    result
}

fn validate_duration(duration: Duration, label: &str) -> Result<()> {
    if duration < Duration::zero() {
        return Err(TrackerError::InvalidConfig(format!(
            "{label} must be nonnegative"
        )));
    }
    Ok(())
}

fn validate_icon(icon: &str) -> Result<String> {
    let trimmed = icon.trim();
    if trimmed.contains(':') && !trimmed.chars().any(char::is_whitespace) {
        Ok(trimmed.to_owned())
    } else {
        Err(TrackerError::InvalidConfig(
            "icons must be specified as prefix:name without whitespace".into(),
        ))
    }
}

fn required_string(fields: &Map<String, Value>, field: &str, device_id: &str) -> Result<String> {
    fields
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            TrackerError::InvalidConfig(format!(
                "known device {device_id:?} requires string field {field:?}"
            ))
        })
}

fn optional_string(
    fields: &Map<String, Value>,
    field: &str,
    device_id: &str,
) -> Result<Option<String>> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(TrackerError::InvalidConfig(format!(
            "known device {device_id:?} field {field:?} must be a string or null"
        ))),
    }
}

fn parse_time_period(value: &Value) -> std::result::Result<Duration, String> {
    let seconds = match value {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| "time period number is out of range".to_owned())?,
        Value::String(value) => parse_time_period_string(value).or_else(|_| {
            value
                .parse::<f64>()
                .map_err(|_| "time period string must be HH:MM or seconds".to_owned())
        })?,
        Value::Object(fields) => {
            const ALLOWED: [&str; 5] = ["days", "hours", "minutes", "seconds", "milliseconds"];
            if fields.is_empty() {
                return Err("time period object must contain a duration field".into());
            }
            let mut total = 0.0;
            for (field, value) in fields {
                if !ALLOWED.contains(&field.as_str()) {
                    return Err(format!("unsupported time period field {field:?}"));
                }
                let amount = value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                    .ok_or_else(|| format!("time period field {field:?} must be numeric"))?;
                let multiplier = match field.as_str() {
                    "days" => 86_400.0,
                    "hours" => 3_600.0,
                    "minutes" => 60.0,
                    "seconds" => 1.0,
                    "milliseconds" => 0.001,
                    _ => unreachable!("field was checked against ALLOWED"),
                };
                total += amount * multiplier;
            }
            total
        }
        _ => return Err("time period must be seconds, a duration string, or a mapping".into()),
    };

    let milliseconds = seconds * 1_000.0;
    if !milliseconds.is_finite() || milliseconds < i64::MIN as f64 || milliseconds > i64::MAX as f64
    {
        return Err("time period is out of range".into());
    }
    Ok(Duration::milliseconds(milliseconds.round() as i64))
}

fn parse_time_period_string(value: &str) -> std::result::Result<f64, String> {
    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let parts: Vec<_> = unsigned.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return Err("time period string must be HH:MM or HH:MM:SS".into());
    }
    let hours: i64 = parts[0]
        .parse()
        .map_err(|_| "invalid duration hours".to_owned())?;
    let minutes: i64 = parts[1]
        .parse()
        .map_err(|_| "invalid duration minutes".to_owned())?;
    let seconds: f64 = if parts.len() == 3 {
        parts[2]
            .parse()
            .map_err(|_| "invalid duration seconds".to_owned())?
    } else {
        0.0
    };
    let total = hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds;
    if !total.is_finite() {
        return Err("time period is out of range".into());
    }
    Ok(if negative { -total } else { total })
}

fn core_error(error: TrackerError) -> CoreError {
    match error {
        TrackerError::Core(error) => error,
        other => CoreError::InvalidInput(other.to_string()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl DeviceTrackerIntegration {
    /// Load private metadata, publish owned tracker states, register the
    /// source-compatible `device_tracker.see` service, and start expiry timers.
    pub async fn start(
        runtime: Runtime,
        zones: ZoneManager,
        mut config: TrackerConfig,
    ) -> Result<Self> {
        config.validate()?;
        tokio::runtime::Handle::try_current().map_err(|_| TrackerError::TokioRuntimeUnavailable)?;

        for device in &mut config.devices {
            device.dev_id = normalize_device_id(&device.dev_id);
            device.mac = device.mac.take().map(|mac| mac.to_ascii_uppercase());
            device.icon = device.icon.take().map(|icon| icon.trim().to_owned());
        }

        let previous_value = runtime.get_data(METADATA_KEY)?;
        let previous = match previous_value.as_ref() {
            Some(value) => parse_persisted_data(value.clone())?,
            None => PersistedTrackerData::empty(),
        };
        let mut data = merge_config(&previous, &config)?;
        validate_persisted_data(&data)?;

        let owner = format!("hass-device-tracker-{}", Uuid::new_v4());
        let inner = Arc::new(TrackerInner {
            zones,
            config,
            devices: Mutex::new(data.clone()),
            wake: Notify::new(),
            running: AtomicBool::new(false),
        });
        let weak_inner = Arc::downgrade(&inner);
        let handler: ServiceHandler = Arc::new(move |runtime, call| {
            let inner = weak_inner
                .upgrade()
                .ok_or_else(|| CoreError::Service("device tracker is unloaded".into()))?;
            if !inner.running.load(Ordering::Acquire) {
                return Err(CoreError::Service("device tracker is not running".into()));
            }
            if !call.target.is_empty() {
                return Err(CoreError::InvalidInput(
                    "device_tracker.see does not accept service targets".into(),
                ));
            }
            let update = parse_service_data(&call.data).map_err(core_error)?;
            let manager = TrackerManager {
                runtime: runtime.clone(),
                inner,
            };
            let result = manager
                .see(update, call.context.clone())
                .map_err(core_error)?;
            Ok(ServiceResult {
                changed_states: if result.changed {
                    result.state.into_iter().collect()
                } else {
                    Vec::new()
                },
                response: None,
                context: call.context.clone(),
            })
        });

        runtime.register_owned_service(
            &owner,
            SERVICE_DOMAIN,
            SERVICE_SEE,
            service_description(),
            handler,
        )?;

        let (stop_tx, stop_rx) = watch::channel(false);
        let setup = initialize_tracker(&runtime, &inner, &previous, &mut data);
        if let Err(error) = setup {
            let _ = runtime.unregister_owned_services(&owner);
            return Err(error);
        }

        *lock(&inner.devices) = data;
        inner.running.store(true, Ordering::Release);
        let manager = TrackerManager {
            runtime: runtime.clone(),
            inner: Arc::clone(&inner),
        };
        let worker = tokio::spawn(expiry_worker(runtime, inner, stop_rx));
        Ok(Self {
            manager,
            owner,
            stop_tx,
            worker: Some(worker),
        })
    }

    /// Return a cloneable typed interface for applying tracker reports.
    pub fn manager(&self) -> TrackerManager {
        self.manager.clone()
    }

    /// Stop timers, unregister the owned service, and wait for worker cleanup.
    /// Persisted states remain available for the next runtime restore.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.manager.inner.running.store(false, Ordering::Release);
        let _ = self.stop_tx.send(true);
        self.manager
            .runtime
            .unregister_owned_services(&self.owner)?;
        if let Some(worker) = self.worker.take() {
            worker.await.map_err(|error| {
                TrackerError::Core(CoreError::Service(format!(
                    "device tracker timer failed: {error}"
                )))
            })??;
        }
        Ok(())
    }
}

impl Drop for DeviceTrackerIntegration {
    fn drop(&mut self) {
        self.manager.inner.running.store(false, Ordering::Release);
        let _ = self.stop_tx.send(true);
        let _ = self.manager.runtime.unregister_owned_services(&self.owner);
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl TrackerManager {
    /// Apply one typed report using the supplied causal/user context.
    pub fn see(&self, update: TrackerUpdate, context: Context) -> Result<SeeResult> {
        if !self.inner.running.load(Ordering::Acquire) {
            return Err(TrackerError::NotRunning);
        }
        validate_update(&update)?;

        let mut devices = lock(&self.inner.devices);
        let (dev_id, existing_id) = resolve_device_id(&self.runtime, &devices, &update)?;
        let entity_id = entity_id_for(&dev_id);
        let created = existing_id.is_none();
        let mut next = devices.clone();
        let record = next
            .devices
            .entry(dev_id.clone())
            .or_insert_with(|| new_record(&self.inner.config, dev_id.clone(), &update));
        apply_update(record, &update);
        record.pending_state_context_id = None;

        let previous_record = devices.devices.get(&dev_id);
        let current_state = self.runtime.get_state(&entity_id);
        if record.config.track
            && let Some(state) = current_state.as_ref()
            && !previous_record.is_some_and(|record| state_is_owned(record, state))
        {
            return Err(TrackerError::InvalidServiceData(format!(
                "refusing to replace a state not owned by this provider: {entity_id}"
            )));
        }

        let built = if record.config.track {
            let (state, attributes) = build_state(
                record,
                &self.runtime,
                &self.inner.zones,
                self.inner.config.consider_home,
            )?;
            if state.chars().count() > 255 {
                return Err(TrackerError::InvalidServiceData(
                    "device state exceeds 255 characters".into(),
                ));
            }
            record.pending_state_context_id = Some(context.id.clone());
            Some((state, attributes))
        } else {
            None
        };
        validate_persisted_data(&next)?;
        let old_value = self.runtime.get_data(METADATA_KEY)?;
        store_metadata(&self.runtime, &next)?;

        let event_context = context.clone();
        let state_change = if let Some((state, attributes)) = built {
            match self
                .runtime
                .set_state(&entity_id, &state, attributes, context, false)
            {
                Ok(change) => Some(change),
                Err(error) => {
                    restore_metadata(&self.runtime, old_value.as_ref());
                    return Err(error.into());
                }
            }
        } else {
            None
        };

        let record = next
            .devices
            .get_mut(&dev_id)
            .expect("the report inserted or updated this device");
        record.pending_state_context_id = None;
        let (state, changed) = match state_change {
            Some(change) => {
                record.last_state_context_id = Some(change.state.context.id.clone());
                (Some(change.state), change.changed)
            }
            None => (None, false),
        };
        *devices = next;
        store_metadata(&self.runtime, &devices)?;
        if created {
            let record = devices
                .devices
                .get(&dev_id)
                .expect("the report inserted or updated this device");
            let mut event_data = Map::new();
            event_data.insert("entity_id".into(), Value::String(entity_id.clone()));
            event_data.insert(
                "host_name".into(),
                record
                    .host_name
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            event_data.insert(
                "mac".into(),
                record
                    .config
                    .mac
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            self.runtime
                .fire_event(EVENT_NEW_DEVICE, event_data, event_context)?;
        }
        self.inner.wake.notify_one();
        Ok(SeeResult {
            entity_id,
            state,
            created,
            changed,
        })
    }

    /// Return the currently configured and service-created device ids.
    pub fn device_ids(&self) -> Vec<String> {
        lock(&self.inner.devices).devices.keys().cloned().collect()
    }

    fn expire_due(&self, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut devices = lock(&self.inner.devices);
        let due: Vec<String> = devices
            .devices
            .iter()
            .filter(|(_, record)| is_expiry_due(record, now, self.inner.config.consider_home))
            .map(|(id, _)| id.clone())
            .collect();
        for dev_id in due {
            let entity_id = entity_id_for(&dev_id);
            let old_value = self.runtime.get_data(METADATA_KEY)?;
            let mut next = devices.clone();
            let record = next
                .devices
                .get_mut(&dev_id)
                .expect("expiry id came from the current metadata");
            record.gps = None;
            record.expired = true;
            let context = Context::default();
            let (state, attributes) = build_state(
                record,
                &self.runtime,
                &self.inner.zones,
                self.inner.config.consider_home,
            )?;
            if state.chars().count() > 255 {
                return Err(TrackerError::InvalidMetadata(
                    "expired device state exceeds 255 characters".into(),
                ));
            }
            record.pending_state_context_id = Some(context.id.clone());
            validate_persisted_data(&next)?;
            store_metadata(&self.runtime, &next)?;

            let change = match self
                .runtime
                .set_state(&entity_id, &state, attributes, context, false)
            {
                Ok(change) => change,
                Err(error) => {
                    restore_metadata(&self.runtime, old_value.as_ref());
                    return Err(error.into());
                }
            };
            let record = next
                .devices
                .get_mut(&dev_id)
                .expect("expiry id came from the current metadata");
            record.pending_state_context_id = None;
            record.last_state_context_id = Some(change.state.context.id.clone());
            *devices = next;
            store_metadata(&self.runtime, &devices)?;
        }
        self.inner.wake.notify_one();
        Ok(())
    }
}

async fn expiry_worker(
    runtime: Runtime,
    inner: Arc<TrackerInner>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *stop_rx.borrow() || !inner.running.load(Ordering::Acquire) {
            return Ok(());
        }
        let delay = next_expiry_delay(
            &lock(&inner.devices),
            chrono::Utc::now(),
            inner.config.consider_home,
        );
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    return Ok(());
                }
            }
            _ = inner.wake.notified() => {}
            _ = tokio::time::sleep(delay) => {
                TrackerManager { runtime: runtime.clone(), inner: Arc::clone(&inner) }
                    .expire_due(chrono::Utc::now())?;
            }
        }
    }
}

impl PersistedTrackerData {
    fn empty() -> Self {
        Self {
            schema_version: 1,
            devices: BTreeMap::new(),
        }
    }
}

fn service_description() -> Value {
    json!({
        "name": "See a device",
        "fields": {
            "mac": {"type": "string"},
            "dev_id": {"type": "string"},
            "host_name": {"type": "string"},
            "location_name": {"type": "string"},
            "gps": {"type": "[latitude, longitude]"},
            "gps_accuracy": {"type": "nonnegative integer"},
            "battery": {"type": "nonnegative integer"},
            "attributes": {"type": "object"},
            "source_type": {"type": "gps | router | bluetooth | bluetooth_le"},
            "consider_home": {"type": "time period"},
            "battery_status": {"type": "string", "ignored": true},
            "hostname": {"type": "string", "ignored": true}
        },
        "requires_any": ["mac", "dev_id"]
    })
}

fn parse_service_data(data: &Map<String, Value>) -> Result<TrackerUpdate> {
    const ALLOWED: [&str; 12] = [
        "mac",
        "dev_id",
        "host_name",
        "location_name",
        "gps",
        "gps_accuracy",
        "battery",
        "attributes",
        "source_type",
        "consider_home",
        "battery_status",
        "hostname",
    ];
    for field in data.keys() {
        if !ALLOWED.contains(&field.as_str()) {
            return Err(TrackerError::InvalidServiceData(format!(
                "unsupported field {field:?}"
            )));
        }
    }

    let dev_id = service_string(data, "dev_id")?;
    let mac = service_string(data, "mac")?;
    if dev_id.is_none() && mac.is_none() {
        return Err(TrackerError::InvalidServiceData(
            "at least one of mac or dev_id is required".into(),
        ));
    }
    let host_name = service_string(data, "host_name")?;
    let location_name = service_string(data, "location_name")?;
    let gps = match data.get("gps") {
        None => None,
        Some(Value::Array(values)) if values.len() == 2 => {
            let latitude = service_float(&values[0]).ok_or_else(|| {
                TrackerError::InvalidServiceData("gps latitude must be numeric".into())
            })?;
            let longitude = service_float(&values[1]).ok_or_else(|| {
                TrackerError::InvalidServiceData("gps longitude must be numeric".into())
            })?;
            validate_gps(latitude, longitude).map_err(TrackerError::InvalidServiceData)?;
            Some([latitude, longitude])
        }
        Some(_) => {
            return Err(TrackerError::InvalidServiceData(
                "gps must contain exactly latitude and longitude".into(),
            ));
        }
    };
    let gps_accuracy = service_integer(data, "gps_accuracy")?;
    let battery = service_integer(data, "battery")?;
    let attributes = match data.get("attributes") {
        None => None,
        Some(Value::Object(attributes)) => Some(attributes.clone()),
        Some(_) => {
            return Err(TrackerError::InvalidServiceData(
                "attributes must be an object".into(),
            ));
        }
    };
    let source_type = match service_string(data, "source_type")? {
        None => SourceType::Gps,
        Some(value) => match value.as_str() {
            "gps" => SourceType::Gps,
            "router" => SourceType::Router,
            "bluetooth" => SourceType::Bluetooth,
            "bluetooth_le" => SourceType::BluetoothLe,
            _ => {
                return Err(TrackerError::InvalidServiceData(format!(
                    "unknown source_type {value:?}"
                )));
            }
        },
    };
    let consider_home = data
        .get("consider_home")
        .map(|value| parse_time_period(value).map_err(TrackerError::InvalidServiceData))
        .transpose()?;
    let battery_status = service_string(data, "battery_status")?;
    let hostname = service_string(data, "hostname")?;

    let update = TrackerUpdate {
        dev_id,
        mac,
        host_name,
        location_name,
        gps,
        gps_accuracy,
        battery,
        attributes,
        source_type,
        consider_home,
        hostname,
        battery_status,
    };
    validate_update(&update)?;
    Ok(update)
}

fn service_string(data: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match data.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(TrackerError::InvalidServiceData(format!(
            "{key} must be a string"
        ))),
    }
}

fn service_integer(data: &Map<String, Value>, key: &str) -> Result<Option<u32>> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    let integer = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| {
                number.as_f64().and_then(|value| {
                    (value.is_finite() && value.fract() == 0.0 && value >= 0.0)
                        .then_some(value as i64)
                })
            }),
        Value::String(value) => value.parse::<i64>().ok(),
        _ => None,
    }
    .filter(|value| *value >= 0)
    .and_then(|value| u32::try_from(value).ok())
    .ok_or_else(|| {
        TrackerError::InvalidServiceData(format!("{key} must be a nonnegative 32-bit integer"))
    })?;
    Ok(Some(integer))
}

fn service_float(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn validate_update(update: &TrackerUpdate) -> Result<()> {
    if update.dev_id.is_none() && update.mac.is_none() {
        return Err(TrackerError::InvalidServiceData(
            "at least one of mac or dev_id is required".into(),
        ));
    }
    if let Some(dev_id) = &update.dev_id
        && normalize_device_id(dev_id).is_empty()
    {
        return Err(TrackerError::InvalidServiceData(
            "dev_id must contain at least one ASCII letter or digit".into(),
        ));
    }
    if let Some([latitude, longitude]) = update.gps {
        validate_gps(latitude, longitude).map_err(TrackerError::InvalidServiceData)?;
    }
    Ok(())
}

fn validate_gps(latitude: f64, longitude: f64) -> std::result::Result<(), String> {
    if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
        return Err("gps latitude must be between -90 and 90".into());
    }
    if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
        return Err("gps longitude must be between -180 and 180".into());
    }
    Ok(())
}

fn entity_id_for(dev_id: &str) -> String {
    format!("{SERVICE_DOMAIN}.{dev_id}")
}

fn new_record(config: &TrackerConfig, dev_id: String, update: &TrackerUpdate) -> PersistedDevice {
    PersistedDevice {
        config: TrackerDeviceConfig {
            dev_id,
            name: None,
            mac: update.mac.as_ref().map(|mac| mac.to_ascii_uppercase()),
            track: config.track_new_devices,
            consider_home: None,
            icon: None,
            picture: None,
        },
        origin: DeviceOrigin::See,
        host_name: None,
        location_name: None,
        gps: None,
        gps_accuracy: 0,
        battery: None,
        source_type: None,
        attributes: Map::new(),
        last_seen: None,
        expired: false,
        last_state_context_id: None,
        pending_state_context_id: None,
    }
}

fn apply_update(record: &mut PersistedDevice, update: &TrackerUpdate) {
    record.source_type = Some(update.source_type);
    record.last_seen = Some(chrono::Utc::now());
    if let Some(host_name) = update.host_name.as_ref().filter(|name| !name.is_empty()) {
        record.host_name = Some(host_name.clone());
    }
    record.location_name = update.location_name.clone();
    if let Some(duration) = update
        .consider_home
        .filter(|duration| *duration != Duration::zero())
    {
        record.config.consider_home = Some(duration);
    }
    record.gps = update.gps;
    if update.gps.is_some() {
        record.gps_accuracy = update.gps_accuracy.unwrap_or_default();
    }
    if let Some(battery) = update.battery {
        record.battery = Some(battery);
    }
    if let Some(attributes) = &update.attributes {
        record.attributes.extend(attributes.clone());
    }
    if record.config.mac.is_none() {
        record.config.mac = update.mac.as_ref().map(|mac| mac.to_ascii_uppercase());
    }
    record.expired = false;
}

fn merge_config(
    previous: &PersistedTrackerData,
    config: &TrackerConfig,
) -> Result<PersistedTrackerData> {
    let mut devices = BTreeMap::new();
    let mut configured_ids = BTreeSet::new();
    for device in &config.devices {
        let mut device = device.clone();
        device.dev_id = normalize_device_id(&device.dev_id);
        device.mac = device.mac.take().map(|mac| mac.to_ascii_uppercase());
        configured_ids.insert(device.dev_id.clone());
    }

    for (dev_id, record) in &previous.devices {
        if record.origin == DeviceOrigin::See && !configured_ids.contains(dev_id) {
            devices.insert(dev_id.clone(), record.clone());
        }
    }
    for mut configured in config.devices.clone() {
        configured.dev_id = normalize_device_id(&configured.dev_id);
        configured.mac = configured.mac.take().map(|mac| mac.to_ascii_uppercase());
        let record = previous
            .devices
            .get(&configured.dev_id)
            .cloned()
            .map(|mut record| {
                record.config = configured.clone();
                record.origin = DeviceOrigin::Config;
                record
            })
            .unwrap_or_else(|| empty_record(configured.clone(), DeviceOrigin::Config));
        devices.insert(configured.dev_id.clone(), record);
    }

    Ok(PersistedTrackerData {
        schema_version: 1,
        devices,
    })
}

fn empty_record(config: TrackerDeviceConfig, origin: DeviceOrigin) -> PersistedDevice {
    PersistedDevice {
        config,
        origin,
        host_name: None,
        location_name: None,
        gps: None,
        gps_accuracy: 0,
        battery: None,
        source_type: None,
        attributes: Map::new(),
        last_seen: None,
        expired: false,
        last_state_context_id: None,
        pending_state_context_id: None,
    }
}

fn parse_persisted_data(value: Value) -> Result<PersistedTrackerData> {
    let data: PersistedTrackerData = serde_json::from_value(value)
        .map_err(|error| TrackerError::InvalidMetadata(error.to_string()))?;
    validate_persisted_data(&data)?;
    Ok(data)
}

fn validate_persisted_data(data: &PersistedTrackerData) -> Result<()> {
    if data.schema_version != 1 {
        return Err(TrackerError::InvalidMetadata(format!(
            "unsupported schema version {}",
            data.schema_version
        )));
    }
    let mut macs = BTreeSet::new();
    for (dev_id, record) in &data.devices {
        if dev_id != &record.config.dev_id || normalize_device_id(dev_id) != *dev_id {
            return Err(TrackerError::InvalidMetadata(format!(
                "device key {dev_id:?} does not match its normalized id"
            )));
        }
        if dev_id.is_empty() || dev_id.len() > 240 {
            return Err(TrackerError::InvalidMetadata(format!(
                "invalid persisted device id {dev_id:?}"
            )));
        }
        if let Some(mac) = &record.config.mac
            && !mac.is_empty()
            && !macs.insert(mac.to_ascii_uppercase())
        {
            return Err(TrackerError::InvalidMetadata(format!(
                "duplicate persisted device MAC address {mac:?}"
            )));
        }
        if let Some([latitude, longitude]) = record.gps {
            validate_gps(latitude, longitude)
                .map_err(|error| TrackerError::InvalidMetadata(error.to_owned()))?;
        }
        if record
            .last_state_context_id
            .as_ref()
            .is_some_and(|id| id.is_empty())
            || record
                .pending_state_context_id
                .as_ref()
                .is_some_and(|id| id.is_empty())
        {
            return Err(TrackerError::InvalidMetadata(
                "empty persisted state ownership context".into(),
            ));
        }
    }
    Ok(())
}

fn store_metadata(runtime: &Runtime, data: &PersistedTrackerData) -> Result<()> {
    validate_persisted_data(data)?;
    let value = serde_json::to_value(data)
        .map_err(|error| TrackerError::InvalidMetadata(error.to_string()))?;
    runtime.set_data(METADATA_KEY, &value)?;
    Ok(())
}

fn restore_metadata(runtime: &Runtime, previous: Option<&Value>) {
    match previous {
        Some(value) => {
            let _ = runtime.set_data(METADATA_KEY, value);
        }
        None => {
            let _ = runtime.delete_data(METADATA_KEY);
        }
    }
}

fn state_is_owned(record: &PersistedDevice, state: &State) -> bool {
    record
        .last_state_context_id
        .as_ref()
        .is_some_and(|id| id == &state.context.id)
        || record
            .pending_state_context_id
            .as_ref()
            .is_some_and(|id| id == &state.context.id)
}

fn resolve_device_id(
    runtime: &Runtime,
    data: &PersistedTrackerData,
    update: &TrackerUpdate,
) -> Result<(String, Option<String>)> {
    if let Some(mac) = &update.mac {
        let normalized_mac = mac.to_ascii_uppercase();
        if let Some((dev_id, _)) = data.devices.iter().find(|(_, record)| {
            record.config.mac.as_ref().is_some_and(|known| {
                (record.origin == DeviceOrigin::See || !known.is_empty())
                    && known.eq_ignore_ascii_case(&normalized_mac)
            })
        }) {
            return Ok((dev_id.clone(), Some(dev_id.clone())));
        }

        let host_id = update
            .host_name
            .as_deref()
            .filter(|host_name| !host_name.is_empty())
            .map(normalize_device_id)
            .filter(|host_id| !host_id.is_empty());
        let base_id = host_id.unwrap_or_else(|| normalize_device_id(&normalized_mac));
        if base_id.is_empty() {
            return Err(TrackerError::InvalidServiceData(
                "mac and host_name do not produce a valid device id".into(),
            ));
        }
        let dev_id = if data.devices.contains_key(&base_id) {
            unique_device_id(&base_id, &data.devices)
        } else {
            base_id
        };
        reject_foreign_state(runtime, data, &dev_id)?;
        return Ok((dev_id, None));
    }

    let raw_id = update.dev_id.as_deref().ok_or_else(|| {
        TrackerError::InvalidServiceData("at least one of mac or dev_id is required".into())
    })?;
    let dev_id = normalize_device_id(raw_id);
    if dev_id.is_empty() {
        return Err(TrackerError::InvalidServiceData(
            "dev_id must contain at least one ASCII letter or digit".into(),
        ));
    }
    if data.devices.contains_key(&dev_id) {
        return Ok((dev_id.clone(), Some(dev_id)));
    }
    reject_foreign_state(runtime, data, &dev_id)?;
    Ok((dev_id, None))
}

fn unique_device_id(base_id: &str, devices: &BTreeMap<String, PersistedDevice>) -> String {
    for suffix in 2_u32.. {
        let suffix = format!("_{suffix}");
        let max_base_len = 240_usize.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", &base_id[..base_id.len().min(max_base_len)]);
        if !devices.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 id suffix space is exhausted")
}

fn reject_foreign_state(
    runtime: &Runtime,
    data: &PersistedTrackerData,
    dev_id: &str,
) -> Result<()> {
    let entity_id = entity_id_for(dev_id);
    if let Some(state) = runtime.get_state(&entity_id)
        && !data
            .devices
            .get(dev_id)
            .is_some_and(|record| state_is_owned(record, &state))
    {
        return Err(TrackerError::InvalidServiceData(format!(
            "a different provider already owns {entity_id}"
        )));
    }
    Ok(())
}

fn build_state(
    record: &PersistedDevice,
    runtime: &Runtime,
    zones: &ZoneManager,
    default_consider_home: Duration,
) -> Result<(String, Map<String, Value>)> {
    let mut zones_in = Vec::new();
    let mut active_zone = None;
    if let Some([latitude, longitude]) = record.gps {
        let matches = zones.zones_containing(latitude, longitude, record.gps_accuracy as f64)?;
        zones_in = matches.zones;
        active_zone = matches.active_zone;
    }

    let state = if let Some(location_name) = record
        .location_name
        .as_deref()
        .filter(|location_name| !location_name.is_empty())
    {
        location_name.to_owned()
    } else if record.source_type == Some(SourceType::Gps) && record.gps.is_some() {
        match active_zone.as_deref() {
            Some(HOME_ZONE_ID) => STATE_HOME.to_owned(),
            Some(zone_id) => runtime
                .get_state(zone_id)
                .and_then(|zone| {
                    zone.attributes
                        .get("friendly_name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .or_else(|| {
                    zones
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .map(|zone| zone.config.name)
                })
                .unwrap_or_else(|| STATE_NOT_HOME.to_owned()),
            None => STATE_NOT_HOME.to_owned(),
        }
    } else if !is_stale(record, chrono::Utc::now(), default_consider_home) {
        STATE_HOME.to_owned()
    } else {
        STATE_NOT_HOME.to_owned()
    };

    let mut attributes = Map::new();
    attributes.insert(
        "in_zones".into(),
        Value::Array(zones_in.into_iter().map(Value::String).collect()),
    );
    attributes.insert(
        "tracking_type".into(),
        Value::String(TRACKING_TYPE_POSITION.into()),
    );
    if let Some(source_type) = record.source_type {
        attributes.insert(
            "source_type".into(),
            Value::String(source_type.as_str().into()),
        );
    }
    if let Some([latitude, longitude]) = record.gps {
        attributes.insert("latitude".into(), json!(latitude));
        attributes.insert("longitude".into(), json!(longitude));
        attributes.insert("gps_accuracy".into(), json!(record.gps_accuracy));
    }
    if let Some(battery) = record.battery {
        attributes.insert("battery".into(), json!(battery));
    }
    attributes.insert(
        "friendly_name".into(),
        Value::String(
            record
                .config
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .or(record.host_name.as_deref().filter(|name| !name.is_empty()))
                .unwrap_or(&record.config.dev_id)
                .to_owned(),
        ),
    );
    if let Some(icon) = &record.config.icon {
        attributes.insert("icon".into(), Value::String(icon.clone()));
    }
    if let Some(picture) = &record.config.picture {
        attributes.insert("entity_picture".into(), Value::String(picture.clone()));
    }
    // The upstream payload explicitly allows caller-provided extra attributes.
    attributes.extend(record.attributes.clone());
    Ok((state, attributes))
}

impl SourceType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gps => "gps",
            Self::Router => "router",
            Self::Bluetooth => "bluetooth",
            Self::BluetoothLe => "bluetooth_le",
        }
    }
}

fn effective_consider_home(record: &PersistedDevice, default: Duration) -> Duration {
    record.config.consider_home.unwrap_or(default)
}

fn is_stale(
    record: &PersistedDevice,
    now: chrono::DateTime<chrono::Utc>,
    default: Duration,
) -> bool {
    match record.last_seen {
        None => true,
        Some(last_seen) => {
            now.signed_duration_since(last_seen) > effective_consider_home(record, default)
        }
    }
}

fn is_expiry_due(
    record: &PersistedDevice,
    now: chrono::DateTime<chrono::Utc>,
    default: Duration,
) -> bool {
    record.config.track
        && !record.expired
        && record.location_name.as_deref().is_none_or(str::is_empty)
        && (record.source_type != Some(SourceType::Gps) || record.gps.is_none())
        && record.last_seen.is_some()
        && is_stale(record, now, default)
}

fn next_expiry_delay(
    data: &PersistedTrackerData,
    now: chrono::DateTime<chrono::Utc>,
    default: Duration,
) -> std::time::Duration {
    data.devices
        .values()
        .filter(|record| {
            record.config.track
                && !record.expired
                && record.location_name.as_deref().is_none_or(str::is_empty)
                && (record.source_type != Some(SourceType::Gps) || record.gps.is_none())
        })
        .filter_map(|record| {
            let last_seen = record.last_seen?;
            let age = now.signed_duration_since(last_seen);
            let remaining = effective_consider_home(record, default)
                .checked_sub(&age)?
                .checked_add(&Duration::milliseconds(1))?;
            Some(if remaining <= Duration::zero() {
                std::time::Duration::from_millis(10)
            } else {
                remaining
                    .to_std()
                    .unwrap_or(std::time::Duration::from_millis(10))
            })
        })
        .min()
        .unwrap_or(std::time::Duration::from_secs(86_400))
}

fn initialize_tracker(
    runtime: &Runtime,
    inner: &TrackerInner,
    previous: &PersistedTrackerData,
    data: &mut PersistedTrackerData,
) -> Result<()> {
    let desired: BTreeSet<String> = data.devices.keys().cloned().collect();
    for (dev_id, previous_record) in &previous.devices {
        if desired.contains(dev_id) {
            continue;
        }
        let entity_id = entity_id_for(dev_id);
        if let Some(state) = runtime.get_state(&entity_id)
            && state_is_owned(previous_record, &state)
        {
            runtime.remove_state(&entity_id, Context::default())?;
        }
    }

    let now = chrono::Utc::now();
    let mut state_writes = Vec::new();
    for (dev_id, record) in &mut data.devices {
        let entity_id = entity_id_for(dev_id);
        let current_state = runtime.get_state(&entity_id);
        if !record.config.track {
            if current_state
                .as_ref()
                .is_some_and(|state| state_is_owned(record, state))
            {
                runtime.remove_state(&entity_id, Context::default())?;
            }
            record.last_state_context_id = None;
            record.pending_state_context_id = None;
            continue;
        }

        if let Some(state) = current_state.as_ref() {
            if !state_is_owned(record, state) {
                return Err(TrackerError::InvalidConfig(format!(
                    "a different provider already owns {entity_id}"
                )));
            }
            record.last_state_context_id = Some(state.context.id.clone());
            record.pending_state_context_id = None;
            // RestoreEntity resets last_seen when a persisted tracker entity is
            // added to Home Assistant. That gives no-GPS observations a fresh
            // consider_home window after restart.
            if record.last_seen.is_some() {
                record.last_seen = Some(now);
                record.expired = false;
            }
        }
        if is_expiry_due(record, now, inner.config.consider_home) {
            record.gps = None;
            record.expired = true;
        }
        let (state, attributes) =
            build_state(record, runtime, &inner.zones, inner.config.consider_home)?;
        if state.chars().count() > 255 {
            return Err(TrackerError::InvalidConfig(format!(
                "device state exceeds 255 characters for {entity_id}"
            )));
        }
        let context = Context::default();
        record.pending_state_context_id = Some(context.id.clone());
        state_writes.push((dev_id.clone(), entity_id, state, attributes, context));
    }

    // Write a validated restore journal before touching state rows. A crash
    // between these operations is recoverable through the pending context IDs.
    store_metadata(runtime, data)?;
    for (dev_id, entity_id, state, attributes, context) in state_writes {
        let change = runtime.set_state(&entity_id, &state, attributes, context, false)?;
        let record = data
            .devices
            .get_mut(&dev_id)
            .expect("startup state id came from tracker metadata");
        record.last_state_context_id = Some(change.state.context.id.clone());
        record.pending_state_context_id = None;
    }
    store_metadata(runtime, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::{MemoryStore, ServiceCall, Store};
    use hass_zone::{HomeZoneConfig, ZoneConfig, ZoneIntegration};

    fn zone_config(zones: Vec<ZoneConfig>) -> HomeZoneConfig {
        HomeZoneConfig {
            name: "Home".into(),
            latitude: 0.0,
            longitude: 0.0,
            zones,
        }
    }

    fn zone(name: &str, latitude: f64, longitude: f64, radius: f64, passive: bool) -> ZoneConfig {
        ZoneConfig {
            name: name.into(),
            latitude,
            longitude,
            radius,
            passive,
            icon: None,
        }
    }

    async fn start_tracker(
        config: TrackerConfig,
        zone_config: HomeZoneConfig,
    ) -> (Runtime, ZoneIntegration, DeviceTrackerIntegration) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config)
            .await
            .unwrap();
        let tracker = DeviceTrackerIntegration::start(runtime.clone(), zones.manager(), config)
            .await
            .unwrap();
        (runtime, zones, tracker)
    }

    fn call_service(
        runtime: &Runtime,
        data: Value,
        context: Context,
    ) -> hass_core::Result<ServiceResult> {
        runtime.call_service(ServiceCall {
            domain: SERVICE_DOMAIN.into(),
            service: SERVICE_SEE.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: Map::new(),
            context,
        })
    }

    fn state(runtime: &Runtime, id: &str) -> State {
        runtime
            .get_state(id)
            .unwrap_or_else(|| panic!("missing state {id}"))
    }

    fn attr_string<'a>(state: &'a State, key: &str) -> Option<&'a str> {
        state.attributes.get(key).and_then(Value::as_str)
    }

    fn tracked_config(dev_id: &str) -> TrackerDeviceConfig {
        TrackerDeviceConfig {
            dev_id: dev_id.into(),
            name: None,
            mac: None,
            track: true,
            consider_home: None,
            icon: None,
            picture: None,
        }
    }

    #[test]
    fn defaults_match_upstream() {
        let config = TrackerConfig::default();
        assert_eq!(config.consider_home, Duration::seconds(180));
        assert!(config.track_new_devices);
        assert!(config.devices.is_empty());
        let invalid_duration = TrackerConfig {
            consider_home: Duration::seconds(-1),
            ..TrackerConfig::default()
        };
        assert!(matches!(
            invalid_duration.validate(),
            Err(TrackerError::InvalidConfig(_))
        ));
        let duplicate_ids = TrackerConfig {
            devices: vec![
                tracked_config("Office Phone"),
                tracked_config("office-phone"),
            ],
            ..TrackerConfig::default()
        };
        assert!(matches!(
            duplicate_ids.validate(),
            Err(TrackerError::InvalidConfig(_))
        ));
    }

    #[test]
    fn known_devices_parser_applies_source_defaults_and_normalizes_mac() {
        let devices = parse_known_devices(json!({
            "Office Phone": {
                "name": "Phone",
                "mac": "aa:bb:cc:dd:ee:ff",
                "icon": " mdi:phone ",
                "vendor": "ignored",
                "hide_if_away": true
            }
        }))
        .unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].dev_id, "office_phone");
        assert_eq!(devices[0].name.as_deref(), Some("Phone"));
        assert_eq!(devices[0].mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(devices[0].icon.as_deref(), Some("mdi:phone"));
        assert!(!devices[0].track);
    }

    #[test]
    fn known_devices_parser_requires_name_and_rejects_unknown_fields() {
        assert!(parse_known_devices(json!({ "phone": { "track": true } })).is_err());
        assert!(
            parse_known_devices(json!({
                "phone": { "name": "Phone", "custom": "no" }
            }))
            .is_err()
        );
        assert!(
            parse_known_devices(json!({
                "phone": { "name": "Phone", "icon": "phone" }
            }))
            .is_err()
        );
    }

    #[test]
    fn known_devices_parser_resolves_gravatar_and_prefers_it_over_picture() {
        let devices = parse_known_devices(json!({
            "phone": {
                "name": "Phone",
                "gravatar": "Me@Example.com",
                "picture": "https://example.com/fallback.jpg"
            },
            "tablet": {
                "name": "Tablet",
                "gravatar": null,
                "picture": "https://example.com/tablet.jpg"
            }
        }))
        .unwrap();

        let phone = devices
            .iter()
            .find(|device| device.dev_id == "phone")
            .unwrap();
        assert_eq!(
            phone.picture.as_deref(),
            Some(
                "https://www.gravatar.com/avatar/2e0d5407ce8609047b8255c50405d7b1.jpg?s=80&d=wavatar"
            )
        );
        let tablet = devices
            .iter()
            .find(|device| device.dev_id == "tablet")
            .unwrap();
        assert_eq!(
            tablet.picture.as_deref(),
            Some("https://example.com/tablet.jpg")
        );
    }

    #[test]
    fn known_devices_parser_accepts_upstream_time_period_forms() {
        let devices = parse_known_devices(json!({
            "phone": {
                "name": "Phone",
                "consider_home": { "minutes": 2, "seconds": 3.5 }
            }
        }))
        .unwrap();
        assert_eq!(
            devices[0].consider_home,
            Some(Duration::milliseconds(123_500))
        );
        assert_eq!(
            parse_time_period(&json!("2")).unwrap(),
            Duration::seconds(2)
        );
    }

    #[tokio::test]
    async fn service_requires_mac_or_device_id_and_rejects_unknown_fields() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        assert!(call_service(&runtime, json!({ "battery": 20 }), Context::default()).is_err());
        assert!(
            call_service(
                &runtime,
                json!({ "dev_id": "phone", "arbitrary_setter": true }),
                Context::default()
            )
            .is_err()
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn service_accepts_ios_compatibility_fields_and_discards_them() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        let mut events = runtime.subscribe();
        call_service(
            &runtime,
            json!({
                "dev_id": "phone",
                "hostname": "ignored.example",
                "battery_status": "Not Charging"
            }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, STATE_HOME);
        assert_eq!(current.attributes.get("hostname"), None);
        assert_eq!(current.attributes.get("battery_status"), None);
        let mut new_device_event = None;
        while let Ok(event) = events.try_recv() {
            if event.event_type == EVENT_NEW_DEVICE {
                new_device_event = Some(event);
            }
        }
        let new_device_event = new_device_event.expect("new devices emit the upstream event");
        assert_eq!(
            Value::Object(new_device_event.data.clone()),
            json!({
                "entity_id": "device_tracker.phone",
                "host_name": null,
                "mac": null
            })
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn service_preserves_causal_context() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        let context = Context {
            id: "caller-context".into(),
            parent_id: Some("parent-context".into()),
            user_id: Some("user-7".into()),
        };
        let result = call_service(&runtime, json!({ "dev_id": "phone" }), context.clone()).unwrap();
        assert_eq!(result.context, context);
        assert_eq!(state(&runtime, "device_tracker.phone").context, context);
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gps_home_report_contains_position_capabilities_and_real_zone_membership() {
        let (runtime, zones, mut tracker) = start_tracker(
            TrackerConfig::default(),
            zone_config(vec![zone("Wide", 0.0, 0.0, 2_000.0, false)]),
        )
        .await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "gps": [0.0, 0.0], "gps_accuracy": 0 }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, STATE_HOME);
        assert_eq!(attr_string(&current, "source_type"), Some("gps"));
        assert_eq!(attr_string(&current, "tracking_type"), Some("position"));
        assert_eq!(
            current.attributes.get("in_zones").unwrap(),
            &json!(["zone.home", "zone.wide"])
        );
        assert_eq!(current.attributes.get("latitude").unwrap(), &json!(0.0));
        assert_eq!(current.attributes.get("longitude").unwrap(), &json!(0.0));
        assert_eq!(current.attributes.get("gps_accuracy").unwrap(), &json!(0));
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gps_state_uses_the_smallest_active_overlapping_zone() {
        let (runtime, zones, mut tracker) = start_tracker(
            TrackerConfig::default(),
            zone_config(vec![
                zone("Big", 0.0, 0.0, 2_000.0, false),
                zone("Small", 0.0, 0.0, 1_000.0, false),
            ]),
        )
        .await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "gps": [0.0, 0.005] }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, "Small", "{current:?}");
        assert_eq!(
            current.attributes.get("in_zones").unwrap(),
            &json!(["zone.small", "zone.big"])
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn passive_zones_appear_in_membership_but_do_not_set_state() {
        let (runtime, zones, mut tracker) = start_tracker(
            TrackerConfig::default(),
            zone_config(vec![
                zone("Passive", 0.0, 0.0, 500.0, true),
                zone("Active", 0.0, 0.0, 2_000.0, false),
            ]),
        )
        .await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "gps": [0.0, 0.002] }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, "Active", "{current:?}");
        assert_eq!(
            current.attributes.get("in_zones").unwrap(),
            &json!(["zone.passive", "zone.active"])
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gps_accuracy_expands_zone_membership_radius() {
        let (runtime, zones, mut tracker) = start_tracker(
            TrackerConfig::default(),
            zone_config(vec![zone("Nearby", 0.0, 0.02, 1_000.0, false)]),
        )
        .await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "gps": [0.0, 0.01], "gps_accuracy": 150 }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, "Nearby", "{current:?}");
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_coordinates_and_negative_numeric_fields_are_rejected() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        assert!(
            call_service(
                &runtime,
                json!({ "dev_id": "phone", "gps": [91.0, 0.0] }),
                Context::default()
            )
            .is_err()
        );
        assert!(
            call_service(
                &runtime,
                json!({ "dev_id": "phone", "gps": [0.0] }),
                Context::default()
            )
            .is_err()
        );
        assert!(
            call_service(
                &runtime,
                json!({ "dev_id": "phone", "battery": -1 }),
                Context::default()
            )
            .is_err()
        );
        call_service(
            &runtime,
            json!({ "dev_id": "coerced", "gps": ["0", "0"], "gps_accuracy": "0", "battery": "0" }),
            Context::default(),
        )
        .unwrap();
        assert_eq!(state(&runtime, "device_tracker.coerced").state, STATE_HOME);
        assert!(runtime.get_state("device_tracker.phone").is_none());
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn router_report_is_home_and_uses_connection_source_metadata() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "source_type": "router" }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, STATE_HOME);
        assert_eq!(attr_string(&current, "source_type"), Some("router"));
        assert_eq!(attr_string(&current, "tracking_type"), Some("position"));
        assert_eq!(current.attributes.get("in_zones").unwrap(), &json!([]));
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "source_type": "router", "consider_home": 0 }),
            Context::default(),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(state(&runtime, "device_tracker.phone").state, STATE_HOME);
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn location_name_takes_precedence_over_gps_zone_state() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "gps": [0.0, 0.0], "location_name": "Work" }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, "Work");
        assert_eq!(
            current.attributes.get("in_zones").unwrap(),
            &json!(["zone.home"])
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn custom_attributes_merge_and_battery_is_retained_when_omitted() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "battery": 30, "attributes": { "room": "office" } }),
            Context::default(),
        )
        .unwrap();
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "attributes": { "charging": true } }),
            Context::default(),
        )
        .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.attributes.get("battery").unwrap(), &json!(30));
        assert_eq!(current.attributes.get("room").unwrap(), &json!("office"));
        assert_eq!(current.attributes.get("charging").unwrap(), &json!(true));
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn track_new_disabled_keeps_metadata_without_publishing_state() {
        let (runtime, zones, mut tracker) = start_tracker(
            TrackerConfig {
                track_new_devices: false,
                ..TrackerConfig::default()
            },
            zone_config(Vec::new()),
        )
        .await;
        call_service(
            &runtime,
            json!({ "dev_id": "untracked" }),
            Context::default(),
        )
        .unwrap();
        assert!(runtime.get_state("device_tracker.untracked").is_none());
        assert_eq!(tracker.manager().device_ids(), vec!["untracked"]);
        assert!(runtime.get_data(METADATA_KEY).unwrap().is_some());
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mac_resolves_the_configured_device_even_with_another_dev_id() {
        let mut config = TrackerConfig::default();
        config.devices.push(TrackerDeviceConfig {
            dev_id: "office".into(),
            name: Some("Office device".into()),
            mac: Some("AA:BB:CC:DD:EE:FF".into()),
            track: true,
            consider_home: None,
            icon: None,
            picture: None,
        });
        let (runtime, zones, mut tracker) = start_tracker(config, zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({ "mac": "aa:bb:cc:dd:ee:ff", "dev_id": "ignored" }),
            Context::default(),
        )
        .unwrap();
        assert!(runtime.get_state("device_tracker.office").is_some());
        assert!(runtime.get_state("device_tracker.ignored").is_none());
        assert_eq!(
            attr_string(&state(&runtime, "device_tracker.office"), "friendly_name"),
            Some("Office device")
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_mac_uses_host_name_and_reuses_its_mac_mapping() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({ "mac": "aa:bb", "host_name": "Kitchen phone" }),
            Context::default(),
        )
        .unwrap();
        call_service(
            &runtime,
            json!({ "mac": "AA:BB", "host_name": "Other label" }),
            Context::default(),
        )
        .unwrap();
        assert_eq!(tracker.manager().device_ids(), vec!["kitchen_phone"]);
        assert_eq!(
            attr_string(
                &state(&runtime, "device_tracker.kitchen_phone"),
                "friendly_name"
            ),
            Some("Other label")
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn non_gps_report_expires_and_clears_old_coordinates() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        call_service(
            &runtime,
            json!({
                "dev_id": "phone",
                "source_type": "router",
                "gps": [0.0, 0.0],
                "consider_home": 0.03
            }),
            Context::default(),
        )
        .unwrap();
        assert_eq!(state(&runtime, "device_tracker.phone").state, STATE_HOME);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, STATE_NOT_HOME);
        assert!(current.attributes.get("latitude").is_none());
        assert_eq!(current.attributes.get("in_zones").unwrap(), &json!([]));
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_private_metadata_fails_before_service_or_state_mutation() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime
            .set_data(
                METADATA_KEY,
                &json!({ "schema_version": 99, "devices": {} }),
            )
            .unwrap();
        runtime
            .set_state(
                "device_tracker.foreign",
                "home",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let result = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig::default(),
        )
        .await;
        assert!(matches!(result, Err(TrackerError::InvalidMetadata(_))));
        assert_eq!(state(&runtime, "device_tracker.foreign").state, "home");
        assert!(call_service(&runtime, json!({ "dev_id": "phone" }), Context::default()).is_err());
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_removes_stale_owned_state_but_preserves_foreign_state() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig {
                devices: vec![tracked_config("old")],
                ..TrackerConfig::default()
            },
        )
        .await
        .unwrap();
        assert!(runtime.get_state("device_tracker.old").is_some());
        runtime
            .set_state(
                "device_tracker.foreign",
                "home",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
        drop(tracker);
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig::default(),
        )
        .await
        .unwrap();
        assert!(runtime.get_state("device_tracker.old").is_none());
        assert_eq!(state(&runtime, "device_tracker.foreign").state, "home");
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_preserves_state_overwritten_by_another_provider() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig {
                devices: vec![tracked_config("reused")],
                ..TrackerConfig::default()
            },
        )
        .await
        .unwrap();
        tracker.shutdown().await.unwrap();
        runtime
            .set_state(
                "device_tracker.reused",
                "foreign-provider",
                Map::new(),
                Context::default(),
                false,
            )
            .unwrap();
        zones.shutdown().await.unwrap();
        drop(tracker);
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            state(&runtime, "device_tracker.reused").state,
            "foreign-provider"
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tracker_state_and_private_metadata_restore_on_a_new_runtime() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        let config = TrackerConfig {
            consider_home: Duration::milliseconds(30),
            ..TrackerConfig::default()
        };
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker =
            DeviceTrackerIntegration::start(runtime.clone(), zones.manager(), config.clone())
                .await
                .unwrap();
        call_service(
            &runtime,
            json!({ "dev_id": "phone", "source_type": "router", "battery": 55 }),
            Context::default(),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(
            state(&runtime, "device_tracker.phone").state,
            STATE_NOT_HOME
        );
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
        drop(tracker);
        drop(runtime);

        let runtime = Runtime::new(store).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut tracker = DeviceTrackerIntegration::start(runtime.clone(), zones.manager(), config)
            .await
            .unwrap();
        let current = state(&runtime, "device_tracker.phone");
        assert_eq!(current.state, STATE_HOME);
        assert_eq!(current.attributes.get("battery").unwrap(), &json!(55));
        assert_eq!(attr_string(&current, "source_type"), Some("router"));
        assert_eq!(tracker.manager().device_ids(), vec!["phone"]);
        tracker.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn second_instance_cannot_claim_service_until_first_releases_it() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        let zones = ZoneIntegration::start(runtime.clone(), zone_config(Vec::new()))
            .await
            .unwrap();
        let mut first = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig::default(),
        )
        .await
        .unwrap();
        assert!(
            DeviceTrackerIntegration::start(
                runtime.clone(),
                zones.manager(),
                TrackerConfig::default(),
            )
            .await
            .is_err()
        );
        first.shutdown().await.unwrap();
        let mut replacement = DeviceTrackerIntegration::start(
            runtime.clone(),
            zones.manager(),
            TrackerConfig::default(),
        )
        .await
        .unwrap();
        replacement.shutdown().await.unwrap();
        zones.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_unregisters_service_and_closes_typed_manager() {
        let (runtime, zones, mut tracker) =
            start_tracker(TrackerConfig::default(), zone_config(Vec::new())).await;
        let manager = tracker.manager();
        tracker.shutdown().await.unwrap();
        assert!(call_service(&runtime, json!({ "dev_id": "phone" }), Context::default()).is_err());
        assert!(matches!(
            manager.see(
                TrackerUpdate {
                    dev_id: Some("phone".into()),
                    ..TrackerUpdate::default()
                },
                Context::default()
            ),
            Err(TrackerError::NotRunning)
        ));
        zones.shutdown().await.unwrap();
    }
}
