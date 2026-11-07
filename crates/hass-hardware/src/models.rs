//! Typed hardware records and system-status values.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Board metadata reported by a loaded hardware provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BoardInfo {
    /// Board identifier supplied by Home Assistant Supervisor, when available.
    pub hassio_board_id: Option<String>,
    /// Board manufacturer.
    pub manufacturer: String,
    /// Human-readable board model, when available.
    pub model: Option<String>,
    /// Board revision, when available.
    pub revision: Option<String>,
}

/// USB metadata stored by a configured hardware integration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct USBInfo {
    /// Vendor ID as represented by the integration configuration.
    pub vid: String,
    /// Product ID as represented by the integration configuration.
    pub pid: String,
    /// USB serial number, when available.
    pub serial_number: Option<String>,
    /// USB manufacturer string, when available.
    pub manufacturer: Option<String>,
    /// USB product description, when available.
    pub description: Option<String>,
}

/// A board or device record supplied by one loaded hardware provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareInfo {
    /// Display name, when supplied by the provider.
    pub name: Option<String>,
    /// Board metadata, when this record describes a board.
    pub board: Option<BoardInfo>,
    /// Config-entry identifiers associated with this record, when known.
    pub config_entries: Option<Vec<String>>,
    /// Configured USB dongle metadata, when this record describes one.
    pub dongle: Option<USBInfo>,
    /// Documentation URL, when supplied by the provider.
    pub url: Option<String>,
}

/// Raw values returned by an injected operating-system metrics provider.
///
/// The memory percentage is the provider's own value. It is not inferred from
/// the byte counts, since Home Assistant's source uses psutil's `percent` field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawSystemMetrics {
    /// CPU utilization percentage before integer rounding.
    pub cpu_percent: f64,
    /// Total physical memory in bytes.
    pub total_bytes: u64,
    /// Available physical memory in bytes.
    pub available_bytes: u64,
    /// Memory utilization percentage from the provider.
    pub memory_used_percent: f64,
}

/// One sampled system-status event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SystemStatus {
    /// CPU utilization rounded to an integer using ties-to-even.
    pub cpu_percent: i64,
    /// Memory utilization percentage supplied by the metrics provider.
    pub memory_used_percent: f64,
    /// Used memory in MiB, rounded to one decimal place.
    pub memory_used_mb: f64,
    /// Available memory in MiB, rounded to one decimal place.
    pub memory_free_mb: f64,
    /// UTC timestamp in ISO-8601 form with an explicit `+00:00` offset.
    pub timestamp: String,
}

impl SystemStatus {
    /// Convert raw values at the current UTC time.
    pub fn from_metrics(metrics: RawSystemMetrics) -> hass_core::Result<Self> {
        Self::from_metrics_at(metrics, Utc::now())
    }

    /// Convert raw values using a supplied timestamp, useful for deterministic
    /// integrations and tests.
    pub fn from_metrics_at(
        metrics: RawSystemMetrics,
        timestamp: DateTime<Utc>,
    ) -> hass_core::Result<Self> {
        validate_metrics(metrics)?;

        let cpu_percent = metrics.cpu_percent.round_ties_even() as i64;
        let mib = 1024.0 * 1024.0;
        let memory_used_mb =
            round_one_decimal((metrics.total_bytes - metrics.available_bytes) as f64 / mib);
        let memory_free_mb = round_one_decimal(metrics.available_bytes as f64 / mib);

        Ok(Self {
            cpu_percent,
            memory_used_percent: metrics.memory_used_percent,
            memory_used_mb,
            memory_free_mb,
            timestamp: iso_timestamp(timestamp),
        })
    }
}

fn iso_timestamp(timestamp: DateTime<Utc>) -> String {
    // Python datetime.isoformat uses six digits whenever microseconds are
    // present, and has no nanosecond precision.
    let mut value = timestamp.format("%Y-%m-%dT%H:%M:%S").to_string();
    let micros = timestamp.timestamp_subsec_micros();
    if micros != 0 {
        value.push_str(&format!(".{micros:06}"));
    }
    value.push_str("+00:00");
    value
}

fn validate_metrics(metrics: RawSystemMetrics) -> hass_core::Result<()> {
    if metrics.total_bytes == 0 {
        return Err(hass_core::Error::InvalidInput(
            "total memory must be greater than zero".into(),
        ));
    }
    if !metrics.cpu_percent.is_finite() || !metrics.memory_used_percent.is_finite() {
        return Err(hass_core::Error::InvalidInput(
            "system metrics percentages must be finite".to_owned(),
        ));
    }
    if metrics.available_bytes > metrics.total_bytes {
        return Err(hass_core::Error::InvalidInput(
            "available memory cannot exceed total memory".to_owned(),
        ));
    }
    Ok(())
}

fn round_one_decimal(value: f64) -> f64 {
    (value * 10.0).round_ties_even() / 10.0
}
