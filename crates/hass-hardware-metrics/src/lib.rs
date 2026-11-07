//! Native CPU and physical-memory metrics for the hardware integration.
//!
//! Construction performs no operating-system reads. The first call to
//! [`MetricsProvider::read`] samples CPU counters twice, matching psutil's
//! first non-blocking sample when its thread-local baseline is absent.

mod cpu;
#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos_metrics;
mod memory;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(test)]
mod tests;

use std::sync::Mutex;

use cpu::{CpuCounters, sample_cpu_percent};
use hass_hardware::{MetricsProvider, RawSystemMetrics};
use memory::MemorySample;

/// Native system metrics provider for Linux and macOS.
#[derive(Debug)]
pub struct NativeMetricsProvider {
    cpu_baseline: Mutex<Option<CpuCounters>>,
}

impl NativeMetricsProvider {
    /// Create a provider without reading CPU, memory, or other OS state.
    pub fn new() -> Self {
        Self {
            cpu_baseline: Mutex::new(None),
        }
    }

    fn read_cpu_percent(&self) -> hass_core::Result<f64> {
        let mut baseline = self
            .cpu_baseline
            .lock()
            .map_err(|_| core_error("CPU counter baseline mutex is poisoned"))?;
        sample_cpu_percent(&mut baseline, read_cpu_counters)
    }
}

impl Default for NativeMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsProvider for NativeMetricsProvider {
    fn read(&self) -> hass_core::Result<RawSystemMetrics> {
        // psutil updates its CPU baseline before the independent memory read.
        let cpu_percent = self.read_cpu_percent()?;
        let memory = read_memory_sample()?;

        Ok(RawSystemMetrics {
            cpu_percent,
            total_bytes: memory.total_bytes,
            available_bytes: memory.available_bytes,
            memory_used_percent: memory.used_percent,
        })
    }
}

#[cfg(target_os = "linux")]
fn read_cpu_counters() -> hass_core::Result<CpuCounters> {
    linux::read_cpu_counters()
}

#[cfg(target_os = "macos")]
fn read_cpu_counters() -> hass_core::Result<CpuCounters> {
    macos::read_cpu_counters()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_cpu_counters() -> hass_core::Result<CpuCounters> {
    Err(core_error(format!(
        "CPU metrics are unsupported on target OS `{}`",
        std::env::consts::OS
    )))
}

#[cfg(target_os = "linux")]
fn read_memory_sample() -> hass_core::Result<MemorySample> {
    linux::read_memory_sample()
}

#[cfg(target_os = "macos")]
fn read_memory_sample() -> hass_core::Result<MemorySample> {
    macos::read_memory_sample()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_memory_sample() -> hass_core::Result<MemorySample> {
    Err(core_error(format!(
        "memory metrics are unsupported on target OS `{}`",
        std::env::consts::OS
    )))
}

fn round_one_decimal(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    // Round the exact binary value to decimal tenths. Multiplying in f64
    // first can manufacture a tie: Python round(3.45, 1) is 3.5, whereas
    // (3.45_f64 * 10.0).round_ties_even() / 10.0 is 3.4.
    let bits = value.to_bits();
    let encoded_exponent = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = u128::from(bits & ((1_u64 << 52) - 1))
        | if encoded_exponent == 0 {
            0
        } else {
            1_u128 << 52
        };
    let exponent = if encoded_exponent == 0 {
        -1074
    } else {
        encoded_exponent - 1023 - 52
    };
    if exponent >= 0 {
        return value; // Already an integer at this binary precision.
    }
    let shift = (-exponent) as u32;
    if shift >= 128 {
        return 0.0_f64.copysign(value);
    }
    let scaled = mantissa * 10;
    let mut tenths = scaled >> shift;
    let remainder = scaled & ((1_u128 << shift) - 1);
    let half = 1_u128 << (shift - 1);
    if remainder > half || (remainder == half && tenths & 1 != 0) {
        tenths += 1;
    }
    (tenths as f64 / 10.0).copysign(value)
}

fn core_error(message: impl std::fmt::Display) -> hass_core::Error {
    hass_core::Error::Service(format!("native hardware metrics: {message}"))
}
